use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crossbeam_channel::Sender;
use serde_json::Value;
use url::Url;
use uuid::Uuid;

use crate::db;
use crate::thumbs;

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124 Safari/537.36";

pub struct Metadata {
    pub title: String,
    pub channel: Option<String>,
    pub duration_s: Option<i64>,
    pub thumb_url: Option<String>,
    pub thumb_width: Option<u32>,
}

pub fn fetch_metadata(program: &str, url: &str) -> Result<Metadata> {
    let output = Command::new(program)
        .args([
            "-J",
            "--no-warnings",
            "--no-playlist",
            "--skip-download",
            url,
        ])
        .output()
        .with_context(|| format!("no se pudo ejecutar {program}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{}", stderr.trim());
    }

    let value: Value =
        serde_json::from_slice(&output.stdout).context("respuesta de yt-dlp inválida")?;

    if value.get("_type").and_then(Value::as_str) == Some("playlist")
        || value.get("entries").is_some()
    {
        bail!("las listas de reproducción no están soportadas");
    }

    let title = value
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(url)
        .to_string();

    let channel = value
        .get("channel")
        .or_else(|| value.get("uploader"))
        .or_else(|| value.get("creator"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let duration_s = value
        .get("duration")
        .and_then(Value::as_f64)
        .map(|duration| duration.round() as i64);

    let (thumb_url, thumb_width) = pick_thumbnail(&value, 320);

    Ok(Metadata {
        title,
        channel,
        duration_s,
        thumb_url,
        thumb_width,
    })
}

fn pick_thumbnail(value: &Value, target_width: u32) -> (Option<String>, Option<u32>) {
    let mut candidates: Vec<(u32, String)> = value
        .get("thumbnails")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|thumbnail| {
            let url = thumbnail.get("url")?.as_str()?.to_string();
            let width = thumbnail.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
            Some((width, url))
        })
        .collect();

    candidates.sort_by_key(|(width, _)| *width);
    let known: Vec<&(u32, String)> = candidates.iter().filter(|(w, _)| *w > 0).collect();

    if let Some((width, url)) = known
        .iter()
        .find(|(width, _)| *width >= target_width)
        .or_else(|| known.first())
    {
        return (Some(url.clone()), Some(*width));
    }

    if let Some((_, url)) = candidates.first() {
        return (Some(url.clone()), None);
    }

    (
        value
            .get("thumbnail")
            .and_then(Value::as_str)
            .map(str::to_string),
        None,
    )
}

fn fetch_bytes(url: &str) -> Result<Vec<u8>> {
    let mut response = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .context("no se pudo descargar el thumbnail")?;
    let bytes = response
        .body_mut()
        .read_to_vec()
        .context("thumbnail vacío")?;

    if bytes.is_empty() {
        bail!("thumbnail vacío");
    }

    Ok(bytes)
}

pub fn normalize_url(raw: &str) -> String {
    let trimmed = raw.trim();
    let Ok(mut url) = Url::parse(trimmed) else {
        return trimmed.to_string();
    };

    url.set_fragment(None);

    if let Some(host) = url.host_str().map(|host| host.to_ascii_lowercase()) {
        if host == "youtu.be" {
            let id = url
                .path()
                .trim_start_matches('/')
                .trim_end_matches('/')
                .to_string();
            if !id.is_empty()
                && let Ok(canonical) = Url::parse(&format!("https://www.youtube.com/watch?v={id}"))
            {
                url = canonical;
            }
        } else if host == "youtube.com" || host.ends_with(".youtube.com") {
            let video_id = url
                .query_pairs()
                .find(|(key, _)| key == "v")
                .map(|(_, value)| value.to_string())
                .or_else(|| url.path().strip_prefix("/shorts/").map(str::to_string))
                .or_else(|| url.path().strip_prefix("/live/").map(str::to_string));

            if let Some(video_id) = video_id
                && !video_id.is_empty()
                && let Ok(canonical) =
                    Url::parse(&format!("https://www.youtube.com/watch?v={video_id}"))
            {
                url = canonical;
            }
        }
    }

    if url.path().len() > 1 {
        let path = url.path().trim_end_matches('/').to_string();
        url.set_path(&path);
    }

    let mut query: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| !is_tracking_param(key))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    query.sort();

    url.set_query(None);
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in &query {
            pairs.append_pair(key, value);
        }
    }

    url.to_string()
}

