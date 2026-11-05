use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use crossbeam_channel::Sender;
use rusqlite::{Connection, params};
use uuid::Uuid;
use walkdir::{DirEntry, WalkDir};

pub enum ScanEvent {
    Progress(usize),
    Finished { found: usize, error: Option<String> },
}

pub fn spawn(root: PathBuf, extensions: Vec<String>, db_path: PathBuf, tx: Sender<ScanEvent>) {
    std::thread::spawn(move || {
        let event = match run(&root, &extensions, &db_path, &tx) {
            Ok(found) => ScanEvent::Finished { found, error: None },
            Err(error) => ScanEvent::Finished {
                found: 0,
                error: Some(error.to_string()),
            },
        };

        let _ = tx.send(event);
    });
}

struct ScannedFile {
    rel_path: String,
    abs_path: String,
    size: i64,
    mtime: i64,
    fingerprint: String,
    title: String,
}

#[derive(Clone)]
struct ExistingFile {
    id: String,
    rel_path: String,
    fingerprint: Option<String>,
    size: Option<i64>,
    mtime: Option<i64>,
}

impl ExistingFile {
    fn match_fingerprint(&self) -> Option<String> {
        self.fingerprint
            .clone()
            .filter(|fingerprint| !fingerprint.is_empty())
            .or_else(|| match (self.size, self.mtime) {
                (Some(size), Some(mtime)) => Some(fingerprint(size, mtime)),
                _ => None,
            })
    }
}

#[derive(Clone)]
struct DuplicateRow {
    id: String,
    url: Option<String>,
    url_norm: Option<String>,
    thumb_key: Option<String>,
    channel: Option<String>,
    duration_s: Option<i64>,
    present: bool,
}

