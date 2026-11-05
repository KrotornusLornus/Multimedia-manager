use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use anyhow::{Result, bail};
use crossbeam_channel::{Receiver, Sender, unbounded};
use image::{ImageDecoder, ImageFormat, imageops::FilterType};

struct Job {
    id: String,
    path: PathBuf,
}

pub struct Thumbs {
    jobs: Sender<Job>,
    done: Receiver<String>,
}

impl Thumbs {
    pub fn new(directory: PathBuf, ffmpeg: String, width: u32, workers: usize) -> Self {
        let (jobs, job_rx) = unbounded::<Job>();
        let (done_tx, done) = unbounded::<String>();

        for _ in 0..workers {
            let job_rx = job_rx.clone();
            let done_tx = done_tx.clone();
            let directory = directory.clone();
            let ffmpeg = ffmpeg.clone();

            thread::spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let _ = generate_local(&directory, &job.id, &job.path, &ffmpeg, width);
                    let _ = done_tx.send(job.id);
                }
            });
        }

        Self { jobs, done }
    }

    pub fn enqueue(&self, id: &str, path: &Path) {
        let _ = self.jobs.send(Job {
            id: id.to_string(),
            path: path.to_path_buf(),
        });
    }

    pub fn poll(&self) -> Vec<String> {
        self.done.try_iter().collect()
    }
}

struct PhotoJob {
    id: String,
    path: PathBuf,
}

pub struct PhotoThumbs {
    jobs: Sender<PhotoJob>,
    done: Receiver<String>,
}

impl PhotoThumbs {
    pub fn new(directory: PathBuf, width: u32, workers: usize) -> Self {
        let (jobs, job_rx) = unbounded::<PhotoJob>();
        let (done_tx, done) = unbounded::<String>();

        for _ in 0..workers {
            let job_rx = job_rx.clone();
            let done_tx = done_tx.clone();
            let directory = directory.clone();

            thread::spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let _ = generate_photo(&directory, &job.id, &job.path, width);
                    let _ = done_tx.send(job.id);
                }
            });
        }

        Self { jobs, done }
    }

    pub fn enqueue(&self, id: &str, path: &Path) {
        let _ = self.jobs.send(PhotoJob {
            id: id.to_string(),
            path: path.to_path_buf(),
        });
    }

    pub fn poll(&self) -> Vec<String> {
        self.done.try_iter().collect()
    }
}

fn generate_photo(directory: &Path, id: &str, path: &Path, width: u32) -> Result<String> {
    fs::create_dir_all(directory)?;

    let key = format!("{id}.jpg");
    let reader = image::ImageReader::open(path)?.with_guessed_format()?;
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = image::DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);

    let source_width = image.width().max(1);
    let source_height = image.height().max(1);
    let target_width = width.min(source_width);
    let target_height = ((source_height as f32 * target_width as f32 / source_width as f32).round())
        .max(1.0) as u32;
    let resized = image.resize(target_width, target_height, FilterType::Triangle);

    let mut output = fs::File::create(directory.join(&key))?;
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 75);
    encoder.encode_image(&resized)?;

    Ok(key)
}

struct DocumentJob {
    id: String,
    path: PathBuf,
}

pub struct DocumentThumbs {
    jobs: Sender<DocumentJob>,
    done: Receiver<String>,
}

impl DocumentThumbs {
    pub fn new(directory: PathBuf, width: u32, python: String, workers: usize) -> Self {
        let (jobs, job_rx) = unbounded::<DocumentJob>();
        let (done_tx, done) = unbounded::<String>();

        for _ in 0..workers {
            let job_rx = job_rx.clone();
            let done_tx = done_tx.clone();
            let directory = directory.clone();
            let python = python.clone();

            thread::spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let _ = generate_document_thumb(&directory, &job.id, &job.path, width, &python);
                    let _ = done_tx.send(job.id);
                }
            });
        }

        Self { jobs, done }
    }

    pub fn enqueue(&self, id: &str, path: &Path) {
        let _ = self.jobs.send(DocumentJob {
            id: id.to_string(),
            path: path.to_path_buf(),
        });
    }

    pub fn poll(&self) -> Vec<String> {
        self.done.try_iter().collect()
    }
}

