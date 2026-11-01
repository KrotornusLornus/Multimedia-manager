use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Video {
    pub id: String,
    pub title: String,
    pub rel_path: Option<String>,
    pub abs_cache: Option<PathBuf>,
    pub status: String,
    pub url: Option<String>,
    pub channel: Option<String>,
    pub duration_s: Option<i64>,
    pub thumb_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Category {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Photo {
    pub id: String,
    pub title: String,
    pub rel_path: Option<String>,
    pub abs_cache: Option<PathBuf>,
    pub status: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub thumb_key: Option<String>,
}

impl Photo {
    pub fn path(&self, root: &Path) -> Option<PathBuf> {
        if let Some(abs) = &self.abs_cache
            && abs.is_file()
        {
            return Some(abs.clone());
        }

        if let Some(rel) = &self.rel_path {
            let candidate = root.join(rel);
            if candidate.is_file() {
                return Some(candidate);
            }
        }

        None
    }
}

impl Video {
    pub fn path(&self, root: &Path) -> Option<PathBuf> {
        if let Some(abs) = &self.abs_cache
            && abs.is_file()
        {
            return Some(abs.clone());
        }

        if let Some(rel) = &self.rel_path {
            let candidate = root.join(rel);
            if candidate.is_file() {
                return Some(candidate);
            }
        }

        None
    }

    pub fn badge(&self) -> String {
        self.rel_path
            .as_deref()
            .and_then(|path| Path::new(path).extension())
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_uppercase())
            .unwrap_or_else(|| "URL".to_string())
    }
}