fn merge_duplicate_rows(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let rows: Vec<((i64, i64), DuplicateRow)> = {
        let mut stmt = tx.prepare(
            "SELECT id, size, mtime, url, url_norm, thumb_key, channel, duration_s,
                    CASE WHEN rel_path IN (SELECT rel_path FROM seen) THEN 1 ELSE 0 END
             FROM videos
             WHERE rel_path IS NOT NULL
               AND size IS NOT NULL
               AND mtime IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            let size: i64 = row.get(1)?;
            let mtime: i64 = row.get(2)?;
            Ok((
                (size, mtime),
                DuplicateRow {
                    id: row.get(0)?,
                    url: row.get(3)?,
                    url_norm: row.get(4)?,
                    thumb_key: row.get(5)?,
                    channel: row.get(6)?,
                    duration_s: row.get(7)?,
                    present: row.get::<_, i64>(8)? != 0,
                },
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut groups: HashMap<(i64, i64), Vec<DuplicateRow>> = HashMap::new();
    for (key, row) in rows {
        groups.entry(key).or_default().push(row);
    }

    for group in groups.values() {
        let present: Vec<&DuplicateRow> = group.iter().filter(|row| row.present).collect();
        if present.len() != 1 || group.len() < 2 {
            continue;
        }

        let mut canonical = present[0].clone();
        if canonical.url.is_some() {
            continue;
        }

        for duplicate in group.iter().filter(|row| !row.present && row.url.is_some()) {
            tx.execute(
                "INSERT OR IGNORE INTO video_categories(video_id, category_id)
                 SELECT ?1, category_id FROM video_categories WHERE video_id = ?2",
                params![canonical.id, duplicate.id],
            )?;

            if canonical.thumb_key.is_none() && duplicate.thumb_key.is_some() {
                tx.execute(
                    "UPDATE videos SET thumb_key = ?1 WHERE id = ?2",
                    params![duplicate.thumb_key, canonical.id],
                )?;
                canonical.thumb_key = duplicate.thumb_key.clone();
            }
            if canonical.channel.is_none() && duplicate.channel.is_some() {
                tx.execute(
                    "UPDATE videos SET channel = ?1 WHERE id = ?2",
                    params![duplicate.channel, canonical.id],
                )?;
                canonical.channel = duplicate.channel.clone();
            }
            if canonical.duration_s.is_none() && duplicate.duration_s.is_some() {
                tx.execute(
                    "UPDATE videos SET duration_s = ?1 WHERE id = ?2",
                    params![duplicate.duration_s, canonical.id],
                )?;
                canonical.duration_s = duplicate.duration_s;
            }
            if canonical.url.is_none() && duplicate.url.is_some() {
                tx.execute(
                    "UPDATE videos SET url = NULL, url_norm = NULL WHERE id = ?1",
                    params![duplicate.id],
                )?;
                tx.execute(
                    "UPDATE videos SET url = ?1, url_norm = ?2 WHERE id = ?3",
                    params![duplicate.url, duplicate.url_norm, canonical.id],
                )?;
                canonical.url = duplicate.url.clone();
                canonical.url_norm = duplicate.url_norm.clone();
            }

            tx.execute("DELETE FROM videos WHERE id = ?1", params![duplicate.id])?;
        }
    }

    Ok(())
}

fn run(
    root: &Path,
    extensions: &[String],
    db_path: &Path,
    tx: &Sender<ScanEvent>,
) -> Result<usize> {
    if !root.is_dir() {
        bail!("the folder {} does not exist", root.display());
    }

    let extensions: Vec<String> = extensions
        .iter()
        .map(|ext| ext.trim_start_matches('.').to_ascii_lowercase())
        .collect();

    let mut files = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_ignored(entry))
        .filter_map(|entry| entry.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };
        if !extensions
            .iter()
            .any(|ext| ext == &extension.to_ascii_lowercase())
        {
            continue;
        }

        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };

        let size = metadata.len() as i64;
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);

        files.push(ScannedFile {
            rel_path: relative.to_string_lossy().to_string(),
            abs_path: path.to_string_lossy().to_string(),
            size,
            mtime,
            fingerprint: fingerprint(size, mtime),
            title: title_from_path(path),
        });
    }

    let found = files.len();
    let seen: HashSet<String> = files.iter().map(|file| file.rel_path.clone()).collect();

    let mut conn = Connection::open(db_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    let tx_db = conn.transaction()?;
    tx_db.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS seen(rel_path TEXT PRIMARY KEY);
         DELETE FROM seen;",
    )?;

    let existing: Vec<ExistingFile> = {
        let mut stmt = tx_db.prepare(
            "SELECT id, rel_path, fingerprint, size, mtime
             FROM videos
             WHERE rel_path IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ExistingFile {
                id: row.get(0)?,
                rel_path: row.get(1)?,
                fingerprint: row.get(2)?,
                size: row.get(3)?,
                mtime: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let by_path: HashMap<String, ExistingFile> = existing
        .iter()
        .map(|file| (file.rel_path.clone(), file.clone()))
        .collect();
    let mut by_fingerprint: HashMap<String, Vec<ExistingFile>> = HashMap::new();
    for file in existing {
        if let Some(fingerprint) = file.match_fingerprint() {
            by_fingerprint.entry(fingerprint).or_default().push(file);
        }
    }

    let now = timestamp();
    {
        let mut claimed = HashSet::new();
        let mut update_seen = tx_db.prepare(
            "UPDATE videos
             SET abs_cache = ?1,
                 size = ?2,
                 mtime = ?3,
                 fingerprint = ?4,
                 status = 'ok',
                 last_seen_at = ?5,
                 title = CASE
                   WHEN title IS NULL OR title = '' THEN ?6
                   ELSE title
                 END
             WHERE id = ?7",
        )?;
        let mut move_file = tx_db.prepare(
            "UPDATE videos
             SET rel_path = ?1,
                 abs_cache = ?2,
                 size = ?3,
                 mtime = ?4,
                 fingerprint = ?5,
                 status = 'ok',
                 last_seen_at = ?6,
                 title = CASE
                   WHEN title IS NULL OR title = '' THEN ?7
                   ELSE title
                 END
             WHERE id = ?8",
        )?;
        let mut insert_file = tx_db.prepare(
            "INSERT INTO videos
               (id, rel_path, abs_cache, size, mtime, fingerprint, title, status, added_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'ok', ?8, ?8)",
        )?;
        let mut mark_seen = tx_db.prepare("INSERT OR IGNORE INTO seen(rel_path) VALUES (?1)")?;

        for (index, file) in files.iter().enumerate() {
            if let Some(existing) = by_path.get(&file.rel_path) {
                update_seen.execute(params![
                    file.abs_path,
                    file.size,
                    file.mtime,
                    file.fingerprint,
                    now,
                    file.title,
                    existing.id,
                ])?;
                claimed.insert(existing.id.clone());
            } else {
                let moved = by_fingerprint
                    .get(&file.fingerprint)
                    .and_then(|candidates| {
                        let available: Vec<&ExistingFile> = candidates
                            .iter()
                            .filter(|candidate| {
                                !seen.contains(&candidate.rel_path)
                                    && !claimed.contains(&candidate.id)
                            })
                            .collect();
                        let exact: Vec<&ExistingFile> = available
                            .iter()
                            .copied()
                            .filter(|candidate| {
                                candidate.fingerprint.as_deref() == Some(file.fingerprint.as_str())
                            })
                            .collect();

                        if exact.len() == 1 {
                            Some(exact[0])
                        } else if available.len() == 1 {
                            Some(available[0])
                        } else {
                            None
                        }
                    });

                if let Some(existing) = moved {
                    move_file.execute(params![
                        file.rel_path,
                        file.abs_path,
                        file.size,
                        file.mtime,
                        file.fingerprint,
                        now,
                        file.title,
                        existing.id,
                    ])?;
                    claimed.insert(existing.id.clone());
                } else {
                    insert_file.execute(params![
                        Uuid::now_v7().to_string(),
                        file.rel_path,
                        file.abs_path,
                        file.size,
                        file.mtime,
                        file.fingerprint,
                        file.title,
                        now,
                    ])?;
                }
            }

            mark_seen.execute(params![file.rel_path])?;

            if (index + 1).is_multiple_of(100) {
                let _ = tx.send(ScanEvent::Progress(index + 1));
            }
        }
    }

    merge_duplicate_rows(&tx_db)?;

    tx_db.execute(
        "UPDATE videos
         SET status = CASE WHEN url IS NOT NULL THEN 'link' ELSE 'missing' END
         WHERE rel_path IS NOT NULL
           AND rel_path NOT IN (SELECT rel_path FROM seen)",
        [],
    )?;

    tx_db.commit()?;

    Ok(found)
}

pub enum MediaScanEvent {
    Progress(usize),
    Finished { found: usize, error: Option<String> },
}

pub fn spawn_photos(
    root: PathBuf,
    extensions: Vec<String>,
    db_path: PathBuf,
    tx: Sender<MediaScanEvent>,
) {
    spawn_media(root, extensions, db_path, tx, "photos", true);
}

pub fn spawn_documents(
    root: PathBuf,
    extensions: Vec<String>,
    db_path: PathBuf,
    tx: Sender<MediaScanEvent>,
) {
    spawn_media(root, extensions, db_path, tx, "documents", false);
}

fn spawn_media(
    root: PathBuf,
    extensions: Vec<String>,
    db_path: PathBuf,
    tx: Sender<MediaScanEvent>,
    table: &'static str,
    read_dimensions: bool,
) {
    std::thread::spawn(move || {
        let event = match run_media(&root, &extensions, &db_path, &tx, table, read_dimensions) {
            Ok(found) => MediaScanEvent::Finished { found, error: None },
            Err(error) => MediaScanEvent::Finished {
                found: 0,
                error: Some(error.to_string()),
            },
        };

        let _ = tx.send(event);
    });
}

struct ScannedMedia {
    rel_path: String,
    abs_path: String,
    size: i64,
    mtime: i64,
    fingerprint: String,
    title: String,
    width: Option<i64>,
    height: Option<i64>,
}

#[derive(Clone)]
struct ExistingMedia {
    id: String,
    rel_path: String,
    fingerprint: Option<String>,
    size: Option<i64>,
    mtime: Option<i64>,
}

impl ExistingMedia {
    fn match_fingerprint(&self) -> Option<String> {
        self.fingerprint
            .clone()
            .filter(|fingerprint| !fingerprint.is_empty())
            .or_else(|| match (self.size, self.mtime) {
                (Some(size), Some(mtime)) => Some(fingerprint(size, mtime)),
                _ => None,
            })
    }
}

fn run_media(
    root: &Path,
    extensions: &[String],
    db_path: &Path,
    tx: &Sender<MediaScanEvent>,
    table: &str,
    read_dimensions: bool,
) -> Result<usize> {
    if !root.is_dir() {
        bail!("the folder {} does not exist", root.display());
    }

    let extensions: Vec<String> = extensions
        .iter()
        .map(|ext| ext.trim_start_matches('.').to_ascii_lowercase())
        .collect();

    let mut files = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_ignored(entry))
        .filter_map(|entry| entry.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };
        if !extensions
            .iter()
            .any(|ext| ext == &extension.to_ascii_lowercase())
        {
            continue;
        }

        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };

        let size = metadata.len() as i64;
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        let (width, height) = if read_dimensions {
            image::image_dimensions(path)
                .map(|(width, height)| (Some(width as i64), Some(height as i64)))
                .unwrap_or((None, None))
        } else {
            (None, None)
        };

        files.push(ScannedMedia {
            rel_path: relative.to_string_lossy().to_string(),
            abs_path: path.to_string_lossy().to_string(),
            size,
            mtime,
            fingerprint: fingerprint(size, mtime),
            title: title_from_path(path),
            width,
            height,
        });
    }

    let found = files.len();
    let seen: HashSet<String> = files.iter().map(|file| file.rel_path.clone()).collect();

    let mut conn = Connection::open(db_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    let seen_table = format!("{table}_seen");
    let tx_db = conn.transaction()?;
    tx_db.execute_batch(&format!(
        "CREATE TEMP TABLE IF NOT EXISTS {seen_table}(rel_path TEXT PRIMARY KEY);
         DELETE FROM {seen_table};"
    ))?;

    let existing: Vec<ExistingMedia> = {
        let mut stmt = tx_db.prepare(&format!(
            "SELECT id, rel_path, fingerprint, size, mtime
             FROM {table}
             WHERE rel_path IS NOT NULL"
        ))?;
        let rows = stmt.query_map([], |row| {
            Ok(ExistingMedia {
                id: row.get(0)?,
                rel_path: row.get(1)?,
                fingerprint: row.get(2)?,
                size: row.get(3)?,
                mtime: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let by_path: HashMap<String, ExistingMedia> = existing
        .iter()
        .map(|file| (file.rel_path.clone(), file.clone()))
        .collect();
    let mut by_fingerprint: HashMap<String, Vec<ExistingMedia>> = HashMap::new();
    for file in existing {
        if let Some(fingerprint) = file.match_fingerprint() {
            by_fingerprint.entry(fingerprint).or_default().push(file);
        }
    }

    let now = timestamp();
    {
        let mut claimed = HashSet::new();
        let mut update_seen = tx_db.prepare(&format!(
            "UPDATE {table}
             SET abs_cache = ?1,
                 size = ?2,
                 mtime = ?3,
                 fingerprint = ?4,
                 width = ?5,
                 height = ?6,
                 status = 'ok',
                 last_seen_at = ?7,
                 title = CASE
                   WHEN title IS NULL OR title = '' THEN ?8
                   ELSE title
                 END
             WHERE id = ?9"
        ))?;
        let mut move_file = tx_db.prepare(&format!(
            "UPDATE {table}
             SET rel_path = ?1,
                 abs_cache = ?2,
                 size = ?3,
                 mtime = ?4,
                 fingerprint = ?5,
                 width = ?6,
                 height = ?7,
                 status = 'ok',
                 last_seen_at = ?8,
                 title = CASE
                   WHEN title IS NULL OR title = '' THEN ?9
                   ELSE title
                 END
             WHERE id = ?10"
        ))?;
        let mut insert_file = tx_db.prepare(&format!(
            "INSERT INTO {table}
               (id, rel_path, abs_cache, size, mtime, fingerprint, title, width, height,
                status, added_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'ok', ?10, ?10)"
        ))?;
        let mut mark_seen = tx_db.prepare(&format!(
            "INSERT OR IGNORE INTO {seen_table}(rel_path) VALUES (?1)"
        ))?;

        for (index, file) in files.iter().enumerate() {
            if let Some(existing) = by_path.get(&file.rel_path) {
                update_seen.execute(params![
                    file.abs_path,
                    file.size,
                    file.mtime,
                    file.fingerprint,
                    file.width,
                    file.height,
                    now,
                    file.title,
                    existing.id,
                ])?;
                claimed.insert(existing.id.clone());
            } else {
                let moved = by_fingerprint
                    .get(&file.fingerprint)
                    .and_then(|candidates| {
                        let available: Vec<&ExistingMedia> = candidates
                            .iter()
                            .filter(|candidate| {
                                !seen.contains(&candidate.rel_path)
                                    && !claimed.contains(&candidate.id)
                            })
                            .collect();
                        let exact: Vec<&ExistingMedia> = available
                            .iter()
                            .copied()
                            .filter(|candidate| {
                                candidate.fingerprint.as_deref() == Some(file.fingerprint.as_str())
                            })
                            .collect();

                        if exact.len() == 1 {
                            Some(exact[0])
                        } else if available.len() == 1 {
                            Some(available[0])
                        } else {
                            None
                        }
                    });

                if let Some(existing) = moved {
                    move_file.execute(params![
                        file.rel_path,
                        file.abs_path,
                        file.size,
                        file.mtime,
                        file.fingerprint,
                        file.width,
                        file.height,
                        now,
                        file.title,
                        existing.id,
                    ])?;
                    claimed.insert(existing.id.clone());
                } else {
                    insert_file.execute(params![
                        Uuid::now_v7().to_string(),
                        file.rel_path,
                        file.abs_path,
                        file.size,
                        file.mtime,
                        file.fingerprint,
                        file.title,
                        file.width,
                        file.height,
                        now,
                    ])?;
                }
            }

            mark_seen.execute(params![file.rel_path])?;

            if (index + 1).is_multiple_of(100) {
                let _ = tx.send(MediaScanEvent::Progress(index + 1));
            }
        }
    }

    tx_db.execute(
        &format!(
            "UPDATE {table}
             SET status = 'missing'
             WHERE rel_path IS NOT NULL
               AND rel_path NOT IN (SELECT rel_path FROM {seen_table})"
        ),
        [],
    )?;

    tx_db.commit()?;

    Ok(found)
}

fn is_ignored(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    name.starts_with('.') || name == "node_modules" || name == "lost+found"
}

fn title_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.replace('_', " ").trim().to_string())
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

fn fingerprint(size: i64, mtime: i64) -> String {
    format!("{size}:{mtime}")
}

fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::title_from_path;
    use std::path::Path;

    #[test]
    fn builds_title_from_file_stem() {
        assert_eq!(
            title_from_path(Path::new("/videos/Mi_pelicula.mkv")),
            "Mi pelicula"
        );
    }
}

#[cfg(test)]
mod scan_tests {
    use super::run;
    use crate::db::Db;
    use crossbeam_channel::unbounded;
    use rusqlite::{Connection, params};
    use std::fs::{self, FileTimes, OpenOptions};
    use std::time::UNIX_EPOCH;
    use uuid::Uuid;

    fn temp_root() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("video-player-test-{}", Uuid::now_v7()));
        fs::create_dir_all(base.join("Videos/sub")).unwrap();
        base
    }

    #[test]
    fn indexes_videos_and_marks_missing_ones() {
        let base = temp_root();
        let root = base.join("Videos");
        let db_path = base.join("library.db");
        let db = Db::open(&db_path).unwrap();

        fs::write(root.join("uno.mkv"), b"uno").unwrap();
        fs::write(root.join("sub/dos.mp4"), b"dos").unwrap();
        fs::write(root.join("nota.txt"), b"texto").unwrap();

        let (tx, _rx) = unbounded();
        let extensions = vec!["mkv".to_string(), "mp4".to_string()];

        let found = run(&root, &extensions, &db_path, &tx).unwrap();
        assert_eq!(found, 2);
        assert_eq!(db.list_videos().unwrap().len(), 2);

        fs::remove_file(root.join("uno.mkv")).unwrap();
        let found = run(&root, &extensions, &db_path, &tx).unwrap();
        assert_eq!(found, 1);

        let videos = db.list_videos().unwrap();
        let missing = videos
            .iter()
            .find(|video| video.title == "uno")
            .expect("video eliminado no encontrado");
        assert_eq!(missing.status, "missing");

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn preserves_identity_when_video_moves() {
        let base = temp_root();
        let root = base.join("Videos");
        let db_path = base.join("library.db");
        let mut db = Db::open(&db_path).unwrap();

        fs::write(root.join("uno.mkv"), b"contenido").unwrap();

        let (tx, _rx) = unbounded();
        let extensions = vec!["mkv".to_string()];

        run(&root, &extensions, &db_path, &tx).unwrap();
        let before = db.list_videos().unwrap().pop().expect("video no indexado");
        db.assign_category_path(&before.id, "Pelis/Accion").unwrap();

        fs::create_dir_all(root.join("sub")).unwrap();
        fs::rename(root.join("uno.mkv"), root.join("sub/uno.mkv")).unwrap();

        let found = run(&root, &extensions, &db_path, &tx).unwrap();
        assert_eq!(found, 1);

        let videos = db.list_videos().unwrap();
        assert_eq!(videos.len(), 1);
        let after = &videos[0];
        assert_eq!(after.id, before.id);
        assert_eq!(after.rel_path.as_deref(), Some("sub/uno.mkv"));
        assert_eq!(after.status, "ok");

        let assignments = db.list_assignments().unwrap();
        assert!(
            assignments
                .iter()
                .any(|(video_id, _)| video_id == &before.id)
        );

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn preserves_identity_when_legacy_row_has_no_fingerprint() {
        let base = temp_root();
        let root = base.join("Videos");
        let db_path = base.join("library.db");
        let mut db = Db::open(&db_path).unwrap();

        fs::write(root.join("uno.mkv"), b"contenido").unwrap();

        let (tx, _rx) = unbounded();
        let extensions = vec!["mkv".to_string()];
        run(&root, &extensions, &db_path, &tx).unwrap();

        let before = db.list_videos().unwrap().pop().expect("video no indexado");
        db.assign_category_path(&before.id, "Pelis/Accion").unwrap();
        Connection::open(&db_path)
            .unwrap()
            .execute("UPDATE videos SET fingerprint = NULL", [])
            .unwrap();

        fs::create_dir_all(root.join("sub")).unwrap();
        fs::rename(root.join("uno.mkv"), root.join("sub/uno.mkv")).unwrap();

        run(&root, &extensions, &db_path, &tx).unwrap();

        let videos = db.list_videos().unwrap();
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0].id, before.id);
        assert_eq!(videos[0].rel_path.as_deref(), Some("sub/uno.mkv"));

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn merges_legacy_duplicate_into_seen_file() {
        let base = temp_root();
        let root = base.join("Videos");
        let db_path = base.join("library.db");
        let db = Db::open(&db_path).unwrap();

        fs::write(root.join("uno.mkv"), b"contenido").unwrap();
        let (tx, _rx) = unbounded();
        let extensions = vec!["mkv".to_string()];
        run(&root, &extensions, &db_path, &tx).unwrap();

        let before = db.list_videos().unwrap().pop().expect("video no indexado");
        let metadata = fs::metadata(root.join("uno.mkv")).unwrap();
        let size = metadata.len() as i64;
        let mtime = metadata
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute(
                "INSERT INTO videos
                   (id, url, url_norm, rel_path, size, mtime, title, status, added_at)
                 VALUES ('legacy', 'https://example.com/video', 'https://example.com/video',
                         'viejo/uno.mkv', ?1, ?2, 'Viejo', 'link', '0')",
                params![size, mtime],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO categories(id, parent_id, name, created_at)
                 VALUES ('categoria', NULL, 'Pelis', '0')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO video_categories(video_id, category_id)
                 VALUES ('legacy', 'categoria')",
                [],
            )
            .unwrap();
        }

        run(&root, &extensions, &db_path, &tx).unwrap();

        let videos = db.list_videos().unwrap();
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0].id, before.id);
        assert_eq!(videos[0].url.as_deref(), Some("https://example.com/video"));
        assert_eq!(
            db.list_assignments().unwrap(),
            vec![(before.id.clone(), "categoria".to_string())]
        );

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn keeps_copies_when_fingerprint_matches_an_existing_file() {
        let base = temp_root();
        let root = base.join("Videos");
        let db_path = base.join("library.db");
        let db = Db::open(&db_path).unwrap();

        let source = root.join("uno.mkv");
        fs::write(&source, b"contenido").unwrap();
        let modified = fs::metadata(&source).unwrap().modified().unwrap();

        let (tx, _rx) = unbounded();
        let extensions = vec!["mkv".to_string()];
        run(&root, &extensions, &db_path, &tx).unwrap();

        let copy = root.join("copia.mkv");
        fs::copy(&source, &copy).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&copy)
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();

        let found = run(&root, &extensions, &db_path, &tx).unwrap();
        assert_eq!(found, 2);

        let videos = db.list_videos().unwrap();
        assert_eq!(videos.len(), 2);
        assert!(videos.iter().all(|video| video.status == "ok"));
        assert_ne!(videos[0].id, videos[1].id);

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn indexes_photos_and_preserves_identity_when_moved() {
        let base = temp_root();
        let root = base.join("Pictures");
        fs::create_dir_all(&root).unwrap();
        let db_path = base.join("library.db");
        let mut db = Db::open(&db_path).unwrap();

        image::RgbImage::from_pixel(4, 3, image::Rgb([10, 20, 30]))
            .save(root.join("foto.png"))
            .unwrap();
        fs::write(root.join("nota.txt"), b"texto").unwrap();

        let (tx, _rx) = unbounded();
        let extensions = vec!["png".to_string()];
        let found = super::run_media(&root, &extensions, &db_path, &tx, "photos", true).unwrap();
        assert_eq!(found, 1);

        let before = db.list_photos().unwrap().pop().expect("foto no indexada");
        assert_eq!(before.width, Some(4));
        assert_eq!(before.height, Some(3));

        db.create_photo_category(None, "Viajes").unwrap();
        let category_id = db.list_photo_categories().unwrap()[0].id.clone();
        db.add_photo_category(&before.id, &category_id).unwrap();

        fs::create_dir_all(root.join("sub")).unwrap();
        fs::rename(root.join("foto.png"), root.join("sub/foto.png")).unwrap();
        super::run_media(&root, &extensions, &db_path, &tx, "photos", true).unwrap();

        let photos = db.list_photos().unwrap();
        assert_eq!(photos.len(), 1);
        assert_eq!(photos[0].id, before.id);
        assert_eq!(photos[0].rel_path.as_deref(), Some("sub/foto.png"));
        assert_eq!(db.list_photo_assignments().unwrap().len(), 1);

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn indexes_documents_and_preserves_identity_when_moved() {
        let base = temp_root();
        let root = base.join("Documents");
        fs::create_dir_all(&root).unwrap();
        let db_path = base.join("library.db");
        let mut db = Db::open(&db_path).unwrap();

        fs::write(root.join("manual.pdf"), b"%PDF-1.4").unwrap();
        fs::write(root.join("nota.txt"), b"texto").unwrap();

        let (tx, _rx) = unbounded();
        let extensions = vec!["pdf".to_string()];
        let found =
            super::run_media(&root, &extensions, &db_path, &tx, "documents", false).unwrap();
        assert_eq!(found, 1);

        let before = db
            .list_documents()
            .unwrap()
            .pop()
            .expect("documento no indexado");

        db.create_document_category(None, "Manuales").unwrap();
        let category_id = db.list_document_categories().unwrap()[0].id.clone();
        db.add_document_category(&before.id, &category_id).unwrap();

        fs::create_dir_all(root.join("sub")).unwrap();
        fs::rename(root.join("manual.pdf"), root.join("sub/manual.pdf")).unwrap();
        super::run_media(&root, &extensions, &db_path, &tx, "documents", false).unwrap();

        let documents = db.list_documents().unwrap();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].id, before.id);
        assert_eq!(documents[0].rel_path.as_deref(), Some("sub/manual.pdf"));
        assert_eq!(db.list_document_assignments().unwrap().len(), 1);

        fs::remove_dir_all(&base).unwrap();
    }
}