fn generate_document_thumb(
    directory: &Path,
    id: &str,
    path: &Path,
    width: u32,
    python: &str,
) -> Result<String> {
    fs::create_dir_all(directory)?;

    let key = format!("{id}.png");
    let output = directory.join(&key);
    let script = "import pymupdf,sys\nsrc,out,w=sys.argv[1],sys.argv[2],float(sys.argv[3])\ndoc=pymupdf.open(src)\npage=doc.load_page(0)\nz=w/max(page.rect.width,1.0)\npix=page.get_pixmap(matrix=pymupdf.Matrix(z,z))\npix.save(out)\n";

    let status = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(path)
        .arg(&output)
        .arg(width.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    if !status.success() || !output.is_file() {
        bail!("could not render the document thumbnail");
    }

    Ok(key)
}

pub fn find(directory: &Path, thumb_key: &str) -> Option<PathBuf> {
    let path = directory.join(thumb_key);
    path.is_file().then_some(path)
}

pub fn store_remote_thumb(
    directory: &Path,
    id: &str,
    bytes: &[u8],
    reported_width: Option<u32>,
    target_width: u32,
) -> Result<String> {
    fs::create_dir_all(directory)?;

    let format = image::guess_format(bytes).unwrap_or(ImageFormat::Jpeg);
    let raw_extension = match format {
        ImageFormat::Jpeg => Some("jpg"),
        ImageFormat::Png => Some("png"),
        ImageFormat::WebP => Some("webp"),
        _ => None,
    };
    let reported_width = reported_width.filter(|width| *width > 0);
    let max_small = target_width + target_width / 4;

    if let (Some(extension), Some(width)) = (raw_extension, reported_width)
        && width <= max_small
    {
        let key = format!("{id}.{extension}");
        fs::write(directory.join(&key), bytes)?;
        return Ok(key);
    }

    let image = image::load_from_memory_with_format(bytes, format)?;
    let key = format!("{id}.jpg");

    if let Some(extension) = raw_extension
        && image.width() <= max_small
    {
        let key = format!("{id}.{extension}");
        fs::write(directory.join(&key), bytes)?;
        return Ok(key);
    }

    let height = (image.height() as f32 * target_width as f32 / image.width() as f32)
        .round()
        .max(1.0) as u32;
    let resized = image.resize(target_width, height, FilterType::Lanczos3);

    let mut output = fs::File::create(directory.join(&key))?;
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 80);
    encoder.encode_image(&resized)?;

    Ok(key)
}

fn generate_local(
    directory: &Path,
    id: &str,
    video: &Path,
    ffmpeg: &str,
    width: u32,
) -> Result<String> {
    fs::create_dir_all(directory)?;

    let key = format!("{id}.jpg");
    let output = directory.join(&key);
    let filter = format!("scale={width}:-2");

    if render_frame(ffmpeg, video, &output, &filter, 3).is_err() {
        render_frame(ffmpeg, video, &output, &filter, 0)?;
    }

    Ok(key)
}

fn render_frame(
    ffmpeg: &str,
    video: &Path,
    output: &Path,
    filter: &str,
    seconds: u32,
) -> Result<()> {
    let seconds = seconds.to_string();
    let video = video.to_string_lossy().to_string();
    let output_path = output.to_string_lossy().to_string();

    let status = Command::new(ffmpeg)
        .args([
            "-y",
            "-ss",
            &seconds,
            "-i",
            &video,
            "-frames:v",
            "1",
            "-vf",
            filter,
            "-q:v",
            "4",
            &output_path,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    if !status.success() || !output.is_file() {
        bail!("ffmpeg no pudo generar el thumbnail");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::store_remote_thumb;
    use image::{ImageFormat, Rgb, RgbImage};
    use std::fs;
    use std::io::Cursor;
    use uuid::Uuid;

    #[test]
    fn resizes_large_remote_thumbnails() {
        let directory = std::env::temp_dir().join(format!("video-player-thumb-{}", Uuid::now_v7()));
        let image = RgbImage::from_pixel(1280, 720, Rgb([30, 60, 90]));
        let mut bytes = Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, ImageFormat::Jpeg)
            .expect("could not encode the test image");

        let key = store_remote_thumb(&directory, "abc", bytes.get_ref(), Some(1280), 320)
            .expect("could not save the thumbnail");
        assert_eq!(key, "abc.jpg");

        let saved = image::open(directory.join(&key)).expect("thumbnail ilegible");
        assert_eq!(saved.width(), 320);

        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn generates_photo_thumbnails() {
        let directory =
            std::env::temp_dir().join(format!("video-player-photo-thumb-{}", Uuid::now_v7()));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("foto.png");
        RgbImage::from_pixel(80, 40, Rgb([100, 150, 200]))
            .save(&source)
            .expect("could not save the test image");

        let key = super::generate_photo(&directory, "foto", &source, 20)
            .expect("could not generate the thumbnail");
        assert_eq!(key, "foto.jpg");

        let saved = image::open(directory.join(&key)).expect("thumbnail ilegible");
        assert_eq!(saved.width(), 20);
        assert_eq!(saved.height(), 10);

        fs::remove_dir_all(&directory).unwrap();
    }
}
