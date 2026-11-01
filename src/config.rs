use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use directories::{ProjectDirs, UserDirs};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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

pub fn load_or_create() -> Result<Config> {
    let config_dir = project_dirs().config_dir().to_path_buf();
    fs::create_dir_all(&config_dir)?;

    let path = config_dir.join("config.toml");
    if path.exists() {
        let text = fs::read_to_string(&path)?;
        Ok(toml::from_str(&text)?)
    } else {
        let config = Config::default();
        fs::write(&path, toml::to_string_pretty(&config)?)?;
        Ok(config)
    }
}