fn is_tracking_param(key: &str) -> bool {
    key.starts_with("utm_")
        || matches!(key, "si" | "feature" | "fbclid" | "gclid" | "igshid" | "pp")
}

pub enum ImportEvent {
    Imported(String),
    Skipped(String),
    Failed(String),
    Finished {
        imported: usize,
        skipped: usize,
        failed: usize,
    },
}

pub struct ImportOptions {
    pub program: String,
    pub thumb_width: u32,
}

pub fn spawn_import(
    urls: Vec<String>,
    options: ImportOptions,
    db_path: PathBuf,
    thumbs_dir: PathBuf,
    tx: Sender<ImportEvent>,
) {
    thread::spawn(move || {
        let (imported, skipped, failed) =
            match run_import(&urls, &options, &db_path, &thumbs_dir, &tx) {
                Ok(counts) => counts,
                Err(error) => {
                    let _ = tx.send(ImportEvent::Failed(error.to_string()));
                    (0, 0, urls.len())
                }
            };

        let _ = tx.send(ImportEvent::Finished {
            imported,
            skipped,
            failed,
        });
    });
}

fn run_import(
    urls: &[String],
    options: &ImportOptions,
    db_path: &Path,
    thumbs_dir: &Path,
    tx: &Sender<ImportEvent>,
) -> Result<(usize, usize, usize)> {
    let conn = db::open_connection(db_path)?;
    let mut imported = 0;
    let mut skipped = 0;
    let mut failed = 0;

    for raw in urls {
        let url = raw.trim();
        if url.is_empty() {
            continue;
        }

        let normalized = normalize_url(url);
        if normalized.is_empty() {
            continue;
        }

        if db::url_exists(&conn, &normalized)? {
            skipped += 1;
            let _ = tx.send(ImportEvent::Skipped(url.to_string()));
            continue;
        }

        let metadata = match fetch_metadata(&options.program, url) {
            Ok(metadata) => metadata,
            Err(error) => {
                failed += 1;
                let _ = tx.send(ImportEvent::Failed(format!("{url}: {error}")));
                continue;
            }
        };

        let id = Uuid::now_v7().to_string();
        let thumb_key = metadata
            .thumb_url
            .as_deref()
            .and_then(|thumb_url| fetch_bytes(thumb_url).ok())
            .and_then(|bytes| {
                thumbs::store_remote_thumb(
                    thumbs_dir,
                    &id,
                    &bytes,
                    metadata.thumb_width,
                    options.thumb_width,
                )
                .ok()
            });

        let inserted = db::insert_url_video(
            &conn,
            &id,
            url,
            &normalized,
            &metadata.title,
            metadata.channel.as_deref(),
            metadata.duration_s,
            thumb_key.as_deref(),
        )?;

        if inserted {
            imported += 1;
            let _ = tx.send(ImportEvent::Imported(metadata.title));
        } else {
            skipped += 1;
            let _ = tx.send(ImportEvent::Skipped(url.to_string()));
        }
    }

    Ok((imported, skipped, failed))
}

#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub percent: Option<f64>,
    pub speed: Option<String>,
    pub raw: String,
}

pub enum DownloadEvent {
    Progress(DownloadProgress),
    Finished,
    Failed(String),
}

fn parse_download_progress(line: &str) -> DownloadProgress {
    let raw = line.trim().to_string();
    let mut parts = raw.split_whitespace();
    let percent = parts
        .next()
        .and_then(|part| part.strip_suffix('%'))
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite());
    let speed = if percent.is_some() {
        parts.next().map(str::to_string)
    } else {
        None
    };

    DownloadProgress {
        percent,
        speed,
        raw,
    }
}

pub struct DownloadOptions {
    pub program: String,
    pub format: String,
    pub root: PathBuf,
    pub subdir: PathBuf,
}

pub fn spawn_download(
    video_id: String,
    url: String,
    options: DownloadOptions,
    db_path: PathBuf,
    tx: Sender<DownloadEvent>,
) {
    thread::spawn(move || {
        let result = run_download(&video_id, &url, &options, &db_path, &tx);
        match result {
            Ok(()) => {
                let _ = tx.send(DownloadEvent::Finished);
            }
            Err(error) => {
                let _ = tx.send(DownloadEvent::Failed(error.to_string()));
            }
        }
    });
}

