use std::collections::HashMap;
use std::path::{Path, PathBuf};

use directories::{ProjectDirs, UserDirs};

#[derive(Debug, Clone)]
pub struct Config {
    pub root: PathBuf,
    pub extensions: Vec<String>,
    pub mpv: String,
    pub ytdlp: String,
    pub ffmpeg: String,
    pub ytdl_format: String,
    pub thumb_width: u32,
    pub pictures_root: PathBuf,
    pub photo_extensions: Vec<String>,
    pub image_viewer: String,
    pub photo_thumb_width: u32,
}

impl Default for Config {
    fn default() -> Self {
        let user_dirs = UserDirs::new();
        let root = user_dirs
            .as_ref()
            .map(|dirs| dirs.home_dir().join("Videos"))
            .unwrap_or_else(|| PathBuf::from("Videos"));
        let pictures_root = user_dirs
            .as_ref()
            .and_then(|dirs| dirs.picture_dir().map(Path::to_path_buf))
            .or_else(|| {
                user_dirs
                    .as_ref()
                    .map(|dirs| dirs.home_dir().join("Pictures"))
            })
            .unwrap_or_else(|| PathBuf::from("Pictures"));

        Self {
            root,
            extensions: ["mkv", "mp4", "webm", "avi", "mov", "m4v", "ts", "flv"]
                .iter()
                .map(|ext| ext.to_string())
                .collect(),
            mpv: "mpv".to_string(),
            ytdlp: "yt-dlp".to_string(),
            ffmpeg: "ffmpeg".to_string(),
            ytdl_format: "bv*[height<=1080]+ba/b".to_string(),
            thumb_width: 320,
            pictures_root,
            photo_extensions: ["jpg", "jpeg", "png", "webp", "gif"]
                .iter()
                .map(|ext| ext.to_string())
                .collect(),
            image_viewer: "swayimg".to_string(),
            photo_thumb_width: 320,
        }
    }
}

impl Config {
    pub fn from_meta(meta: &HashMap<String, String>) -> Self {
        let mut config = Self::default();

        if let Some(value) = meta.get("root") {
            config.root = PathBuf::from(value);
        }
        if let Some(value) = meta.get("pictures_root") {
            config.pictures_root = PathBuf::from(value);
        }
        if let Some(value) = meta.get("mpv") {
            config.mpv = value.clone();
        }
        if let Some(value) = meta.get("ytdlp") {
            config.ytdlp = value.clone();
        }
        if let Some(value) = meta.get("ffmpeg") {
            config.ffmpeg = value.clone();
        }
        if let Some(value) = meta.get("ytdl_format") {
            config.ytdl_format = value.clone();
        }
        if let Some(value) = meta.get("image_viewer") {
            config.image_viewer = value.clone();
        }
        if let Some(value) = meta.get("thumb_width").and_then(|value| value.parse().ok()) {
            config.thumb_width = value;
        }
        if let Some(value) = meta
            .get("photo_thumb_width")
            .and_then(|value| value.parse().ok())
        {
            config.photo_thumb_width = value;
        }
        if let Some(value) = meta.get("extensions") {
            config.extensions = split_list(value);
        }
        if let Some(value) = meta.get("photo_extensions") {
            config.photo_extensions = split_list(value);
        }

        config
    }

    pub fn to_meta(&self) -> Vec<(String, String)> {
        vec![
            ("root".to_string(), self.root.display().to_string()),
            (
                "pictures_root".to_string(),
                self.pictures_root.display().to_string(),
            ),
            ("mpv".to_string(), self.mpv.clone()),
            ("ytdlp".to_string(), self.ytdlp.clone()),
            ("ffmpeg".to_string(), self.ffmpeg.clone()),
            ("ytdl_format".to_string(), self.ytdl_format.clone()),
            ("image_viewer".to_string(), self.image_viewer.clone()),
            ("thumb_width".to_string(), self.thumb_width.to_string()),
            (
                "photo_thumb_width".to_string(),
                self.photo_thumb_width.to_string(),
            ),
            ("extensions".to_string(), self.extensions.join(", ")),
            (
                "photo_extensions".to_string(),
                self.photo_extensions.join(", "),
            ),
        ]
    }
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|item| item.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|item| !item.is_empty())
        .collect()
}

pub fn project_dirs() -> ProjectDirs {
    ProjectDirs::from("", "", "video-player").expect("no se pudo resolver el directorio de datos")
}

pub fn data_dir() -> PathBuf {
    project_dirs().data_dir().to_path_buf()
}

pub fn cache_dir() -> PathBuf {
    project_dirs().cache_dir().to_path_buf()
}

pub fn thumbs_dir() -> PathBuf {
    cache_dir().join("thumbs")
}

pub fn photo_thumbs_dir() -> PathBuf {
    cache_dir().join("photos")
}