fn run_download(
    video_id: &str,
    url: &str,
    options: &DownloadOptions,
    db_path: &Path,
    tx: &Sender<DownloadEvent>,
) -> Result<()> {
    let base = resolve_download_base(&options.root, &options.subdir)?;
    if !base.is_dir() {
        bail!("la carpeta de destino no existe: {}", base.display());
    }

    let base_path = base.to_string_lossy().to_string();

    let mut child = Command::new(&options.program)
        .args([
            "--no-warnings",
            "--no-playlist",
            "-c",
            "--newline",
            "--progress-template",
            "download:%(progress._percent_str)s %(progress._speed_str)s",
            "--print",
            "after_move:filepath",
            "-f",
            &options.format,
            "-P",
            &base_path,
            "-o",
            "%(title).180B [%(id)s].%(ext)s",
            url,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("no se pudo ejecutar {}", options.program))?;

    let stderr_thread = child.stderr.take().map(|mut stderr| {
        thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        })
    });

    let stdout = child.stdout.take().context("sin salida de yt-dlp")?;
    let mut final_path: Option<PathBuf> = None;

    for line in BufReader::new(stdout).lines().map_while(|line| line.ok()) {
        if let Some(progress) = line.strip_prefix("download:") {
            let _ = tx.send(DownloadEvent::Progress(parse_download_progress(progress)));
        } else if !line.trim().is_empty() && Path::new(line.trim()).is_file() {
            final_path = Some(PathBuf::from(line.trim()));
        }
    }

    let status = child.wait()?;
    let stderr = stderr_thread
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();

    if !status.success() {
        let message = stderr.trim();
        if message.is_empty() {
            bail!("yt-dlp terminó con error");
        }
        bail!("{message}");
    }

    let path = final_path.context("yt-dlp no devolvió la ruta del archivo")?;
    let metadata = std::fs::metadata(&path)?;
    let relative = path
        .strip_prefix(&options.root)
        .context("el archivo descargado quedó fuera de la carpeta de vídeos")?;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_else(now_seconds);

    let conn = db::open_connection(db_path)?;
    db::set_downloaded(
        &conn,
        video_id,
        &relative.to_string_lossy(),
        &path.to_string_lossy(),
        metadata.len() as i64,
        mtime,
    )?;

    Ok(())
}

fn resolve_download_base(root: &Path, subdir: &Path) -> Result<PathBuf> {
    let mut base = root.to_path_buf();
    for component in subdir.components() {
        match component {
            Component::Normal(part) => base.push(part),
            Component::CurDir => {}
            _ => bail!("la carpeta de descarga debe ser relativa a Vídeos"),
        }
    }
    Ok(base)
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{
        DownloadEvent, ImportEvent, ImportOptions, fetch_bytes, fetch_metadata, normalize_url,
        resolve_download_base, spawn_download, spawn_import,
    };
    use crate::db::{self, Db};
    use crossbeam_channel::unbounded;
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    #[test]
    fn parses_download_progress() {
        let progress = super::parse_download_progress("  42.5% 1.2MiB/s ");
        assert_eq!(progress.percent, Some(42.5));
        assert_eq!(progress.speed.as_deref(), Some("1.2MiB/s"));

        let unknown = super::parse_download_progress("N/A");
        assert_eq!(unknown.percent, None);
    }

    #[test]
    fn resolves_download_base() {
        let root = Path::new("/tmp/videos");
        assert_eq!(
            resolve_download_base(root, Path::new("")).unwrap(),
            PathBuf::from("/tmp/videos")
        );
        assert_eq!(
            resolve_download_base(root, Path::new("Pelis/Accion")).unwrap(),
            PathBuf::from("/tmp/videos/Pelis/Accion")
        );
        assert!(resolve_download_base(root, Path::new("/abs")).is_err());
    }

    #[test]
    fn normalizes_youtube_urls() {
        assert_eq!(
            normalize_url("https://youtu.be/abc123?si=xyz&t=30"),
            "https://www.youtube.com/watch?v=abc123"
        );
        assert_eq!(
            normalize_url("https://www.youtube.com/watch?v=abc123&utm_source=test"),
            "https://www.youtube.com/watch?v=abc123"
        );
    }

    #[test]
    fn strips_tracking_parameters() {
        assert_eq!(
            normalize_url("https://example.com/video?utm_source=x&id=7&fbclid=y"),
            "https://example.com/video?id=7"
        );
    }

    #[test]
    #[ignore]
    fn fetches_real_metadata_and_thumbnail() {
        let Ok(url) = std::env::var("VP_TEST_URL") else {
            return;
        };

        let metadata = fetch_metadata("yt-dlp", &url).expect("metadata");
        assert!(!metadata.title.is_empty());
        assert!(metadata.duration_s.unwrap_or(0) > 0);

        let thumb_url = metadata.thumb_url.expect("sin thumbnail");
        let bytes = fetch_bytes(&thumb_url).expect("thumbnail");
        assert!(!bytes.is_empty());
    }

    #[test]
    #[ignore]
    fn imports_url_end_to_end() {
        let Ok(url) = std::env::var("VP_TEST_URL") else {
            return;
        };

        let base = std::env::temp_dir().join(format!("video-player-import-{}", Uuid::now_v7()));
        let db_path = base.join("library.db");
        let thumbs_dir = base.join("thumbs");
        let db = Db::open(&db_path).unwrap();
        let (tx, rx) = unbounded();

        spawn_import(
            vec![url],
            ImportOptions {
                program: "yt-dlp".to_string(),
                thumb_width: 320,
            },
            db_path,
            thumbs_dir.clone(),
            tx,
        );

        let mut finished = None;
        for _ in 0..120 {
            match rx.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(ImportEvent::Finished {
                    imported,
                    skipped,
                    failed,
                }) => {
                    finished = Some((imported, skipped, failed));
                    break;
                }
                Ok(_) => {}
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }

        assert_eq!(finished, Some((1, 0, 0)));

        let videos = db.list_videos().unwrap();
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0].status, "link");
        let thumb = videos[0].thumb_key.as_ref().expect("sin thumbnail");
        assert!(thumbs_dir.join(thumb).is_file());

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    #[ignore]
    fn downloads_url_to_library() {
        if Command::new("python3").arg("--version").output().is_err()
            || Command::new("ffmpeg").arg("-version").output().is_err()
        {
            return;
        }

        let base = std::env::temp_dir().join(format!("video-player-download-{}", Uuid::now_v7()));
        let root = base.join("Videos");
        let serve = base.join("serve");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join("Pelis/Accion")).unwrap();
        std::fs::create_dir_all(&serve).unwrap();

        let sample = serve.join("sample.mp4");
        let rendered = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=320x240:rate=25",
                "-pix_fmt",
                "yuv420p",
                &sample.to_string_lossy(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        if !rendered.success() {
            return;
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut server = Command::new("python3")
            .args([
                "-m",
                "http.server",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
                "--directory",
                &serve.to_string_lossy(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            if Instant::now() > deadline {
                let _ = server.kill();
                let _ = server.wait();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let db_path = base.join("library.db");
        let conn = db::open_connection(&db_path).unwrap();
        let id = Uuid::now_v7().to_string();
        let url = format!("http://127.0.0.1:{port}/sample.mp4");
        db::insert_url_video(&conn, &id, &url, &url, "Sample", None, None, None).unwrap();
        drop(conn);

        let (tx, rx) = unbounded();
        spawn_download(
            id.clone(),
            url,
            super::DownloadOptions {
                program: "yt-dlp".to_string(),
                format: "b".to_string(),
                root: root.clone(),
                subdir: PathBuf::from("Pelis/Accion"),
            },
            db_path.clone(),
            tx,
        );

        let mut finished = false;
        let mut error = None;
        for _ in 0..60 {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(DownloadEvent::Finished) => {
                    finished = true;
                    break;
                }
                Ok(DownloadEvent::Failed(message)) => {
                    error = Some(message);
                    break;
                }
                Ok(DownloadEvent::Progress(_)) => {}
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }

        let _ = server.kill();
        let _ = server.wait();

        assert!(finished, "descarga fallida: {error:?}");

        let db = Db::open(&db_path).unwrap();
        let videos = db.list_videos().unwrap();
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0].status, "ok");
        let relative = videos[0].rel_path.as_ref().expect("sin ruta local");
        assert!(relative.starts_with("Pelis/Accion"));
        assert!(root.join(relative).is_file());

        std::fs::remove_dir_all(&base).unwrap();
    }
}
