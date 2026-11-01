use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crossbeam_channel::{Receiver, TryRecvError};
use eframe::egui::{self, Align, Align2, FontId, Key, Layout, RichText, Sense, StrokeKind};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as MatcherConfig, Matcher, Utf32Str};
use walkdir::WalkDir;

use crate::config::{self, Config};
use crate::db::Db;
use crate::domain::{Category, Photo, Video};
use crate::player::{PlaybackState, Player, spawn_detached};
use crate::scan::{self, PhotoScanEvent, ScanEvent};
use crate::thumbs::{self, PhotoThumbs, Thumbs};
use crate::ytdlp::{self, DownloadEvent, DownloadProgress, ImportEvent, ImportOptions};

const CARD_MIN_WIDTH: f32 = 168.0;
const CARD_GAP: f32 = 14.0;
const CARD_RATIO: f32 = 9.0 / 16.0;
const NAME_FILTER_ID: &str = "name-filter";

#[derive(Clone, Copy)]
enum CardAction {
    Play,
    Download,
    Categories,
    Info,
    RegenerateYtdlp,
    RegenerateFfmpeg,
    UnlinkUrl,
    Delete,
    RemoveRow,
    SetPlayQuality(Quality),
}

#[derive(Clone)]
struct VideoTarget {
    id: String,
    title: String,
    path: Option<PathBuf>,
    thumb_key: Option<String>,
}

struct LogEntry {
    text: String,
    is_error: bool,
}

#[derive(Clone)]
enum ConfirmAction {
    DeleteFiles(Vec<VideoTarget>),
    RemoveRows(Vec<VideoTarget>),
    DeletePhotos(Vec<VideoTarget>),
    RemovePhotoRows(Vec<VideoTarget>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quality {
    Preferred,
    Maximum,
    P1080,
    P720,
    P480,
}

impl Quality {
    const ALL: [Quality; 5] = [
        Quality::Preferred,
        Quality::Maximum,
        Quality::P1080,
        Quality::P720,
        Quality::P480,
    ];

    fn label(self) -> &'static str {
        match self {
            Quality::Preferred => "Predeterminada",
            Quality::Maximum => "Máxima",
            Quality::P1080 => "1080p",
            Quality::P720 => "720p",
            Quality::P480 => "480p",
        }
    }

    fn format(self, preferred: &str) -> String {
        match self {
            Quality::Preferred => preferred.to_string(),
            Quality::Maximum => "bv*+ba/b".to_string(),
            Quality::P1080 => "bv*[height<=1080]+ba/b[height<=1080]/b".to_string(),
            Quality::P720 => "bv*[height<=720]+ba/b[height<=720]/b".to_string(),
            Quality::P480 => "bv*[height<=480]+ba/b[height<=480]/b".to_string(),
        }
    }
}

#[derive(Clone)]
struct DownloadTask {
    id: String,
    url: String,
    subdir: String,
    format: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SourceFilter {
    #[default]
    All,
    Local,
    Online,
}

impl SourceFilter {
    const ALL: [SourceFilter; 3] = [SourceFilter::All, SourceFilter::Local, SourceFilter::Online];

    fn label(self) -> &'static str {
        match self {
            SourceFilter::All => "Todos",
            SourceFilter::Local => "Locales",
            SourceFilter::Online => "Online",
        }
    }

    fn matches(self, local: bool, online: bool) -> bool {
        match self {
            SourceFilter::All => true,
            SourceFilter::Local => local,
            SourceFilter::Online => online && !local,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Default)]
enum CategoryFilter {
    #[default]
    All,
    Uncategorized,
    Path(String),
}

impl CategoryFilter {
    fn matches(&self, paths: &[&str]) -> bool {
        match self {
            CategoryFilter::All => true,
            CategoryFilter::Uncategorized => paths.is_empty(),
            CategoryFilter::Path(selected) => paths.iter().any(|path| {
                *path == selected
                    || path
                        .strip_prefix(selected)
                        .is_some_and(|rest| rest.starts_with('/'))
            }),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SearchMode {
    #[default]
    Name,
    Category,
    Path,
}

impl SearchMode {
    const ALL: [SearchMode; 3] = [SearchMode::Name, SearchMode::Category, SearchMode::Path];

    fn label(self) -> &'static str {
        match self {
            SearchMode::Name => "Nombre",
            SearchMode::Category => "Categoría",
            SearchMode::Path => "Ruta",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            SearchMode::Name => "Buscar por nombre…",
            SearchMode::Category => "Buscar categoría…",
            SearchMode::Path => "Buscar carpeta local…",
        }
    }
}

#[derive(Clone)]
enum CategoryDialog {
    Create {
        parent_id: Option<String>,
        parent_path: Option<String>,
        name: String,
    },
    Rename {
        id: String,
        old_path: String,
        name: String,
    },
    Delete {
        id: String,
        path: String,
        categories: usize,
        videos: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CategorySelection {
    None,
    Some,
    All,
}

#[derive(Clone, PartialEq, Eq, Default)]
struct Filters {
    name: String,
    category: CategoryFilter,
    path_prefix: Option<String>,
    source: SourceFilter,
}

impl Filters {
    fn active(&self) -> bool {
        !self.name.trim().is_empty()
            || self.category != CategoryFilter::All
            || self.path_prefix.is_some()
            || self.source != SourceFilter::All
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum MediaTab {
    #[default]
    Videos,
    Photos,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum CategoryScope {
    #[default]
    Video,
    Photo,
}

#[derive(Clone, PartialEq, Eq, Default)]
struct PhotoFilters {
    name: String,
    category: CategoryFilter,
}

impl PhotoFilters {
    fn active(&self) -> bool {
        !self.name.trim().is_empty() || self.category != CategoryFilter::All
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

pub struct App {
    config: Config,
    db_path: PathBuf,
    db: Db,
    videos: Vec<Video>,
    thumbs_dir: PathBuf,
    thumbs: Thumbs,
    pending_thumbs: HashSet<String>,
    thumb_regen_rx: Option<(String, Receiver<Result<String, String>>)>,
    player: Player,
    filters: Filters,
    applied_filters: Filters,
    filtered: Vec<usize>,
    filter_dirty: bool,
    search_mode: SearchMode,
    search_query: String,
    search_selected: usize,
    focus_id: Option<String>,
    grid_columns: usize,
    video_page: usize,
    video_page_size: usize,
    scroll_to_focus: bool,
    help_open: bool,
    selected: HashSet<String>,
    status: String,

    scan_rx: Option<Receiver<ScanEvent>>,
    scanning: bool,

    import_open: bool,
    import_text: String,
    import_log: Vec<String>,
    import_rx: Option<Receiver<ImportEvent>>,
    importing: bool,

    categories: Vec<Category>,
    category_paths: HashMap<String, String>,
    category_ids_by_path: HashMap<String, String>,
    category_children: HashMap<Option<String>, Vec<Category>>,
    video_categories: HashMap<String, Vec<(String, String)>>,
    category_targets: Vec<String>,
    category_filter: String,
    category_dialog: Option<CategoryDialog>,
    category_dialog_scope: CategoryScope,

    tab: MediaTab,
    photos: Vec<Photo>,
    photo_filters: PhotoFilters,
    photo_applied_filters: PhotoFilters,
    photo_filtered: Vec<usize>,
    photo_filter_dirty: bool,
    photo_selected: HashSet<String>,
    photo_focus_id: Option<String>,
    photo_grid_columns: usize,
    photo_page: usize,
    photo_page_size: usize,
    photo_scroll_to_focus: bool,
    photo_scan_rx: Option<Receiver<PhotoScanEvent>>,
    photo_scanning: bool,
    photo_thumbs: PhotoThumbs,
    photo_thumbs_dir: PathBuf,
    photo_pending_thumbs: HashSet<String>,
    photo_categories: Vec<Category>,
    photo_category_paths: HashMap<String, String>,
    photo_category_ids_by_path: HashMap<String, String>,
    photo_category_children: HashMap<Option<String>, Vec<Category>>,
    photo_assignments: HashMap<String, Vec<(String, String)>>,
    photo_category_targets: Vec<String>,
    photo_category_filter: String,

    play_quality: Quality,
    audio_only: bool,
    detached_playback: bool,
    playing_title: Option<String>,

    download_rx: Option<Receiver<DownloadEvent>>,
    downloading: Option<DownloadTask>,
    download_progress: Option<DownloadProgress>,
    download_queue: VecDeque<DownloadTask>,
    download_targets: Vec<String>,
    download_dir: String,
    download_dirs: Vec<String>,
    download_quality: Quality,
    download_errors: usize,
    download_failed: Vec<(DownloadTask, String)>,
    download_last_error: Option<String>,

    log: VecDeque<LogEntry>,
    log_open: bool,
    log_alert: bool,
    info_target: Option<(MediaTab, String)>,

    confirm_action: Option<ConfirmAction>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, config: Config, db_path: PathBuf, db: Db) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);

        let thumbs_dir = config::thumbs_dir();
        let photo_thumbs_dir = config::photo_thumbs_dir();
        let photo_thumb_width = config.photo_thumb_width;
        let thumbs = Thumbs::new(
            thumbs_dir.clone(),
            config.ffmpeg.clone(),
            config.thumb_width,
            2,
        );

        let player = Player::new(config.mpv.clone(), config.ytdl_format.clone());

        let mut app = Self {
            config,
            db_path,
            db,
            videos: Vec::new(),
            thumbs_dir,
            thumbs,
            pending_thumbs: HashSet::new(),
            thumb_regen_rx: None,
            player,
            filters: Filters::default(),
            applied_filters: Filters::default(),
            filtered: Vec::new(),
            filter_dirty: true,
            search_mode: SearchMode::Name,
            search_query: String::new(),
            search_selected: 0,
            focus_id: None,
            grid_columns: 1,
            video_page: 0,
            video_page_size: 1,
            scroll_to_focus: false,
            help_open: false,
            selected: HashSet::new(),
            status: String::new(),
            scan_rx: None,
            scanning: false,
            import_open: false,
            import_text: String::new(),
            import_log: Vec::new(),
            import_rx: None,
            importing: false,
            categories: Vec::new(),
            category_paths: HashMap::new(),
            category_ids_by_path: HashMap::new(),
            category_children: HashMap::new(),
            video_categories: HashMap::new(),
            category_targets: Vec::new(),
            category_filter: String::new(),
            category_dialog: None,
            category_dialog_scope: CategoryScope::Video,
            tab: MediaTab::Videos,
            photos: Vec::new(),
            photo_filters: PhotoFilters::default(),
            photo_applied_filters: PhotoFilters::default(),
            photo_filtered: Vec::new(),
            photo_filter_dirty: true,
            photo_selected: HashSet::new(),
            photo_focus_id: None,
            photo_grid_columns: 1,
            photo_page: 0,
            photo_page_size: 1,
            photo_scroll_to_focus: false,
            photo_scan_rx: None,
            photo_scanning: false,
            photo_thumbs: PhotoThumbs::new(photo_thumbs_dir.clone(), photo_thumb_width, 2),
            photo_thumbs_dir,
            photo_pending_thumbs: HashSet::new(),
            photo_categories: Vec::new(),
            photo_category_paths: HashMap::new(),
            photo_category_ids_by_path: HashMap::new(),
            photo_category_children: HashMap::new(),
            photo_assignments: HashMap::new(),
            photo_category_targets: Vec::new(),
            photo_category_filter: String::new(),
            play_quality: Quality::Preferred,
            audio_only: false,
            detached_playback: false,
            playing_title: None,
            download_rx: None,
            downloading: None,
            download_progress: None,
            download_queue: VecDeque::new(),
            download_targets: Vec::new(),
            download_dir: String::new(),
            download_dirs: Vec::new(),
            download_quality: Quality::Preferred,
            download_errors: 0,
            download_failed: Vec::new(),
            download_last_error: None,
            log: VecDeque::new(),
            log_open: false,
            log_alert: false,
            info_target: None,
            confirm_action: None,
        };

        app.reload();
        app.reload_photos();
        app
    }

    fn report_error(&mut self, source: &str, detail: impl std::fmt::Display) {
        self.push_log_entry(format!("[{source}] {detail}"), true);
        self.status = "Ocurrió un error (ver Registro)".to_string();
    }

    fn push_log(&mut self, text: impl std::fmt::Display) {
        self.push_log_entry(text.to_string(), false);
    }

    fn push_log_entry(&mut self, text: String, is_error: bool) {
        self.log.push_back(LogEntry { text, is_error });
        while self.log.len() > 1000 {
            self.log.pop_front();
        }
        if is_error {
            self.log_alert = true;
        }
    }

    fn poll_player_errors(&mut self) {
        if let Some(error) = self.player.take_last_error() {
            self.report_error("mpv", error);
        }
    }

    fn reload(&mut self) {
        match self.db.list_videos() {
            Ok(videos) => {
                self.videos = videos;
                self.selected
                    .retain(|id| self.videos.iter().any(|video| &video.id == id));
            }
            Err(error) => self.report_error("app", error),
        }
        self.reload_categories();
    }

    fn reload_categories(&mut self) {
        let categories = self.db.list_categories();
        let assignments = self.db.list_assignments();

        match (categories, assignments) {
            (Ok(categories), Ok(assignments)) => {
                self.category_paths = build_category_paths(&categories);
                self.category_ids_by_path = self
                    .category_paths
                    .iter()
                    .map(|(id, path)| (path.clone(), id.clone()))
                    .collect();
                self.category_children = build_category_children(&categories);
                self.categories = categories;

                if let CategoryFilter::Path(path) = &self.filters.category
                    && !self.category_ids_by_path.contains_key(path)
                {
                    self.filters.category = CategoryFilter::All;
                }

                let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
                for (video_id, category_id) in assignments {
                    if let Some(path) = self.category_paths.get(&category_id) {
                        map.entry(video_id)
                            .or_default()
                            .push((category_id, path.clone()));
                    }
                }
                for paths in map.values_mut() {
                    paths.sort_by(|left, right| left.1.cmp(&right.1));
                }
                self.video_categories = map;
            }
            (Err(error), _) | (_, Err(error)) => {
                self.report_error("app", error);
            }
        }

        self.filter_dirty = true;
    }

    fn reload_photos(&mut self) {
        match self.db.list_photos() {
            Ok(photos) => {
                self.photos = photos;
                self.photo_selected
                    .retain(|id| self.photos.iter().any(|photo| &photo.id == id));
            }
            Err(error) => self.report_error("app", error),
        }
        self.reload_photo_categories();
        self.photo_filter_dirty = true;
    }

    fn reload_photo_categories(&mut self) {
        let categories = self.db.list_photo_categories();
        let assignments = self.db.list_photo_assignments();

        match (categories, assignments) {
            (Ok(categories), Ok(assignments)) => {
                self.photo_category_paths = build_category_paths(&categories);
                self.photo_category_ids_by_path = self
                    .photo_category_paths
                    .iter()
                    .map(|(id, path)| (path.clone(), id.clone()))
                    .collect();
                self.photo_category_children = build_category_children(&categories);
                self.photo_categories = categories;

                if let CategoryFilter::Path(path) = &self.photo_filters.category
                    && !self.photo_category_ids_by_path.contains_key(path)
                {
                    self.photo_filters.category = CategoryFilter::All;
                }

                let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
                for (photo_id, category_id) in assignments {
                    if let Some(path) = self.photo_category_paths.get(&category_id) {
                        map.entry(photo_id)
                            .or_default()
                            .push((category_id, path.clone()));
                    }
                }
                for paths in map.values_mut() {
                    paths.sort_by(|left, right| left.1.cmp(&right.1));
                }
                self.photo_assignments = map;
            }
            (Err(error), _) | (_, Err(error)) => self.report_error("app", error),
        }

        self.photo_filter_dirty = true;
    }

    fn refresh_photo_filter(&mut self) {
        if !self.photo_filter_dirty && self.photo_applied_filters == self.photo_filters {
            return;
        }

        let mut patterns = Vec::new();
        for raw in self.photo_filters.name.split_whitespace() {
            patterns.push(Pattern::parse(
                raw,
                CaseMatching::Ignore,
                Normalization::Smart,
            ));
        }

        let filters_changed = self.photo_applied_filters != self.photo_filters;
        let mut matcher = Matcher::new(MatcherConfig::DEFAULT.match_paths());
        let mut buffer = Vec::new();
        let mut scored: Vec<(u32, usize)> = Vec::new();

        for (index, photo) in self.photos.iter().enumerate() {
            let category_paths: Vec<&str> = self
                .photo_assignments
                .get(&photo.id)
                .map(|entries| entries.iter().map(|(_, path)| path.as_str()).collect())
                .unwrap_or_default();
            if !self.photo_filters.category.matches(&category_paths) {
                continue;
            }

            let mut score = 0u32;
            let mut matched = true;
            for pattern in &patterns {
                match pattern.score(Utf32Str::new(&photo.title, &mut buffer), &mut matcher) {
                    Some(value) => score = score.saturating_add(value),
                    None => {
                        matched = false;
                        break;
                    }
                }
            }

            if matched {
                scored.push((score, index));
            }
        }

        scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        self.photo_filtered = scored.into_iter().map(|(_, index)| index).collect();

        if filters_changed {
            self.photo_selected.clear();
        }

        let focus_valid = self.photo_focus_id.as_ref().is_some_and(|id| {
            self.photo_filtered
                .iter()
                .any(|&index| &self.photos[index].id == id)
        });
        if !focus_valid {
            self.photo_focus_id = self
                .photo_filtered
                .first()
                .map(|&index| self.photos[index].id.clone());
        }

        self.photo_applied_filters = self.photo_filters.clone();
        self.photo_filter_dirty = false;
    }

    fn start_photo_scan(&mut self) {
        if self.photo_scanning {
            return;
        }

        let (tx, rx) = crossbeam_channel::unbounded();
        self.photo_scan_rx = Some(rx);
        self.photo_scanning = true;
        self.status = "Escaneando fotos…".to_string();

        scan::spawn_photos(
            self.config.pictures_root.clone(),
            self.config.photo_extensions.clone(),
            self.db_path.clone(),
            tx,
        );
    }

    fn poll_photo_scan(&mut self, ctx: &egui::Context) {
        if let Some(rx) = self.photo_scan_rx.clone() {
            let mut finished = None;

            loop {
                match rx.try_recv() {
                    Ok(PhotoScanEvent::Progress(found)) => {
                        self.status = format!("Escaneando fotos… {found} archivos");
                    }
                    Ok(PhotoScanEvent::Finished { found, error }) => {
                        finished = Some((found, error));
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = Some((0, Some("el escaneo de fotos se detuvo".to_string())));
                        break;
                    }
                }
            }

            if let Some((found, error)) = finished {
                self.photo_scanning = false;
                self.photo_scan_rx = None;
                self.reload_photos();
                match error {
                    Some(error) => self.report_error("fotos", error),
                    None => {
                        self.push_log(format!("Escaneo de fotos: {found} fotos"));
                        self.status = format!("{found} fotos encontradas");
                    }
                }
            }
        }

        if self.photo_scanning {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }

    fn poll_photo_thumbs(&mut self) {
        for id in self.photo_thumbs.poll() {
            self.photo_pending_thumbs.remove(&id);

            if let Some(key) = self.existing_photo_thumb_key(&id) {
                let _ = self.db.set_photo_thumb_key(&id, &key);
                if let Some(photo) = self.photos.iter_mut().find(|photo| photo.id == id) {
                    photo.thumb_key = Some(key);
                }
            }
        }
    }

    fn poll_thumb_regen(&mut self) {
        if let Some((id, rx)) = self.thumb_regen_rx.clone() {
            match rx.try_recv() {
                Ok(Ok(key)) => {
                    let _ = self.db.set_thumb_key(&id, &key);
                    if let Some(video) = self.videos.iter_mut().find(|video| video.id == id) {
                        video.thumb_key = Some(key);
                    }
                    self.status = "Miniatura regenerada".to_string();
                    self.thumb_regen_rx = None;
                }
                Ok(Err(error)) => {
                    self.report_error("yt-dlp", error);
                    self.thumb_regen_rx = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.report_error("yt-dlp", "la regeneración se interrumpió");
                    self.thumb_regen_rx = None;
                }
            }
        }
    }

    fn regenerate_thumb_ytdlp(&mut self, id: &str) {
        let Some(video) = self.videos.iter().find(|video| video.id == id) else {
            return;
        };
        let Some(url) = video.url.clone() else {
            self.report_error("yt-dlp", "el video no tiene URL");
            return;
        };

        self.remove_thumb_files(id, video.thumb_key.as_deref());
        if let Some(video) = self.videos.iter_mut().find(|video| video.id == id) {
            video.thumb_key = None;
        }

        let program = self.config.ytdlp.clone();
        let thumbs_dir = self.thumbs_dir.clone();
        let width = self.config.thumb_width;
        let id = id.to_string();
        let (tx, rx) = crossbeam_channel::unbounded();
        self.thumb_regen_rx = Some((id.clone(), rx));
        self.status = "Regenerando miniatura con yt-dlp…".to_string();
        std::thread::spawn(move || {
            let result = ytdlp::refresh_thumbnail(&program, &url, &id, &thumbs_dir, width)
                .map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
    }

    fn regenerate_thumb_ffmpeg(&mut self, id: &str) {
        let Some(video) = self.videos.iter().find(|video| video.id == id) else {
            return;
        };
        let Some(path) = video.path(&self.config.root) else {
            self.report_error("ffmpeg", "el video no tiene archivo local");
            return;
        };

        self.remove_thumb_files(id, video.thumb_key.as_deref());
        if let Some(video) = self.videos.iter_mut().find(|video| video.id == id) {
            video.thumb_key = None;
        }
        self.pending_thumbs.insert(id.to_string());
        self.thumbs.enqueue(id, &path);
        self.status = "Regenerando miniatura con ffmpeg…".to_string();
    }

    fn regenerate_photo_thumb(&mut self, id: &str) {
        let Some(photo) = self.photos.iter().find(|photo| photo.id == id) else {
            return;
        };
        let Some(path) = photo.path(&self.config.pictures_root) else {
            self.report_error("fotos", "la foto no tiene archivo local");
            return;
        };

        self.remove_photo_thumb_files(id, photo.thumb_key.as_deref());
        if let Some(photo) = self.photos.iter_mut().find(|photo| photo.id == id) {
            photo.thumb_key = None;
        }
        self.photo_pending_thumbs.insert(id.to_string());
        self.photo_thumbs.enqueue(id, &path);
        self.status = "Regenerando miniatura…".to_string();
    }

    fn existing_photo_thumb_key(&self, id: &str) -> Option<String> {
        let key = format!("{id}.jpg");
        self.photo_thumbs_dir.join(&key).is_file().then_some(key)
    }

    fn photo_thumb_path(&self, photo: &Photo) -> Option<PathBuf> {
        if let Some(key) = &photo.thumb_key
            && let Some(path) = thumbs::find(&self.photo_thumbs_dir, key)
        {
            return Some(path);
        }

        let key = self.existing_photo_thumb_key(&photo.id)?;
        Some(self.photo_thumbs_dir.join(key))
    }

    fn photo_selected_ids(&self) -> Vec<String> {
        self.photo_filtered
            .iter()
            .map(|&index| &self.photos[index])
            .filter(|photo| self.photo_selected.contains(&photo.id))
            .map(|photo| photo.id.clone())
            .collect()
    }

    fn current_photo_category_path(&self) -> Option<&str> {
        match &self.photo_filters.category {
            CategoryFilter::Path(path) => Some(path.as_str()),
            _ => None,
        }
    }

    fn current_photo_category_id(&self) -> Option<&str> {
        self.current_photo_category_path()
            .and_then(|path| self.photo_category_ids_by_path.get(path))
            .map(String::as_str)
    }

    fn current_photo_children(&self) -> Vec<Category> {
        let parent_id = match &self.photo_filters.category {
            CategoryFilter::All => None,
            CategoryFilter::Uncategorized => return Vec::new(),
            CategoryFilter::Path(_) => self.current_photo_category_id().map(str::to_string),
        };
        self.photo_category_children
            .get(&parent_id)
            .cloned()
            .unwrap_or_default()
    }

    fn set_photo_category_filter(&mut self, category: CategoryFilter) {
        self.photo_filters.category = category;
        self.photo_filter_dirty = true;
    }

    fn open_create_photo_category(&mut self) {
        self.category_dialog_scope = CategoryScope::Photo;
        self.category_dialog = Some(CategoryDialog::Create {
            parent_id: self.current_photo_category_id().map(str::to_string),
            parent_path: self.current_photo_category_path().map(str::to_string),
            name: String::new(),
        });
    }

    fn open_rename_photo_category(&mut self) {
        let (Some(id), Some(path)) = (
            self.current_photo_category_id().map(str::to_string),
            self.current_photo_category_path().map(str::to_string),
        ) else {
            return;
        };
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        self.category_dialog_scope = CategoryScope::Photo;
        self.category_dialog = Some(CategoryDialog::Rename {
            id,
            old_path: path,
            name,
        });
    }

    fn open_delete_photo_category(&mut self) {
        let (Some(id), Some(path)) = (
            self.current_photo_category_id().map(str::to_string),
            self.current_photo_category_path().map(str::to_string),
        ) else {
            return;
        };
        let (categories, photos) = self.photo_category_subtree_stats(&id);
        self.category_dialog_scope = CategoryScope::Photo;
        self.category_dialog = Some(CategoryDialog::Delete {
            id,
            path,
            categories,
            videos: photos,
        });
    }

    fn photo_category_subtree_stats(&self, id: &str) -> (usize, usize) {
        let mut ids = HashSet::new();
        let mut stack = vec![id.to_string()];
        while let Some(current) = stack.pop() {
            if !ids.insert(current.clone()) {
                continue;
            }
            if let Some(children) = self.photo_category_children.get(&Some(current)) {
                for child in children {
                    stack.push(child.id.clone());
                }
            }
        }

        let photos = self
            .photo_assignments
            .values()
            .filter(|entries| {
                entries
                    .iter()
                    .any(|(category_id, _)| ids.contains(category_id))
            })
            .count();
        (ids.len(), photos)
    }

    fn photo_targets(&self, ids: &[String]) -> Vec<VideoTarget> {
        ids.iter()
            .filter_map(|id| self.photos.iter().find(|photo| &photo.id == id))
            .map(|photo| VideoTarget {
                id: photo.id.clone(),
                title: photo.title.clone(),
                path: photo.path(&self.config.pictures_root),
                thumb_key: photo.thumb_key.clone(),
            })
            .collect()
    }

    fn request_photo_delete(&mut self, ids: &[String]) {
        let targets = self.photo_targets(ids);
        if targets.is_empty() {
            return;
        }
        self.confirm_action = Some(ConfirmAction::DeletePhotos(targets));
    }

    fn request_photo_remove(&mut self, ids: &[String]) {
        let targets = self.photo_targets(ids);
        if targets.is_empty() {
            return;
        }
        self.confirm_action = Some(ConfirmAction::RemovePhotoRows(targets));
    }

    fn remove_photo_thumb_files(&self, id: &str, thumb_key: Option<&str>) {
        if let Some(key) = thumb_key {
            let _ = fs::remove_file(self.photo_thumbs_dir.join(key));
        }
        let _ = fs::remove_file(self.photo_thumbs_dir.join(format!("{id}.jpg")));
    }

    fn open_photos(&mut self, ids: &[String]) {
        let mut paths = Vec::new();
        for id in ids {
            if let Some(photo) = self.photos.iter().find(|photo| &photo.id == id)
                && let Some(path) = photo.path(&self.config.pictures_root)
            {
                paths.push(path);
            }
        }

        if paths.is_empty() {
            self.report_error("visor", "no se encontraron archivos de imagen");
            return;
        }

        match Command::new(&self.config.image_viewer)
            .arg("-v")
            .args(&paths)
            .spawn()
        {
            Ok(_) => self.status = format!("Abriendo {} foto(s)…", paths.len()),
            Err(error) => self.report_error(
                "visor",
                format!("no se pudo abrir {}: {error}", self.config.image_viewer),
            ),
        }
    }

    fn start_scan(&mut self) {
        if self.scanning {
            return;
        }

        let (tx, rx) = crossbeam_channel::unbounded();
        self.scan_rx = Some(rx);
        self.scanning = true;
        self.status = "Escaneando…".to_string();

        scan::spawn(
            self.config.root.clone(),
            self.config.extensions.clone(),
            self.db_path.clone(),
            tx,
        );
    }

    fn poll_scan(&mut self, ctx: &egui::Context) {
        if let Some(rx) = self.scan_rx.clone() {
            let mut finished = None;

            loop {
                match rx.try_recv() {
                    Ok(ScanEvent::Progress(found)) => {
                        self.status = format!("Escaneando… {found} archivos");
                    }
                    Ok(ScanEvent::Finished { found, error }) => {
                        finished = Some((found, error));
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = Some((0, Some("el escaneo se detuvo".to_string())));
                        break;
                    }
                }
            }

            if let Some((found, error)) = finished {
                self.scanning = false;
                self.scan_rx = None;
                self.reload();
                match error {
                    Some(error) => self.report_error("scan", error),
                    None => {
                        self.push_log(format!("Escaneo: {found} videos"));
                        self.status = format!("{found} archivos encontrados");
                    }
                }
            }
        }

        if self.scanning {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }

    fn start_import(&mut self) {
        if self.importing {
            return;
        }

        let urls: Vec<String> = self
            .import_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_string)
            .collect();

        if urls.is_empty() {
            return;
        }

        self.import_log.clear();
        self.importing = true;
        self.status = "Importando…".to_string();

        let (tx, rx) = crossbeam_channel::unbounded();
        self.import_rx = Some(rx);

        ytdlp::spawn_import(
            urls,
            ImportOptions {
                program: self.config.ytdlp.clone(),
                thumb_width: self.config.thumb_width,
            },
            self.db_path.clone(),
            self.thumbs_dir.clone(),
            tx,
        );
    }

    fn poll_import(&mut self) {
        if let Some(rx) = self.import_rx.clone() {
            loop {
                match rx.try_recv() {
                    Ok(ImportEvent::Imported(title)) => {
                        self.import_log.push(format!("OK · {title}"));
                        self.push_log(format!("Importado: {title}"));
                    }
                    Ok(ImportEvent::Skipped(url)) => {
                        self.import_log.push(format!("Ya existe · {url}"));
                        self.push_log(format!("Ya existe: {url}"));
                    }
                    Ok(ImportEvent::Failed(error)) => {
                        self.report_error("yt-dlp", &error);
                        self.import_log.push(format!("Error · {error}"));
                    }
                    Ok(ImportEvent::Finished {
                        imported,
                        skipped,
                        failed,
                    }) => {
                        self.importing = false;
                        self.import_rx = None;
                        self.reload();
                        self.push_log(format!(
                            "Importación: {imported} importados · {skipped} repetidos · {failed} fallos"
                        ));
                        self.status = format!(
                            "{imported} importados · {skipped} repetidos · {failed} fallos"
                        );
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.importing = false;
                        self.import_rx = None;
                        self.report_error("yt-dlp", "conexión interrumpida");
                        self.status = "La importación se detuvo".to_string();
                        break;
                    }
                }
            }
        }
    }

    fn open_download(&mut self, ids: Vec<String>) {
        let targets: Vec<String> = ids
            .into_iter()
            .filter(|id| {
                self.videos.iter().any(|video| {
                    &video.id == id
                        && video.url.is_some()
                        && video.path(&self.config.root).is_none()
                })
            })
            .collect();

        if targets.is_empty() {
            self.status = "No hay videos descargables en la selección".to_string();
            return;
        }

        self.download_targets = targets;
        self.download_dirs = list_existing_dirs(&self.config.root);
        if !self.download_dir.is_empty() && !self.download_dirs.contains(&self.download_dir) {
            self.download_dir.clear();
        }
    }

    fn start_downloads(&mut self) {
        let mut tasks = Vec::new();
        for id in self.download_targets.clone() {
            let Some(video) = self.videos.iter().find(|video| video.id == id) else {
                continue;
            };
            let Some(url) = video.url.clone() else {
                continue;
            };
            tasks.push(DownloadTask {
                id: video.id.clone(),
                url,
                subdir: self.download_dir.clone(),
                format: self.download_quality.format(&self.config.ytdl_format),
            });
        }

        self.download_targets.clear();
        if tasks.is_empty() {
            self.status = "Nada que descargar".to_string();
            return;
        }

        if self.downloading.is_none() && self.download_queue.is_empty() {
            self.download_errors = 0;
            self.download_failed.clear();
            self.download_last_error = None;
            self.download_progress = None;
        }

        self.download_queue.extend(tasks);
        self.start_next_download();
    }

    fn start_next_download(&mut self) {
        if self.downloading.is_some() {
            return;
        }
        let Some(task) = self.download_queue.pop_front() else {
            return;
        };

        let title = self
            .videos
            .iter()
            .find(|video| video.id == task.id)
            .map(|video| video.title.clone())
            .unwrap_or_else(|| "video".to_string());
        let pending = self.download_queue.len();
        self.status = if pending > 0 {
            format!("Descargando «{title}» · {pending} en cola")
        } else {
            format!("Descargando «{title}»")
        };
        self.download_progress = None;
        self.downloading = Some(task.clone());

        let (tx, rx) = crossbeam_channel::unbounded();
        self.download_rx = Some(rx);

        ytdlp::spawn_download(
            task.id,
            task.url,
            ytdlp::DownloadOptions {
                program: self.config.ytdlp.clone(),
                format: task.format,
                root: self.config.root.clone(),
                subdir: PathBuf::from(task.subdir),
            },
            self.db_path.clone(),
            tx,
        );
    }

    fn retry_downloads(&mut self) {
        if self.download_failed.is_empty() {
            return;
        }

        for (task, _) in std::mem::take(&mut self.download_failed) {
            self.download_queue.push_back(task);
        }
        self.download_last_error = None;
        self.download_errors = 0;
        self.start_next_download();
    }

    fn dismiss_download_errors(&mut self) {
        self.download_failed.clear();
        self.download_last_error = None;
        self.download_errors = 0;
    }

    fn poll_download(&mut self) {
        if let Some(rx) = self.download_rx.clone() {
            loop {
                match rx.try_recv() {
                    Ok(DownloadEvent::Output(line)) => {
                        self.push_log(line);
                    }
                    Ok(DownloadEvent::Progress(progress)) => {
                        self.status = match progress.percent {
                            Some(percent) => format!("Descargando… {percent:.1}%"),
                            None => "Descargando…".to_string(),
                        };
                        self.download_progress = Some(progress);
                    }
                    Ok(DownloadEvent::Finished) => {
                        self.downloading = None;
                        self.download_progress = None;
                        self.download_rx = None;
                        self.reload();
                        if self.download_queue.is_empty() {
                            self.status = if self.download_errors > 0 {
                                format!(
                                    "Descargas terminadas con {} fallo(s)",
                                    self.download_errors
                                )
                            } else {
                                "Descarga terminada".to_string()
                            };
                        } else {
                            self.start_next_download();
                        }
                        break;
                    }
                    Ok(DownloadEvent::Failed(error)) => {
                        if let Some(task) = self.downloading.take() {
                            self.download_failed.push((task, error.clone()));
                        }
                        self.download_errors += 1;
                        self.report_error("yt-dlp", &error);
                        self.download_last_error = Some(error);
                        self.download_progress = None;
                        self.download_rx = None;
                        self.reload();
                        if self.download_queue.is_empty() {
                            self.status =
                                "Una descarga falló; podés reintentarla abajo".to_string();
                        } else {
                            self.start_next_download();
                        }
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if let Some(task) = self.downloading.take() {
                            let error = "conexión con yt-dlp interrumpida".to_string();
                            self.download_failed.push((task, error.clone()));
                            self.download_errors += 1;
                            self.report_error("yt-dlp", &error);
                            self.download_last_error = Some(error);
                        }
                        self.download_progress = None;
                        self.download_rx = None;
                        self.start_next_download();
                        break;
                    }
                }
            }
        }
    }

    fn poll_thumbs(&mut self) {
        for id in self.thumbs.poll() {
            self.pending_thumbs.remove(&id);

            if let Some(key) = self.existing_thumb_key(&id) {
                let _ = self.db.set_thumb_key(&id, &key);
                if let Some(video) = self.videos.iter_mut().find(|video| video.id == id) {
                    video.thumb_key = Some(key);
                }
            }
        }
    }

    fn existing_thumb_key(&self, id: &str) -> Option<String> {
        ["jpg", "png", "webp"].iter().find_map(|extension| {
            let key = format!("{id}.{extension}");
            self.thumbs_dir.join(&key).is_file().then_some(key)
        })
    }

    fn thumb_path(&self, video: &Video) -> Option<PathBuf> {
        if let Some(key) = &video.thumb_key
            && let Some(path) = thumbs::find(&self.thumbs_dir, key)
        {
            return Some(path);
        }

        let key = self.existing_thumb_key(&video.id)?;
        Some(self.thumbs_dir.join(key))
    }

    fn refresh_filter(&mut self) {
        if !self.filter_dirty && self.applied_filters == self.filters {
            return;
        }

        let mut patterns = Vec::new();
        for raw in self.filters.name.split_whitespace() {
            patterns.push(Pattern::parse(
                raw,
                CaseMatching::Ignore,
                Normalization::Smart,
            ));
        }

        let filters_changed = self.applied_filters != self.filters;
        let mut matcher = Matcher::new(MatcherConfig::DEFAULT.match_paths());
        let mut buffer = Vec::new();
        let mut scored: Vec<(u32, usize)> = Vec::new();

        for (index, video) in self.videos.iter().enumerate() {
            let local = video.path(&self.config.root).is_some();
            let online = video.url.is_some();
            if !self.filters.source.matches(local, online) {
                continue;
            }

            if let Some(prefix) = &self.filters.path_prefix {
                let Some(rel_path) = &video.rel_path else {
                    continue;
                };
                if !path_matches_prefix(rel_path, prefix) {
                    continue;
                }
            }

            let category_paths: Vec<&str> = self
                .video_categories
                .get(&video.id)
                .map(|entries| entries.iter().map(|(_, path)| path.as_str()).collect())
                .unwrap_or_default();
            if !self.filters.category.matches(&category_paths) {
                continue;
            }

            let mut score = 0u32;
            let mut matched = true;
            for pattern in &patterns {
                match pattern.score(Utf32Str::new(&video.title, &mut buffer), &mut matcher) {
                    Some(value) => score = score.saturating_add(value),
                    None => {
                        matched = false;
                        break;
                    }
                }
            }

            if matched {
                scored.push((score, index));
            }
        }

        scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        self.filtered = scored.into_iter().map(|(_, index)| index).collect();

        if filters_changed {
            self.selected.clear();
        }

        let focus_valid = self.focus_id.as_ref().is_some_and(|id| {
            self.filtered
                .iter()
                .any(|&index| &self.videos[index].id == id)
        });
        if !focus_valid {
            self.focus_id = self
                .filtered
                .first()
                .map(|&index| self.videos[index].id.clone());
        }

        self.applied_filters = self.filters.clone();
        self.filter_dirty = false;
    }

    fn modal_open(&self) -> bool {
        self.help_open
            || self.import_open
            || !self.category_targets.is_empty()
            || !self.photo_category_targets.is_empty()
            || !self.download_targets.is_empty()
            || self.confirm_action.is_some()
            || self.category_dialog.is_some()
            || self.info_target.is_some()
            || self.log_open
    }

    fn close_modals(&mut self) {
        self.help_open = false;
        self.import_open = false;
        self.category_targets.clear();
        self.photo_category_targets.clear();
        self.download_targets.clear();
        self.confirm_action = None;
        self.category_dialog = None;
        self.info_target = None;
        self.log_open = false;
    }

    fn focus_search(&self, ctx: &egui::Context) {
        let id = egui::Id::unique(NAME_FILTER_ID);
        ctx.memory_mut(|memory| memory.request_focus(id));
    }

    fn set_search_mode(&mut self, mode: SearchMode) {
        if self.search_mode == mode {
            return;
        }
        self.search_mode = mode;
        self.search_query.clear();
        self.search_selected = 0;
        if !self.filters.name.is_empty() {
            self.filters.name.clear();
            self.filter_dirty = true;
        }
    }

    fn focus_search_for_name(&mut self, ctx: &egui::Context) {
        self.set_search_mode(SearchMode::Name);
        self.focus_search(ctx);
    }

    fn current_category_path(&self) -> Option<&str> {
        match &self.filters.category {
            CategoryFilter::Path(path) => Some(path.as_str()),
            _ => None,
        }
    }

    fn current_category_id(&self) -> Option<&str> {
        self.current_category_path()
            .and_then(|path| self.category_ids_by_path.get(path))
            .map(String::as_str)
    }

    fn current_children(&self) -> Vec<Category> {
        let parent_id = match &self.filters.category {
            CategoryFilter::All => None,
            CategoryFilter::Uncategorized => return Vec::new(),
            CategoryFilter::Path(_) => self.current_category_id().map(str::to_string),
        };
        self.category_children
            .get(&parent_id)
            .cloned()
            .unwrap_or_default()
    }

    fn set_category_filter(&mut self, category: CategoryFilter) {
        self.filters.category = category;
        self.filters.path_prefix = None;
        self.filter_dirty = true;
    }

    fn navigate_to_category(&mut self, path: String) {
        self.filters = Filters {
            name: String::new(),
            category: CategoryFilter::Path(path),
            path_prefix: None,
            source: SourceFilter::All,
        };
        self.search_query.clear();
        self.search_selected = 0;
        self.filter_dirty = true;
    }

    fn navigate_to_path(&mut self, prefix: String) {
        self.filters = Filters {
            name: String::new(),
            category: CategoryFilter::All,
            path_prefix: Some(prefix),
            source: SourceFilter::All,
        };
        self.search_query.clear();
        self.search_selected = 0;
        self.filter_dirty = true;
    }

    fn clear_filters_and_search(&mut self) {
        self.filters.clear();
        self.search_query.clear();
        self.search_selected = 0;
        self.filter_dirty = true;
    }

    fn search_results(&self) -> Vec<(String, String)> {
        let query = self.search_query.trim();
        if query.is_empty() {
            return Vec::new();
        }

        match self.search_mode {
            SearchMode::Name => Vec::new(),
            SearchMode::Category => {
                let entries = self
                    .category_paths
                    .values()
                    .map(|path| (path.clone(), path.clone()))
                    .collect();
                rank_search(query, entries)
            }
            SearchMode::Path => {
                let mut folders: HashSet<String> = HashSet::new();
                for video in &self.videos {
                    if let Some(rel_path) = &video.rel_path {
                        folders.insert(path_parent(rel_path).unwrap_or_default());
                    }
                }
                let entries = folders
                    .into_iter()
                    .map(|path| (path_search_label(&path), path))
                    .collect();
                rank_search(query, entries)
            }
        }
    }

    fn apply_search_target(&mut self, target: String) {
        match self.search_mode {
            SearchMode::Category => self.navigate_to_category(target),
            SearchMode::Path => self.navigate_to_path(target),
            SearchMode::Name => {}
        }
    }

    fn apply_search_selection(&mut self) {
        let Some(target) = self
            .search_results()
            .get(self.search_selected)
            .map(|(_, target)| target.clone())
        else {
            return;
        };
        self.apply_search_target(target);
    }

    fn category_subtree_stats(&self, id: &str) -> (usize, usize) {
        let mut ids = HashSet::new();
        let mut stack = vec![id.to_string()];
        while let Some(current) = stack.pop() {
            if !ids.insert(current.clone()) {
                continue;
            }
            if let Some(children) = self.category_children.get(&Some(current)) {
                for child in children {
                    stack.push(child.id.clone());
                }
            }
        }

        let videos = self
            .video_categories
            .values()
            .filter(|entries| {
                entries
                    .iter()
                    .any(|(category_id, _)| ids.contains(category_id))
            })
            .count();
        (ids.len(), videos)
    }

    fn category_selection_state(&self, targets: &[String], category_id: &str) -> CategorySelection {
        let flags: Vec<bool> = targets
            .iter()
            .map(|target| {
                self.video_categories
                    .get(target)
                    .is_some_and(|entries| entries.iter().any(|(id, _)| id == category_id))
            })
            .collect();
        selection_state(&flags)
    }

    fn open_create_category(&mut self) {
        self.category_dialog_scope = CategoryScope::Video;
        self.category_dialog = Some(CategoryDialog::Create {
            parent_id: self.current_category_id().map(str::to_string),
            parent_path: self.current_category_path().map(str::to_string),
            name: String::new(),
        });
    }

    fn open_rename_category(&mut self) {
        self.category_dialog_scope = CategoryScope::Video;
        let (Some(id), Some(path)) = (
            self.current_category_id().map(str::to_string),
            self.current_category_path().map(str::to_string),
        ) else {
            return;
        };
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        self.category_dialog = Some(CategoryDialog::Rename {
            id,
            old_path: path,
            name,
        });
    }

    fn open_delete_category(&mut self) {
        self.category_dialog_scope = CategoryScope::Video;
        let (Some(id), Some(path)) = (
            self.current_category_id().map(str::to_string),
            self.current_category_path().map(str::to_string),
        ) else {
            return;
        };
        let (categories, videos) = self.category_subtree_stats(&id);
        self.category_dialog = Some(CategoryDialog::Delete {
            id,
            path,
            categories,
            videos,
        });
    }

    fn submit_category_dialog(&mut self) {
        if self.category_dialog_scope == CategoryScope::Photo {
            self.submit_photo_category_dialog();
            return;
        }

        let Some(dialog) = self.category_dialog.take() else {
            return;
        };

        match dialog {
            CategoryDialog::Create {
                parent_id,
                parent_path,
                name,
            } => match self.db.create_category(parent_id.as_deref(), &name) {
                Ok(_) => {
                    let path = parent_path
                        .map(|parent| format!("{parent}/{}", name.trim()))
                        .unwrap_or_else(|| name.trim().to_string());
                    self.reload_categories();
                    self.set_category_filter(CategoryFilter::Path(path.clone()));
                    self.status = format!("Categoría creada: {path}");
                }
                Err(error) => {
                    self.report_error("app", &error);
                    self.category_dialog = Some(CategoryDialog::Create {
                        parent_id,
                        parent_path,
                        name,
                    });
                }
            },
            CategoryDialog::Rename { id, old_path, name } => {
                match self.db.rename_category(&id, &name) {
                    Ok(()) => {
                        let new_path = match path_parent(&old_path) {
                            Some(parent) => format!("{parent}/{}", name.trim()),
                            None => name.trim().to_string(),
                        };
                        let filter_path = self.current_category_path().map(str::to_string);
                        self.reload_categories();
                        if let Some(path) = filter_path {
                            let updated = if path == old_path {
                                Some(new_path.clone())
                            } else {
                                path.strip_prefix(&format!("{old_path}/"))
                                    .map(|rest| format!("{new_path}/{rest}"))
                            };
                            if let Some(updated) = updated {
                                self.filters.category = CategoryFilter::Path(updated);
                            }
                        }
                        self.filter_dirty = true;
                        self.status = format!("Categoría renombrada: {new_path}");
                    }
                    Err(error) => {
                        self.report_error("app", &error);
                        self.category_dialog = Some(CategoryDialog::Rename { id, old_path, name });
                    }
                }
            }
            CategoryDialog::Delete {
                id,
                path,
                categories,
                videos,
            } => match self.db.delete_category_subtree(&id) {
                Ok(()) => {
                    self.reload_categories();
                    let parent = path_parent(&path)
                        .map(CategoryFilter::Path)
                        .unwrap_or(CategoryFilter::All);
                    self.set_category_filter(parent);
                    self.status = format!(
                        "Categoría eliminada: {categories} categoría(s), {videos} video(s)"
                    );
                }
                Err(error) => self.report_error("app", error),
            },
        }
    }

    fn submit_photo_category_dialog(&mut self) {
        let Some(dialog) = self.category_dialog.take() else {
            return;
        };

        match dialog {
            CategoryDialog::Create {
                parent_id,
                parent_path,
                name,
            } => match self.db.create_photo_category(parent_id.as_deref(), &name) {
                Ok(_) => {
                    let path = parent_path
                        .map(|parent| format!("{parent}/{}", name.trim()))
                        .unwrap_or_else(|| name.trim().to_string());
                    self.reload_photo_categories();
                    self.set_photo_category_filter(CategoryFilter::Path(path.clone()));
                    self.status = format!("Categoría de fotos creada: {path}");
                }
                Err(error) => {
                    self.report_error("app", &error);
                    self.category_dialog = Some(CategoryDialog::Create {
                        parent_id,
                        parent_path,
                        name,
                    });
                }
            },
            CategoryDialog::Rename { id, old_path, name } => {
                match self.db.rename_photo_category(&id, &name) {
                    Ok(()) => {
                        let new_path = match path_parent(&old_path) {
                            Some(parent) => format!("{parent}/{}", name.trim()),
                            None => name.trim().to_string(),
                        };
                        let filter_path = self.current_photo_category_path().map(str::to_string);
                        self.reload_photo_categories();
                        if let Some(path) = filter_path {
                            let updated = if path == old_path {
                                Some(new_path.clone())
                            } else {
                                path.strip_prefix(&format!("{old_path}/"))
                                    .map(|rest| format!("{new_path}/{rest}"))
                            };
                            if let Some(updated) = updated {
                                self.photo_filters.category = CategoryFilter::Path(updated);
                            }
                        }
                        self.photo_filter_dirty = true;
                        self.status = format!("Categoría de fotos renombrada: {new_path}");
                    }
                    Err(error) => {
                        self.report_error("app", &error);
                        self.category_dialog = Some(CategoryDialog::Rename { id, old_path, name });
                    }
                }
            }
            CategoryDialog::Delete {
                id,
                path,
                categories,
                videos,
            } => match self.db.delete_photo_category_subtree(&id) {
                Ok(()) => {
                    self.reload_photo_categories();
                    let parent = path_parent(&path)
                        .map(CategoryFilter::Path)
                        .unwrap_or(CategoryFilter::All);
                    self.set_photo_category_filter(parent);
                    self.status = format!(
                        "Categoría de fotos eliminada: {categories} categoría(s), {videos} foto(s)"
                    );
                }
                Err(error) => self.report_error("app", error),
            },
        }
    }

    fn photo_category_selection_state(
        &self,
        targets: &[String],
        category_id: &str,
    ) -> CategorySelection {
        let flags: Vec<bool> = targets
            .iter()
            .map(|target| {
                self.photo_assignments
                    .get(target)
                    .is_some_and(|entries| entries.iter().any(|(id, _)| id == category_id))
            })
            .collect();
        selection_state(&flags)
    }

    fn focused_filtered_index(&self) -> Option<usize> {
        let id = self.focus_id.as_ref()?;
        self.filtered
            .iter()
            .position(|&index| &self.videos[index].id == id)
    }

    fn ensure_focus(&mut self) {
        if self.focus_id.is_none()
            && let Some(&index) = self.filtered.first()
        {
            self.focus_id = Some(self.videos[index].id.clone());
        }
    }

    fn move_focus_horizontal(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            self.focus_id = None;
            return;
        }

        let Some(current) = self.focused_filtered_index() else {
            self.ensure_focus();
            self.scroll_to_focus = true;
            return;
        };

        let next = (current as isize + delta).clamp(0, self.filtered.len() as isize - 1) as usize;
        self.set_focus_index(next);
    }

    fn move_focus_vertical(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            self.focus_id = None;
            return;
        }

        let Some(current) = self.focused_filtered_index() else {
            self.ensure_focus();
            self.scroll_to_focus = true;
            return;
        };

        let Some(next) = vertical_focus_target(
            current,
            self.grid_columns.max(1),
            delta,
            self.filtered.len(),
        ) else {
            return;
        };
        self.set_focus_index(next);
    }

    fn set_focus_index(&mut self, index: usize) {
        self.focus_id = Some(self.videos[self.filtered[index]].id.clone());
        self.scroll_to_focus = true;
    }

    fn toggle_focus_selection(&mut self) {
        self.ensure_focus();
        let Some(id) = self.focus_id.clone() else {
            return;
        };
        if !self.selected.remove(&id) {
            self.selected.insert(id);
        }
    }

    fn play_focus_or_selection(&mut self) {
        let ids = if self.selected.len() > 1 {
            self.selected_ids()
        } else if let Some(id) = &self.focus_id {
            vec![id.clone()]
        } else {
            self.selected_ids()
        };

        if !ids.is_empty() {
            self.open_play(ids);
        }
    }

    fn escape_action(&mut self) {
        if !self.selected.is_empty() {
            self.selected.clear();
            return;
        }
        if self.filters.active() {
            self.filters.clear();
            self.filter_dirty = true;
            return;
        }
        self.focus_id = None;
    }

    fn handle_keyboard(&mut self, ctx: &egui::Context) {
        let search_id = egui::Id::unique(NAME_FILTER_ID);

        if self.modal_open() {
            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                self.close_modals();
            }
            return;
        }

        if self.tab == MediaTab::Photos {
            self.handle_photo_keyboard(ctx);
            return;
        }

        if ctx.memory(|memory| memory.has_focus(search_id)) {
            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                if self.search_query.is_empty() {
                    ctx.memory_mut(|memory| memory.surrender_focus(search_id));
                } else {
                    self.search_query.clear();
                    self.search_selected = 0;
                    if self.search_mode == SearchMode::Name {
                        self.filters.name.clear();
                        self.filter_dirty = true;
                    }
                }
                return;
            }

            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Enter)) {
                if self.search_mode == SearchMode::Name {
                    ctx.memory_mut(|memory| memory.surrender_focus(search_id));
                    self.ensure_focus();
                    self.play_focus_or_selection();
                } else {
                    self.apply_search_selection();
                }
                return;
            }

            if self.search_mode != SearchMode::Name {
                let results_len = self.search_results().len();
                if results_len > 0 {
                    if ctx
                        .input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::ArrowDown))
                    {
                        self.search_selected = (self.search_selected + 1).min(results_len - 1);
                        return;
                    }
                    if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::ArrowUp))
                    {
                        self.search_selected = self.search_selected.saturating_sub(1);
                        return;
                    }
                }
            }
            return;
        }

        if let Some(focused) = ctx.memory(|memory| memory.focused()) {
            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                ctx.memory_mut(|memory| memory.surrender_focus(focused));
            }
            return;
        }

        if ctx.any_popup_open() {
            return;
        }

        let page_next = ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::SHIFT, Key::ArrowRight)
                || input.consume_key(egui::Modifiers::SHIFT, Key::L)
        });
        let page_prev = ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::SHIFT, Key::ArrowLeft)
                || input.consume_key(egui::Modifiers::SHIFT, Key::H)
        });
        if page_next {
            self.video_change_page(1);
            return;
        }
        if page_prev {
            self.video_change_page(-1);
            return;
        }

        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::K)) {
            self.focus_search(ctx);
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Slash)) {
            self.focus_search_for_name(ctx);
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Questionmark)) {
            self.help_open = true;
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            self.escape_action();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Enter)) {
            self.ensure_focus();
            self.play_focus_or_selection();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Space)) {
            self.toggle_focus_selection();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::I)) {
            if let Some(id) = self.focus_id.clone() {
                self.open_info(MediaTab::Videos, &id);
            }
            return;
        }

        let down = ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::NONE, Key::J)
                || input.consume_key(egui::Modifiers::NONE, Key::ArrowDown)
        });
        let up = ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::NONE, Key::K)
                || input.consume_key(egui::Modifiers::NONE, Key::ArrowUp)
        });
        let right = ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::NONE, Key::L)
                || input.consume_key(egui::Modifiers::NONE, Key::ArrowRight)
        });
        let left = ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::NONE, Key::H)
                || input.consume_key(egui::Modifiers::NONE, Key::ArrowLeft)
        });

        if down {
            self.move_focus_vertical(1);
        } else if up {
            self.move_focus_vertical(-1);
        } else if right {
            self.move_focus_horizontal(1);
        } else if left {
            self.move_focus_horizontal(-1);
        }
    }

    fn handle_photo_keyboard(&mut self, ctx: &egui::Context) {
        let search_id = egui::Id::unique(NAME_FILTER_ID);

        if ctx.memory(|memory| memory.has_focus(search_id)) {
            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                if self.photo_filters.name.is_empty() {
                    ctx.memory_mut(|memory| memory.surrender_focus(search_id));
                } else {
                    self.photo_filters.name.clear();
                    self.photo_filter_dirty = true;
                }
                return;
            }
            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Enter)) {
                ctx.memory_mut(|memory| memory.surrender_focus(search_id));
                self.open_photo_focus();
            }
            return;
        }

        if let Some(focused) = ctx.memory(|memory| memory.focused()) {
            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                ctx.memory_mut(|memory| memory.surrender_focus(focused));
            }
            return;
        }

        if ctx.any_popup_open() {
            return;
        }

        let page_next = ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::SHIFT, Key::ArrowRight)
                || input.consume_key(egui::Modifiers::SHIFT, Key::L)
        });
        let page_prev = ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::SHIFT, Key::ArrowLeft)
                || input.consume_key(egui::Modifiers::SHIFT, Key::H)
        });
        if page_next {
            self.photo_change_page(1);
            return;
        }
        if page_prev {
            self.photo_change_page(-1);
            return;
        }

        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::K)) {
            self.focus_search(ctx);
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Slash)) {
            self.focus_search(ctx);
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Questionmark)) {
            self.help_open = true;
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            self.photo_filters.clear();
            self.photo_filter_dirty = true;
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Enter)) {
            self.open_photo_focus();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Space)) {
            if let Some(id) = self.photo_focus_id.clone()
                && !self.photo_selected.remove(&id)
            {
                self.photo_selected.insert(id);
            }
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::I)) {
            if let Some(id) = self.photo_focus_id.clone() {
                self.open_info(MediaTab::Photos, &id);
            }
            return;
        }

        let down = ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::NONE, Key::J)
                || input.consume_key(egui::Modifiers::NONE, Key::ArrowDown)
        });
        let up = ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::NONE, Key::K)
                || input.consume_key(egui::Modifiers::NONE, Key::ArrowUp)
        });
        let right = ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::NONE, Key::L)
                || input.consume_key(egui::Modifiers::NONE, Key::ArrowRight)
        });
        let left = ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::NONE, Key::H)
                || input.consume_key(egui::Modifiers::NONE, Key::ArrowLeft)
        });

        if down {
            self.photo_move_vertical(1);
        } else if up {
            self.photo_move_vertical(-1);
        } else if right {
            self.photo_move_horizontal(1);
        } else if left {
            self.photo_move_horizontal(-1);
        }
    }

    fn photo_focus_index(&self) -> Option<usize> {
        let id = self.photo_focus_id.as_ref()?;
        self.photo_filtered
            .iter()
            .position(|&index| &self.photos[index].id == id)
    }

    fn set_photo_focus(&mut self, index: usize) {
        self.photo_focus_id = self
            .photo_filtered
            .get(index)
            .map(|&index| self.photos[index].id.clone());
        self.photo_scroll_to_focus = true;
    }

    fn photo_move_vertical(&mut self, delta: isize) {
        let Some(index) = self.photo_focus_index() else {
            return;
        };
        if let Some(target) = vertical_focus_target(
            index,
            self.photo_grid_columns,
            delta,
            self.photo_filtered.len(),
        ) {
            self.set_photo_focus(target);
        }
    }

    fn photo_move_horizontal(&mut self, delta: isize) {
        let Some(index) = self.photo_focus_index() else {
            return;
        };
        let target = index as isize + delta;
        if target >= 0 && (target as usize) < self.photo_filtered.len() {
            self.set_photo_focus(target as usize);
        }
    }

    fn video_change_page(&mut self, delta: isize) {
        let page_size = self.video_page_size.max(1);
        let page_count = self.filtered.len().div_ceil(page_size).max(1);
        let page = (self.video_page as isize + delta).clamp(0, page_count as isize - 1) as usize;
        self.video_page = page;
        if let Some(&index) = self.filtered.get(page * page_size) {
            self.focus_id = Some(self.videos[index].id.clone());
        }
    }

    fn photo_change_page(&mut self, delta: isize) {
        let page_size = self.photo_page_size.max(1);
        let page_count = self.photo_filtered.len().div_ceil(page_size).max(1);
        let page = (self.photo_page as isize + delta).clamp(0, page_count as isize - 1) as usize;
        self.photo_page = page;
        if let Some(&index) = self.photo_filtered.get(page * page_size) {
            self.photo_focus_id = Some(self.photos[index].id.clone());
        }
    }

    fn open_photo_focus(&mut self) {
        let Some(id) = self.photo_focus_id.clone() else {
            return;
        };
        let ids = self.photo_viewer_targets(&id);
        self.open_photos(&ids);
    }

    fn play_index(&mut self, index: usize) {
        let Some(id) = self.videos.get(index).map(|video| video.id.clone()) else {
            return;
        };
        self.play_ids(std::slice::from_ref(&id), self.play_quality);
    }

    fn play_ids(&mut self, ids: &[String], quality: Quality) {
        let mut targets = Vec::new();
        for id in ids {
            let Some(video) = self.videos.iter().find(|video| &video.id == id) else {
                continue;
            };

            if let Some(path) = video.path(&self.config.root) {
                targets.push(path.to_string_lossy().to_string());
            } else if let Some(url) = &video.url {
                targets.push(url.clone());
            }
        }

        if targets.is_empty() {
            self.status = "No hay nada reproducible en la selección".to_string();
            return;
        }

        let format = quality.format(&self.config.ytdl_format);
        let count = targets.len();
        self.playing_title = if ids.len() == 1 {
            self.videos
                .iter()
                .find(|video| video.id == ids[0])
                .map(|video| video.title.clone())
        } else {
            Some(format!("{count} videos"))
        };

        if self.detached_playback {
            match spawn_detached(&self.config.mpv, &targets, Some(&format), self.audio_only) {
                Ok(()) => {
                    self.status = if count > 1 {
                        format!("Reproduciendo {count} videos (ventana aparte)")
                    } else {
                        "Reproduciendo (ventana aparte)".to_string()
                    };
                }
                Err(error) => self.report_error("mpv", error),
            }
            return;
        }

        match self
            .player
            .play_playlist(&targets, Some(&format), self.audio_only)
        {
            Ok(()) => {
                let suffix = if self.audio_only { " (solo audio)" } else { "" };
                self.status = if count > 1 {
                    format!("Reproduciendo {count} videos{suffix}")
                } else {
                    format!("Reproduciendo{suffix}")
                };
            }
            Err(error) => self.report_error("mpv", error),
        }
    }

    fn open_play(&mut self, ids: Vec<String>) {
        self.play_ids(&ids, self.play_quality);
    }

    fn selected_ids(&self) -> Vec<String> {
        self.filtered
            .iter()
            .filter_map(|&index| {
                let video = &self.videos[index];
                self.selected.contains(&video.id).then(|| video.id.clone())
            })
            .collect()
    }

    fn target_for(&self, video: &Video) -> VideoTarget {
        VideoTarget {
            id: video.id.clone(),
            title: video.title.clone(),
            path: video.path(&self.config.root),
            thumb_key: video.thumb_key.clone(),
        }
    }

    fn request_delete(&mut self, ids: &[String]) {
        let targets: Vec<VideoTarget> = ids
            .iter()
            .filter_map(|id| self.videos.iter().find(|video| &video.id == id))
            .map(|video| self.target_for(video))
            .collect();

        if targets.is_empty() {
            self.status = "No hay elementos en la selección".to_string();
            return;
        }
        self.confirm_action = Some(ConfirmAction::DeleteFiles(targets));
    }

    fn request_remove(&mut self, ids: &[String]) {
        let targets: Vec<VideoTarget> = ids
            .iter()
            .filter_map(|id| self.videos.iter().find(|video| &video.id == id))
            .map(|video| self.target_for(video))
            .collect();

        if targets.is_empty() {
            return;
        }
        self.confirm_action = Some(ConfirmAction::RemoveRows(targets));
    }

    fn handle_card_action(&mut self, index: usize, action: CardAction) {
        let Some(video) = self.videos.get(index) else {
            return;
        };

        match action {
            CardAction::Play => self.play_index(index),
            CardAction::Download => {
                let id = video.id.clone();
                self.open_download(vec![id]);
            }
            CardAction::Categories => {
                self.category_targets = vec![video.id.clone()];
                self.category_filter.clear();
            }
            CardAction::Info => {
                let id = video.id.clone();
                self.open_info(MediaTab::Videos, &id);
            }
            CardAction::RegenerateYtdlp => {
                let id = video.id.clone();
                self.regenerate_thumb_ytdlp(&id);
            }
            CardAction::RegenerateFfmpeg => {
                let id = video.id.clone();
                self.regenerate_thumb_ffmpeg(&id);
            }
            CardAction::UnlinkUrl => {
                let id = video.id.clone();
                match self.db.clear_url(&id) {
                    Ok(()) => {
                        self.reload();
                        self.status = "URL desvinculada".to_string();
                    }
                    Err(error) => self.report_error("app", error),
                }
            }
            CardAction::Delete => {
                let id = video.id.clone();
                self.request_delete(&[id]);
            }
            CardAction::RemoveRow => {
                let id = video.id.clone();
                self.request_remove(&[id]);
            }
            CardAction::SetPlayQuality(quality) => {
                self.play_quality = quality;
                self.status = format!("Calidad de reproducción: {}", quality.label());
            }
        }
    }

    fn remove_thumb_files(&self, id: &str, thumb_key: Option<&str>) {
        if let Some(key) = thumb_key {
            let _ = fs::remove_file(self.thumbs_dir.join(key));
        }
        for extension in ["jpg", "png", "webp"] {
            let _ = fs::remove_file(self.thumbs_dir.join(format!("{id}.{extension}")));
        }
    }

    fn search_results_popup(&mut self, ui: &mut egui::Ui, anchor: egui::Rect) {
        let results = self.search_results();
        let width = anchor.width().max(360.0);
        let mut chosen = None;
        let selected = self.search_selected.min(results.len().saturating_sub(1));

        egui::Area::new(egui::Id::unique("search-results-popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor.left_bottom() + egui::vec2(0.0, 4.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(width);
                    if results.is_empty() {
                        ui.label(RichText::new("Sin resultados").small().weak());
                        return;
                    }

                    egui::ScrollArea::vertical()
                        .max_height(280.0)
                        .show(ui, |ui| {
                            for (index, (label, target)) in results.iter().enumerate() {
                                let response =
                                    ui.selectable_label(index == selected, label.as_str());
                                if index == selected {
                                    response.scroll_to_me(Some(Align::Center));
                                }
                                if response.clicked() {
                                    chosen = Some(target.clone());
                                }
                            }
                        });
                });
            });

        if let Some(target) = chosen {
            self.apply_search_target(target);
        }
    }

    fn category_menu_contents(&mut self, ui: &mut egui::Ui, scope: CategoryScope) {
        let (current_path, current_id, is_all, is_uncategorized, children) = match scope {
            CategoryScope::Video => (
                self.current_category_path().map(str::to_string),
                self.current_category_id().map(str::to_string),
                self.filters.category == CategoryFilter::All,
                self.filters.category == CategoryFilter::Uncategorized,
                self.current_children(),
            ),
            CategoryScope::Photo => (
                self.current_photo_category_path().map(str::to_string),
                self.current_photo_category_id().map(str::to_string),
                self.photo_filters.category == CategoryFilter::All,
                self.photo_filters.category == CategoryFilter::Uncategorized,
                self.current_photo_children(),
            ),
        };

        let mut category_click = None;
        let mut create = false;
        let mut rename = false;
        let mut delete = false;

        if ui.selectable_label(is_all, "Inicio").clicked() {
            category_click = Some(CategoryFilter::All);
        }
        if let Some(path) = &current_path {
            let mut branch = String::new();
            for segment in path.split('/') {
                if !branch.is_empty() {
                    branch.push('/');
                }
                branch.push_str(segment);
                if ui
                    .selectable_label(branch == *path, format!("› {segment}"))
                    .clicked()
                {
                    category_click = Some(CategoryFilter::Path(branch.clone()));
                }
            }
        }
        if (is_all || is_uncategorized)
            && ui
                .selectable_label(is_uncategorized, "Sin categoría")
                .clicked()
        {
            category_click = Some(CategoryFilter::Uncategorized);
        }
        if !children.is_empty() {
            ui.separator();
            for child in &children {
                if ui.button(&child.name).clicked() {
                    let path = match scope {
                        CategoryScope::Video => self.category_paths.get(&child.id).cloned(),
                        CategoryScope::Photo => self.photo_category_paths.get(&child.id).cloned(),
                    };
                    category_click = path.map(CategoryFilter::Path);
                }
            }
        }

        ui.separator();
        if !is_uncategorized && ui.button("+ Subcategoría").clicked() {
            create = true;
        }
        if current_id.is_some() {
            if ui.button("Renombrar").clicked() {
                rename = true;
            }
            if ui.button("Eliminar categoría").clicked() {
                delete = true;
            }
        }

        if let Some(category) = category_click {
            match scope {
                CategoryScope::Video => self.set_category_filter(category),
                CategoryScope::Photo => self.set_photo_category_filter(category),
            }
        }
        if create {
            match scope {
                CategoryScope::Video => self.open_create_category(),
                CategoryScope::Photo => self.open_create_photo_category(),
            }
        }
        if rename {
            match scope {
                CategoryScope::Video => self.open_rename_category(),
                CategoryScope::Photo => self.open_rename_photo_category(),
            }
        }
        if delete {
            match scope {
                CategoryScope::Video => self.open_delete_category(),
                CategoryScope::Photo => self.open_delete_photo_category(),
            }
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(8.0);

            if self.tab == MediaTab::Photos {
                self.photo_top_bar(ui);
                ui.add_space(8.0);
                return;
            }

            let mut chosen_mode = self.search_mode;
            let mut source = self.filters.source;
            let mut clear_filters = false;
            let mut search_rect = None;
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(self.tab == MediaTab::Videos, "Videos")
                    .clicked()
                {
                    self.tab = MediaTab::Videos;
                }
                if ui
                    .selectable_label(self.tab == MediaTab::Photos, "Fotos")
                    .clicked()
                {
                    self.tab = MediaTab::Photos;
                }
                ui.separator();

                let hint = self.search_mode.hint();
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.search_query)
                        .id(egui::Id::unique(NAME_FILTER_ID))
                        .hint_text(hint)
                        .desired_width(260.0),
                );
                if response.changed() {
                    self.search_selected = 0;
                    if self.search_mode == SearchMode::Name {
                        self.filters.name = self.search_query.clone();
                        self.filter_dirty = true;
                    }
                }
                let search_focused = response.has_focus();
                let response_rect = response.rect;

                ui.menu_button(format!("Filtros: {}", self.search_mode.label()), |ui| {
                    for mode in SearchMode::ALL {
                        if ui
                            .selectable_label(self.search_mode == mode, mode.label())
                            .clicked()
                        {
                            chosen_mode = mode;
                        }
                    }
                    ui.separator();
                    for option in SourceFilter::ALL {
                        if ui
                            .selectable_label(self.filters.source == option, option.label())
                            .clicked()
                        {
                            source = option;
                        }
                    }
                });
                if chosen_mode != self.search_mode {
                    self.set_search_mode(chosen_mode);
                    self.focus_search(ui.ctx());
                }

                let category_label = self
                    .current_category_path()
                    .map(|path| format!("Categoría: {path}"))
                    .unwrap_or_else(|| "Categoría: Inicio".to_string());
                ui.menu_button(category_label, |ui| {
                    self.category_menu_contents(ui, CategoryScope::Video);
                });

                ui.menu_button("Reproducción", |ui| {
                    ui.checkbox(&mut self.audio_only, "Solo audio");
                    ui.checkbox(&mut self.detached_playback, "Ventana aparte (sin IPC)");
                    ui.separator();
                    ui.menu_button("Calidad", |ui| {
                        for quality in Quality::ALL {
                            if ui
                                .selectable_label(self.play_quality == quality, quality.label())
                                .clicked()
                            {
                                self.play_quality = quality;
                                ui.close();
                            }
                        }
                    });
                });

                if self.filters.active() && ui.small_button("Limpiar").clicked() {
                    clear_filters = true;
                }

                if search_focused {
                    search_rect = Some(response_rect);
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let scan_label = if self.scanning {
                        "Escaneando…"
                    } else {
                        "Escanear"
                    };

                    if ui
                        .add_enabled(!self.scanning, egui::Button::new(scan_label))
                        .clicked()
                    {
                        self.start_scan();
                    }

                    if ui.button("Importar").clicked() {
                        self.import_open = true;
                    }
                });
            });

            if source != self.filters.source {
                self.filters.source = source;
                self.filter_dirty = true;
            }
            if clear_filters {
                self.clear_filters_and_search();
            }

            if let Some(rect) = search_rect
                && self.search_mode != SearchMode::Name
                && !self.search_query.trim().is_empty()
            {
                self.search_results_popup(ui, rect);
            }

            ui.add_space(8.0);
        });
    }

    fn photo_top_bar(&mut self, ui: &mut egui::Ui) {
        let mut clear_filters = false;

        ui.horizontal(|ui| {
            if ui
                .selectable_label(self.tab == MediaTab::Videos, "Videos")
                .clicked()
            {
                self.tab = MediaTab::Videos;
            }
            if ui
                .selectable_label(self.tab == MediaTab::Photos, "Fotos")
                .clicked()
            {
                self.tab = MediaTab::Photos;
            }
            ui.separator();

            let response = ui.add(
                egui::TextEdit::singleline(&mut self.photo_filters.name)
                    .id(egui::Id::unique(NAME_FILTER_ID))
                    .hint_text("Buscar foto…")
                    .desired_width(260.0),
            );
            if response.changed() {
                self.photo_filter_dirty = true;
            }

            let category_label = self
                .current_photo_category_path()
                .map(|path| format!("Categoría: {path}"))
                .unwrap_or_else(|| "Categoría: Inicio".to_string());
            ui.menu_button(category_label, |ui| {
                self.category_menu_contents(ui, CategoryScope::Photo);
            });

            if self.photo_filters.active() && ui.small_button("Limpiar").clicked() {
                clear_filters = true;
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let scan_label = if self.photo_scanning {
                    "Escaneando…"
                } else {
                    "Escanear fotos"
                };
                if ui
                    .add_enabled(!self.photo_scanning, egui::Button::new(scan_label))
                    .clicked()
                {
                    self.start_photo_scan();
                }
            });
        });

        if clear_filters {
            self.photo_filters.clear();
            self.photo_filter_dirty = true;
        }
    }

    fn photo_viewer_targets(&self, clicked: &str) -> Vec<String> {
        if self.photo_selected.contains(clicked) {
            let mut ids = vec![clicked.to_string()];
            for id in self.photo_selected_ids() {
                if id != clicked {
                    ids.push(id);
                }
            }
            ids
        } else {
            vec![clicked.to_string()]
        }
    }

    fn photo_grid(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default_margins().show(ui, |ui| {
            if self.photos.is_empty() {
                ui.add_space(80.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No hay fotos").weak());
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!(
                            "Pulsá Escanear fotos para indexar {}",
                            self.config.pictures_root.display()
                        ))
                        .small()
                        .weak(),
                    );
                });
                return;
            }

            if self.photo_filtered.is_empty() {
                ui.add_space(80.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Sin resultados").weak());
                });
                return;
            }

            let mut selection_click: Option<(String, bool)> = None;
            let mut focus_click: Option<String> = None;
            let mut hover_click: Option<String> = None;
            let mut card_action: Option<(usize, &'static str)> = None;
            let mut open_photo: Option<String> = None;
            let mut to_generate: Vec<(String, PathBuf)> = Vec::new();
            let additive_select = ui.input(|state| state.modifiers.ctrl || state.modifiers.command);
            let pointer_moved = ui.input(|state| state.pointer.delta() != egui::Vec2::ZERO);

            ui.spacing_mut().item_spacing = egui::vec2(CARD_GAP, 18.0);
            let available_width = ui.available_width();
            let columns = (((available_width + CARD_GAP) / (CARD_MIN_WIDTH + CARD_GAP)).floor()
                as usize)
                .max(1);
            self.photo_grid_columns = columns;
            let card_width = ((available_width - CARD_GAP * (columns.saturating_sub(1)) as f32)
                / columns as f32)
                .max(120.0);
            let card_height = (card_width * CARD_RATIO).round();
            let available_height = ui.available_height();
            let rows = ((available_height / (card_height + 18.0)).floor() as usize).max(1);
            let page_size = (rows * columns).max(1);
            self.photo_page_size = page_size;
            let page_count = self.photo_filtered.len().div_ceil(page_size).max(1);
            if self.photo_scroll_to_focus
                && let Some(position) = self.photo_focus_index()
            {
                self.photo_page = position / page_size;
            }
            self.photo_page = self.photo_page.min(page_count - 1);
            self.photo_scroll_to_focus = false;
            let start = self.photo_page * page_size;
            let end = (start + page_size).min(self.photo_filtered.len());
            for row in self.photo_filtered[start..end].chunks(columns) {
                ui.horizontal_top(|ui| {
                    for &index in row {
                        let photo = &self.photos[index];
                        let selected = self.photo_selected.contains(&photo.id);
                        let focused = self.photo_focus_id.as_deref() == Some(photo.id.as_str());
                        let local_path = photo.path(&self.config.pictures_root);
                        let thumb = self.photo_thumb_path(photo);

                        if thumb.is_none()
                            && let Some(path) = &local_path
                            && !self.photo_pending_thumbs.contains(&photo.id)
                        {
                            to_generate.push((photo.id.clone(), path.clone()));
                        }

                        ui.vertical(|ui| {
                            ui.set_width(card_width);
                            ui.spacing_mut().item_spacing.y = 6.0;

                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2(card_width, card_height),
                                Sense::click(),
                            );

                            if let Some(path) = &thumb {
                                egui::Image::from_uri(format!("file://{}", path.display()))
                                    .corner_radius(egui::CornerRadius::same(4))
                                    .paint_at(ui, rect);
                            } else {
                                let painter = ui.painter();
                                painter.rect_filled(
                                    rect,
                                    egui::CornerRadius::same(4),
                                    ui.visuals().widgets.inactive.weak_bg_fill,
                                );
                                painter.text(
                                    rect.center(),
                                    Align2::CENTER_CENTER,
                                    "FOTO",
                                    FontId::proportional(12.0),
                                    ui.visuals().weak_text_color(),
                                );
                            }

                            if selected {
                                ui.painter().rect_stroke(
                                    rect,
                                    egui::CornerRadius::same(4),
                                    egui::Stroke::new(2.0, ui.visuals().selection.bg_fill),
                                    StrokeKind::Inside,
                                );
                            }

                            if focused {
                                ui.painter().rect_stroke(
                                    rect.expand(2.0),
                                    egui::CornerRadius::same(6),
                                    egui::Stroke::new(2.0, egui::Color32::from_rgb(245, 190, 80)),
                                    StrokeKind::Inside,
                                );
                                if self.photo_scroll_to_focus {
                                    ui.scroll_to_rect(rect, Some(Align::Center));
                                }
                            }

                            if pointer_moved && response.hovered() {
                                hover_click = Some(photo.id.clone());
                            }

                            if response.clicked() && !response.double_clicked() {
                                selection_click = Some((photo.id.clone(), additive_select));
                                focus_click = Some(photo.id.clone());
                            }

                            if response.double_clicked() {
                                selection_click = Some((photo.id.clone(), false));
                                focus_click = Some(photo.id.clone());
                                open_photo = Some(photo.id.clone());
                            }

                            let mut menu_action = None;
                            response.context_menu(|ui| {
                                if ui.button("Abrir en visor").clicked() {
                                    menu_action = Some("open");
                                    ui.close();
                                }
                                if ui.button("Asignar a categorías…").clicked() {
                                    menu_action = Some("categories");
                                    ui.close();
                                }
                                if ui.button("Info…").clicked() {
                                    menu_action = Some("info");
                                    ui.close();
                                }
                                if local_path.is_some()
                                    && ui.button("Regenerar miniatura").clicked()
                                {
                                    menu_action = Some("regen");
                                    ui.close();
                                }
                                ui.separator();
                                if ui.button("Quitar de biblioteca").clicked() {
                                    menu_action = Some("remove");
                                    ui.close();
                                }
                                if local_path.is_some() && ui.button("Borrar del disco").clicked() {
                                    menu_action = Some("delete");
                                    ui.close();
                                }
                            });

                            if let Some(action) = menu_action {
                                card_action = Some((index, action));
                            }

                            ui.add(
                                egui::Label::new(RichText::new(&photo.title).size(12.5)).truncate(),
                            );
                            let detail = match (photo.width, photo.height) {
                                (Some(width), Some(height)) => {
                                    format!("{width}×{height}")
                                }
                                _ => "Foto".to_string(),
                            };
                            ui.add(
                                egui::Label::new(RichText::new(detail).size(10.0).weak())
                                    .truncate(),
                            );
                        });
                    }
                });
            }

            for (id, path) in to_generate {
                self.photo_pending_thumbs.insert(id.clone());
                self.photo_thumbs.enqueue(&id, &path);
            }

            if let Some(id) = hover_click {
                self.photo_focus_id = Some(id);
            }
            if let Some(id) = focus_click {
                self.photo_focus_id = Some(id);
            }

            if let Some((id, additive)) = selection_click {
                if additive {
                    if !self.photo_selected.remove(&id) {
                        self.photo_selected.insert(id);
                    }
                } else if !self.photo_selected.contains(&id) {
                    self.photo_selected.clear();
                    self.photo_selected.insert(id);
                }
            }

            if let Some((index, action)) = card_action {
                let photo_id = self.photos[index].id.clone();
                match action {
                    "open" => {
                        let ids = self.photo_viewer_targets(&photo_id);
                        self.open_photos(&ids);
                    }
                    "categories" => {
                        self.photo_category_targets = vec![photo_id];
                        self.photo_category_filter.clear();
                    }
                    "info" => self.open_info(MediaTab::Photos, &photo_id),
                    "regen" => self.regenerate_photo_thumb(&photo_id),
                    "remove" => self.request_photo_remove(&[photo_id]),
                    "delete" => self.request_photo_delete(&[photo_id]),
                    _ => {}
                }
            }

            if let Some(id) = open_photo {
                let ids = self.photo_viewer_targets(&id);
                self.open_photos(&ids);
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let mut open_help = false;
        let mut open_log = false;
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let count = if self.tab == MediaTab::Photos {
                    let total = self.photos.len();
                    if self.photo_applied_filters.active() {
                        format!("{} de {total}", self.photo_filtered.len())
                    } else {
                        format!("{total} fotos")
                    }
                } else {
                    let total = self.videos.len();
                    if self.applied_filters.active() {
                        format!("{} de {total}", self.filtered.len())
                    } else {
                        format!("{total} videos")
                    }
                };

                ui.label(RichText::new(count).small().weak());

                let (page, page_size, total_items) = if self.tab == MediaTab::Photos {
                    (
                        self.photo_page,
                        self.photo_page_size,
                        self.photo_filtered.len(),
                    )
                } else {
                    (self.video_page, self.video_page_size, self.filtered.len())
                };
                let page_size = page_size.max(1);
                let page_count = total_items.div_ceil(page_size).max(1);
                ui.separator();
                if ui.small_button("‹").clicked() {
                    if self.tab == MediaTab::Photos {
                        self.photo_change_page(-1);
                    } else {
                        self.video_change_page(-1);
                    }
                }
                ui.label(
                    RichText::new(format!("Página {}/{}", page + 1, page_count))
                        .small()
                        .weak(),
                );
                if ui.small_button("›").clicked() {
                    if self.tab == MediaTab::Photos {
                        self.photo_change_page(1);
                    } else {
                        self.video_change_page(1);
                    }
                }

                ui.separator();
                ui.label(RichText::new(&self.status).small().weak());

                let selected_ids = if self.tab == MediaTab::Photos {
                    self.photo_selected_ids()
                } else {
                    self.selected_ids()
                };
                if !selected_ids.is_empty() {
                    ui.separator();
                    if self.tab == MediaTab::Photos {
                        ui.menu_button(format!("Selección ({})", selected_ids.len()), |ui| {
                            if ui.button("Abrir en visor").clicked() {
                                self.open_photos(&selected_ids);
                                ui.close();
                            }
                            if ui.button("Asignar a categorías…").clicked() {
                                self.photo_category_targets = selected_ids.clone();
                                self.photo_category_filter.clear();
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Quitar de biblioteca").clicked() {
                                self.request_photo_remove(&selected_ids);
                                ui.close();
                            }
                            if ui.button("Borrar del disco").clicked() {
                                self.request_photo_delete(&selected_ids);
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Deseleccionar").clicked() {
                                self.photo_selected.clear();
                                ui.close();
                            }
                        });
                    } else {
                        ui.menu_button(format!("Selección ({})", selected_ids.len()), |ui| {
                            if ui.button("Reproducir").clicked() {
                                self.open_play(selected_ids.clone());
                                ui.close();
                            }
                            ui.menu_button("Calidad de reproducción", |ui| {
                                for quality in Quality::ALL {
                                    if ui
                                        .selectable_label(
                                            self.play_quality == quality,
                                            quality.label(),
                                        )
                                        .clicked()
                                    {
                                        self.play_quality = quality;
                                        self.status = format!("Calidad: {}", quality.label());
                                        ui.close();
                                    }
                                }
                            });
                            if ui.button("Descargar…").clicked() {
                                self.open_download(selected_ids.clone());
                                ui.close();
                            }
                            if ui.button("Asignar a categorías…").clicked() {
                                self.category_targets = selected_ids.clone();
                                self.category_filter.clear();
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Borrar del disco").clicked() {
                                self.request_delete(&selected_ids);
                                ui.close();
                            }
                            if ui.button("Quitar de biblioteca").clicked() {
                                self.request_remove(&selected_ids);
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Deseleccionar").clicked() {
                                self.selected.clear();
                                ui.close();
                            }
                        });
                    }
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("? Ayuda").clicked() {
                        open_help = true;
                    }
                    let log_label = if self.log_alert {
                        "Registro (!)"
                    } else {
                        "Registro"
                    };
                    if ui.small_button(log_label).clicked() {
                        open_log = true;
                    }
                });
            });
            ui.add_space(4.0);
        });

        if open_help {
            self.help_open = true;
        }
        if open_log {
            self.log_open = true;
            self.log_alert = false;
        }
    }

    fn grid(&mut self, ui: &mut egui::Ui) {
        if self.tab == MediaTab::Photos {
            self.photo_grid(ui);
            return;
        }

        egui::CentralPanel::default_margins().show(ui, |ui| {
            if self.videos.is_empty() {
                ui.add_space(80.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No hay videos").weak());
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Pulsa Escanear para indexar tu carpeta o Importar para añadir URLs",
                        )
                        .small()
                        .weak(),
                    );
                });
                return;
            }

            if self.filtered.is_empty() {
                ui.add_space(80.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Sin resultados").weak());
                });
                return;
            }

            let mut selection_click: Option<(String, bool)> = None;
            let mut focus_click: Option<String> = None;
            let mut hover_click: Option<String> = None;
            let mut card_action = None;
            let mut to_generate: Vec<(String, PathBuf)> = Vec::new();
            let additive_select = ui.input(|state| state.modifiers.ctrl || state.modifiers.command);
            let pointer_moved = ui.input(|state| state.pointer.delta() != egui::Vec2::ZERO);

            ui.spacing_mut().item_spacing = egui::vec2(CARD_GAP, 18.0);
            let available_width = ui.available_width();
            let columns = (((available_width + CARD_GAP) / (CARD_MIN_WIDTH + CARD_GAP)).floor()
                as usize)
                .max(1);
            self.grid_columns = columns;
            let card_width = ((available_width - CARD_GAP * (columns.saturating_sub(1)) as f32)
                / columns as f32)
                .max(120.0);
            let card_height = (card_width * CARD_RATIO).round();
            let available_height = ui.available_height();
            let rows = ((available_height / (card_height + 18.0)).floor() as usize).max(1);
            let page_size = (rows * columns).max(1);
            self.video_page_size = page_size;
            let page_count = self.filtered.len().div_ceil(page_size).max(1);
            if self.scroll_to_focus
                && let Some(position) = self.focused_filtered_index()
            {
                self.video_page = position / page_size;
            }
            self.video_page = self.video_page.min(page_count - 1);
            self.scroll_to_focus = false;
            let start = self.video_page * page_size;
            let end = (start + page_size).min(self.filtered.len());
            for row in self.filtered[start..end].chunks(columns) {
                ui.horizontal_top(|ui| {
                    for &index in row {
                        let video = &self.videos[index];
                        let selected = self.selected.contains(&video.id);
                        let focused = self.focus_id.as_deref() == Some(video.id.as_str());
                        let local_path = video.path(&self.config.root);
                        let thumb = self.thumb_path(video);

                        if thumb.is_none()
                            && let Some(path) = &local_path
                            && !self.pending_thumbs.contains(&video.id)
                        {
                            to_generate.push((video.id.clone(), path.clone()));
                        }

                        ui.vertical(|ui| {
                            ui.set_width(card_width);
                            ui.spacing_mut().item_spacing.y = 6.0;

                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2(card_width, card_height),
                                Sense::click(),
                            );

                            if let Some(path) = &thumb {
                                egui::Image::from_uri(format!("file://{}", path.display()))
                                    .corner_radius(egui::CornerRadius::same(4))
                                    .paint_at(ui, rect);
                            } else {
                                let painter = ui.painter();
                                painter.rect_filled(
                                    rect,
                                    egui::CornerRadius::same(4),
                                    ui.visuals().widgets.inactive.weak_bg_fill,
                                );
                                painter.text(
                                    rect.center(),
                                    Align2::CENTER_CENTER,
                                    video.badge(),
                                    FontId::proportional(12.0),
                                    ui.visuals().weak_text_color(),
                                );
                            }

                            if selected {
                                ui.painter().rect_stroke(
                                    rect,
                                    egui::CornerRadius::same(4),
                                    egui::Stroke::new(2.0, ui.visuals().selection.bg_fill),
                                    StrokeKind::Inside,
                                );
                            }

                            if focused {
                                ui.painter().rect_stroke(
                                    rect.expand(2.0),
                                    egui::CornerRadius::same(6),
                                    egui::Stroke::new(2.0, egui::Color32::from_rgb(245, 190, 80)),
                                    StrokeKind::Inside,
                                );
                                if self.scroll_to_focus {
                                    ui.scroll_to_rect(rect, Some(Align::Center));
                                }
                            }

                            if pointer_moved && response.hovered() {
                                hover_click = Some(video.id.clone());
                            }

                            if response.clicked() && !response.double_clicked() {
                                selection_click = Some((video.id.clone(), additive_select));
                                focus_click = Some(video.id.clone());
                            }

                            if response.double_clicked() {
                                selection_click = Some((video.id.clone(), false));
                                focus_click = Some(video.id.clone());
                                card_action = Some((index, CardAction::Play));
                            }

                            let has_local = local_path.is_some();
                            let has_url = video.url.is_some();
                            let can_download = has_url && !has_local;
                            let can_unlink = has_url && has_local;
                            let mut menu_action = None;

                            response.context_menu(|ui| {
                                if ui.button("Reproducir").clicked() {
                                    menu_action = Some(CardAction::Play);
                                    ui.close();
                                }
                                ui.menu_button("Calidad de reproducción", |ui| {
                                    for quality in Quality::ALL {
                                        if ui
                                            .selectable_label(
                                                self.play_quality == quality,
                                                quality.label(),
                                            )
                                            .clicked()
                                        {
                                            menu_action = Some(CardAction::SetPlayQuality(quality));
                                            ui.close();
                                        }
                                    }
                                });
                                if ui
                                    .add_enabled(can_download, egui::Button::new("Descargar…"))
                                    .clicked()
                                {
                                    menu_action = Some(CardAction::Download);
                                    ui.close();
                                }
                                if ui.button("Asignar a categorías…").clicked() {
                                    menu_action = Some(CardAction::Categories);
                                    ui.close();
                                }
                                if ui.button("Info…").clicked() {
                                    menu_action = Some(CardAction::Info);
                                    ui.close();
                                }
                                if has_url || has_local {
                                    ui.menu_button("Regenerar miniatura", |ui| {
                                        if has_url && ui.button("Con yt-dlp").clicked() {
                                            menu_action = Some(CardAction::RegenerateYtdlp);
                                            ui.close();
                                        }
                                        if has_local && ui.button("Con ffmpeg").clicked() {
                                            menu_action = Some(CardAction::RegenerateFfmpeg);
                                            ui.close();
                                        }
                                    });
                                }
                                if can_unlink && ui.button("Desvincular URL").clicked() {
                                    menu_action = Some(CardAction::UnlinkUrl);
                                    ui.close();
                                }
                                ui.separator();
                                if ui.button("Borrar del disco").clicked() {
                                    menu_action = Some(CardAction::Delete);
                                    ui.close();
                                }
                                if ui.button("Quitar de la biblioteca").clicked() {
                                    menu_action = Some(CardAction::RemoveRow);
                                    ui.close();
                                }
                            });

                            if let Some(action) = menu_action {
                                card_action = Some((index, action));
                            }

                            ui.add(
                                egui::Label::new(RichText::new(&video.title).size(12.5)).truncate(),
                            );
                        });
                    }
                });
            }

            for (id, path) in to_generate {
                self.pending_thumbs.insert(id.clone());
                self.thumbs.enqueue(&id, &path);
            }

            if let Some(id) = hover_click {
                self.focus_id = Some(id);
            }
            if let Some(id) = focus_click {
                self.focus_id = Some(id);
            }

            if let Some((id, additive)) = selection_click {
                if additive {
                    if !self.selected.remove(&id) {
                        self.selected.insert(id);
                    }
                } else {
                    self.selected.clear();
                    self.selected.insert(id);
                }
            }

            if let Some((index, action)) = card_action {
                self.handle_card_action(index, action);
            }
        });
    }

    fn download_bar(&mut self, ui: &mut egui::Ui) {
        if self.downloading.is_none()
            && self.download_queue.is_empty()
            && self.download_failed.is_empty()
            && self.download_last_error.is_none()
        {
            return;
        }

        let mut retry = false;
        let mut dismiss = false;

        egui::Panel::bottom("download").show(ui, |ui| {
            ui.add_space(4.0);

            if let Some(task) = &self.downloading {
                let title = self
                    .videos
                    .iter()
                    .find(|video| video.id == task.id)
                    .map(|video| video.title.clone())
                    .unwrap_or_else(|| "video".to_string());
                let queued = self.download_queue.len();

                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("Descargando: {title}"))
                            .small()
                            .strong(),
                    );
                    if queued > 0 {
                        ui.label(RichText::new(format!("· {queued} en cola")).small().weak());
                    }

                    if let Some(progress) = &self.download_progress {
                        if let Some(percent) = progress.percent {
                            let fraction = (percent / 100.0).clamp(0.0, 1.0) as f32;
                            let width = (ui.available_width() - 260.0).clamp(120.0, 360.0);
                            ui.add(
                                egui::ProgressBar::new(fraction)
                                    .show_percentage()
                                    .desired_width(width),
                            );
                        } else {
                            ui.spinner();
                            if !progress.raw.is_empty() {
                                ui.label(RichText::new(&progress.raw).small().weak());
                            }
                        }
                        if let Some(speed) = &progress.speed {
                            ui.label(RichText::new(speed).small().weak());
                        }
                    } else {
                        ui.spinner();
                        ui.label(RichText::new("iniciando…").small().weak());
                    }
                });
            } else if !self.download_queue.is_empty() {
                ui.label(
                    RichText::new(format!("{} descarga(s) en cola", self.download_queue.len()))
                        .small()
                        .weak(),
                );
            }

            if self.download_last_error.is_some() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Una descarga falló")
                            .small()
                            .color(ui.visuals().error_fg_color),
                    );

                    if !self.download_failed.is_empty()
                        && ui
                            .small_button(format!("Reintentar ({})", self.download_failed.len()))
                            .clicked()
                    {
                        retry = true;
                    }
                    if ui.small_button("Descartar").clicked() {
                        dismiss = true;
                    }
                });
            }

            ui.add_space(4.0);
        });

        if retry {
            self.retry_downloads();
        }
        if dismiss {
            self.dismiss_download_errors();
        }
    }

    fn player_bar(&mut self, ui: &mut egui::Ui, playback: &PlaybackState) {
        if !playback.active {
            return;
        }

        egui::Panel::bottom("player").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let label = if playback.paused {
                    "Reproducir"
                } else {
                    "Pausa"
                };

                if ui.button(label).clicked() {
                    self.player.toggle_pause();
                }
                if ui.button("Detener").clicked() {
                    self.player.stop();
                }

                let playing_title = self
                    .playing_title
                    .clone()
                    .or_else(|| playback.title.clone());
                if let Some(title) = playing_title {
                    ui.add_sized(
                        [220.0, 18.0],
                        egui::Label::new(RichText::new(title).small().strong()).truncate(),
                    );
                }

                let mut position = playback.position.clamp(0.0, playback.duration.max(0.0));
                let width = (ui.available_width() - 150.0).max(80.0);
                let response = ui.add_sized(
                    [width, 18.0],
                    egui::Slider::new(&mut position, 0.0..=playback.duration.max(1.0))
                        .show_value(false),
                );

                if response.changed() {
                    self.player.seek(position);
                }

                if let Some(title) = &playback.title {
                    let _ = response.on_hover_text(title);
                }

                ui.label(
                    RichText::new(format!(
                        "{} / {}",
                        format_duration(position as i64),
                        format_duration(playback.duration as i64)
                    ))
                    .small()
                    .weak(),
                );
            });
            ui.add_space(4.0);
        });
    }

    fn import_modal(&mut self, ctx: &egui::Context) {
        if !self.import_open {
            return;
        }

        let mut open = self.import_open;
        let mut start = false;

        egui::Window::new("Importar URLs")
            .open(&mut open)
            .default_width(520.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("Una URL por línea. Solo se descargan metadatos y thumbnail.")
                        .small()
                        .weak(),
                );
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::multiline(&mut self.import_text)
                        .desired_rows(6)
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!self.importing, egui::Button::new("Importar"))
                        .clicked()
                    {
                        start = true;
                    }
                    if self.importing {
                        ui.spinner();
                    }
                });

                if !self.import_log.is_empty() {
                    ui.add_space(8.0);
                    egui::ScrollArea::vertical()
                        .max_height(160.0)
                        .show(ui, |ui| {
                            for line in &self.import_log {
                                ui.label(RichText::new(line).small());
                            }
                        });
                }
            });

        self.import_open = open;
        if start {
            self.start_import();
        }
    }

    fn categories_modal(&mut self, ctx: &egui::Context) {
        if self.category_targets.is_empty() {
            return;
        }

        let targets = self.category_targets.clone();

        let mut categories: Vec<(String, String)> = self
            .category_paths
            .iter()
            .map(|(id, path)| (id.clone(), path.clone()))
            .collect();
        categories.sort_by(|left, right| {
            left.1
                .to_lowercase()
                .cmp(&right.1.to_lowercase())
                .then_with(|| left.1.cmp(&right.1))
        });

        let filter = self.category_filter.trim().to_lowercase();
        if !filter.is_empty() {
            categories.retain(|(_, path)| path.to_lowercase().contains(&filter));
        }

        let mut open = true;
        let mut assign: Option<String> = None;
        let mut remove: Option<String> = None;

        egui::Window::new("Asignar a categorías")
            .open(&mut open)
            .default_width(400.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("Elegí a qué categorías pertenecen los videos seleccionados.")
                        .small()
                        .weak(),
                );
                ui.add_space(4.0);
                if targets.len() > 1 {
                    ui.label(
                        RichText::new(format!("{} videos seleccionados", targets.len()))
                            .small()
                            .strong(),
                    );
                    ui.add_space(4.0);
                }

                ui.add(
                    egui::TextEdit::singleline(&mut self.category_filter)
                        .hint_text("Filtrar categorías…")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(6.0);

                if categories.is_empty() {
                    ui.label(
                        RichText::new(
                            "No hay categorías existentes. Creá una desde la barra de categorías.",
                        )
                        .small()
                        .weak(),
                    );
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(280.0)
                        .show(ui, |ui| {
                            for (category_id, path) in &categories {
                                let state = self.category_selection_state(&targets, category_id);
                                let mut checked = state == CategorySelection::All;
                                let response = ui
                                    .add(
                                        egui::Checkbox::new(&mut checked, path.as_str())
                                            .indeterminate(state == CategorySelection::Some),
                                    )
                                    .on_hover_text(
                                        "Marcado: todos los seleccionados. Indeterminado: algunos.",
                                    );
                                if response.changed() {
                                    if checked {
                                        assign = Some(category_id.clone());
                                    } else {
                                        remove = Some(category_id.clone());
                                    }
                                }
                            }
                        });
                }
            });

        if let Some(category_id) = assign {
            let mut failed = None;
            for target in &targets {
                if let Err(error) = self.db.add_video_category(target, &category_id) {
                    failed = Some(error);
                    break;
                }
            }
            match failed {
                Some(error) => self.report_error("app", error),
                None => {
                    self.reload_categories();
                    self.status = if targets.len() > 1 {
                        format!("Categoría asignada a {} videos", targets.len())
                    } else {
                        "Categoría asignada".to_string()
                    };
                }
            }
        }

        if let Some(category_id) = remove {
            let mut failed = None;
            for target in &targets {
                if let Err(error) = self.db.remove_video_category(target, &category_id) {
                    failed = Some(error);
                    break;
                }
            }
            match failed {
                Some(error) => self.report_error("app", error),
                None => {
                    self.reload_categories();
                    self.status = if targets.len() > 1 {
                        format!("Categoría quitada de {} videos", targets.len())
                    } else {
                        "Quitado de la categoría".to_string()
                    };
                }
            }
        }

        if !open {
            self.category_targets.clear();
            self.category_filter.clear();
        }
    }

    fn photo_categories_modal(&mut self, ctx: &egui::Context) {
        if self.photo_category_targets.is_empty() {
            return;
        }

        let targets = self.photo_category_targets.clone();

        let mut categories: Vec<(String, String)> = self
            .photo_category_paths
            .iter()
            .map(|(id, path)| (id.clone(), path.clone()))
            .collect();
        categories.sort_by(|left, right| {
            left.1
                .to_lowercase()
                .cmp(&right.1.to_lowercase())
                .then_with(|| left.1.cmp(&right.1))
        });

        let filter = self.photo_category_filter.trim().to_lowercase();
        if !filter.is_empty() {
            categories.retain(|(_, path)| path.to_lowercase().contains(&filter));
        }

        let mut open = true;
        let mut assign: Option<String> = None;
        let mut remove: Option<String> = None;

        egui::Window::new("Asignar a categorías")
            .id(egui::Id::unique("photo-categories-modal"))
            .open(&mut open)
            .default_width(400.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("Elegí a qué categorías pertenecen las fotos seleccionadas.")
                        .small()
                        .weak(),
                );
                ui.add_space(4.0);
                if targets.len() > 1 {
                    ui.label(
                        RichText::new(format!("{} fotos seleccionadas", targets.len()))
                            .small()
                            .strong(),
                    );
                    ui.add_space(4.0);
                }

                ui.add(
                    egui::TextEdit::singleline(&mut self.photo_category_filter)
                        .hint_text("Filtrar categorías…")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(6.0);

                if categories.is_empty() {
                    ui.label(
                        RichText::new(
                            "No hay categorías de fotos. Creá una desde la barra de categorías.",
                        )
                        .small()
                        .weak(),
                    );
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(280.0)
                        .show(ui, |ui| {
                            for (category_id, path) in &categories {
                                let state =
                                    self.photo_category_selection_state(&targets, category_id);
                                let mut checked = state == CategorySelection::All;
                                let response = ui.add(
                                    egui::Checkbox::new(&mut checked, path.as_str())
                                        .indeterminate(state == CategorySelection::Some),
                                );
                                if response.changed() {
                                    if checked {
                                        assign = Some(category_id.clone());
                                    } else {
                                        remove = Some(category_id.clone());
                                    }
                                }
                            }
                        });
                }
            });

        if let Some(category_id) = assign {
            let mut failed = None;
            for target in &targets {
                if let Err(error) = self.db.add_photo_category(target, &category_id) {
                    failed = Some(error);
                    break;
                }
            }
            match failed {
                Some(error) => self.report_error("app", error),
                None => {
                    self.reload_photo_categories();
                    self.status = if targets.len() > 1 {
                        format!("Categoría asignada a {} fotos", targets.len())
                    } else {
                        "Categoría asignada".to_string()
                    };
                }
            }
        }

        if let Some(category_id) = remove {
            let mut failed = None;
            for target in &targets {
                if let Err(error) = self.db.remove_photo_category(target, &category_id) {
                    failed = Some(error);
                    break;
                }
            }
            match failed {
                Some(error) => self.report_error("app", error),
                None => {
                    self.reload_photo_categories();
                    self.status = if targets.len() > 1 {
                        format!("Categoría quitada de {} fotos", targets.len())
                    } else {
                        "Quitado de la categoría".to_string()
                    };
                }
            }
        }

        if !open {
            self.photo_category_targets.clear();
            self.photo_category_filter.clear();
        }
    }

    fn category_dialog_modal(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.category_dialog.take() else {
            return;
        };
        let is_photo = self.category_dialog_scope == CategoryScope::Photo;

        let title = match &dialog {
            CategoryDialog::Create { .. } => "Nueva subcategoría",
            CategoryDialog::Rename { .. } => "Renombrar categoría",
            CategoryDialog::Delete { .. } => "Eliminar categoría",
        };

        let mut open = true;
        let mut submit = false;
        let mut cancel = false;

        egui::Window::new(title)
            .open(&mut open)
            .default_width(430.0)
            .resizable(false)
            .show(ctx, |ui| match &mut dialog {
                CategoryDialog::Create {
                    parent_path, name, ..
                } => {
                    if let Some(parent) = parent_path {
                        ui.label(
                            RichText::new(format!("Se creará dentro de «{parent}»."))
                                .small()
                                .weak(),
                        );
                    } else {
                        ui.label(
                            RichText::new("Se creará en el nivel principal.").small().weak(),
                        );
                    }
                    ui.add_space(4.0);
                    let response = ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text("Nombre de la categoría")
                            .desired_width(f32::INFINITY),
                    );
                    let enter = response.has_focus()
                        && ui.input(|state| state.key_pressed(Key::Enter));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(!name.trim().is_empty(), egui::Button::new("Crear"))
                            .clicked()
                        {
                            submit = true;
                        }
                        if ui.button("Cancelar").clicked() {
                            cancel = true;
                        }
                    });
                    if enter && !name.trim().is_empty() {
                        submit = true;
                    }
                }
                CategoryDialog::Rename {
                    old_path, name, ..
                } => {
                    ui.label(RichText::new(old_path.as_str()).small().weak());
                    ui.add_space(4.0);
                    let response = ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text("Nuevo nombre")
                            .desired_width(f32::INFINITY),
                    );
                    let enter = response.has_focus()
                        && ui.input(|state| state.key_pressed(Key::Enter));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(!name.trim().is_empty(), egui::Button::new("Renombrar"))
                            .clicked()
                        {
                            submit = true;
                        }
                        if ui.button("Cancelar").clicked() {
                            cancel = true;
                        }
                    });
                    if enter && !name.trim().is_empty() {
                        submit = true;
                    }
                }
                CategoryDialog::Delete {
                    path,
                    categories,
                    videos,
                    ..
                } => {
                    ui.label(
                        RichText::new(format!("¿Eliminar «{path}» y todas sus subcategorías?"))
                            .strong(),
                    );
                    ui.add_space(4.0);
                    let media_label = if is_photo { "foto(s)" } else { "video(s)" };
                    ui.label(
                        RichText::new(format!(
                            "Se eliminarán {categories} categoría(s). Los {videos} {media_label} no se borran: solo se quitan de estas categorías."
                        ))
                        .small()
                        .weak(),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(RichText::new("Eliminar").color(ui.visuals().error_fg_color))
                            .clicked()
                        {
                            submit = true;
                        }
                        if ui.button("Cancelar").clicked() {
                            cancel = true;
                        }
                    });
                }
            });

        if cancel {
            open = false;
        }

        if open {
            self.category_dialog = Some(dialog);
            if submit {
                self.submit_category_dialog();
            }
        }
    }

    fn download_modal(&mut self, ctx: &egui::Context) {
        if self.download_targets.is_empty() {
            return;
        }

        let ids = self.download_targets.clone();
        let titles: Vec<String> = ids
            .iter()
            .filter_map(|id| {
                self.videos
                    .iter()
                    .find(|video| &video.id == id)
                    .map(|video| video.title.clone())
            })
            .collect();
        let preview = if self.download_dir.is_empty() {
            self.config.root.display().to_string()
        } else {
            self.config
                .root
                .join(&self.download_dir)
                .display()
                .to_string()
        };
        let destinations = self.download_dirs.clone();

        let mut open = true;
        let mut start = false;
        let mut cancelled = false;

        egui::Window::new("Descargar")
            .open(&mut open)
            .default_width(460.0)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("Carpeta de destino (existente dentro de Vídeos)").small(),
                );
                egui::ComboBox::from_id_salt("download-dir")
                    .selected_text(if self.download_dir.is_empty() {
                        "Raíz de Vídeos".to_string()
                    } else {
                        self.download_dir.clone()
                    })
                    .width(320.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.download_dir,
                            String::new(),
                            "Raíz de Vídeos",
                        );
                        for destination in &destinations {
                            ui.selectable_value(
                                &mut self.download_dir,
                                destination.clone(),
                                destination,
                            );
                        }
                    });
                ui.label(
                    RichText::new(format!("Se guardará en: {preview}"))
                        .small()
                        .weak(),
                );

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Calidad:");
                    egui::ComboBox::from_id_salt("download_quality")
                        .selected_text(self.download_quality.label())
                        .show_ui(ui, |ui| {
                            for quality in Quality::ALL {
                                ui.selectable_value(
                                    &mut self.download_quality,
                                    quality,
                                    quality.label(),
                                );
                            }
                        });
                });
                ui.label(
                    RichText::new(
                        "Elegí una carpeta existente o la raíz de Vídeos. No se crean carpetas nuevas.",
                    )
                    .small()
                    .weak(),
                );

                ui.add_space(8.0);
                if titles.len() == 1 {
                    ui.label(RichText::new(&titles[0]).strong());
                } else {
                    ui.label(RichText::new(format!("{} videos", titles.len())).strong());
                    egui::ScrollArea::vertical()
                        .max_height(120.0)
                        .show(ui, |ui| {
                            for title in &titles {
                                ui.label(RichText::new(title).small());
                            }
                        });
                }

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancelar").clicked() {
                        cancelled = true;
                    }
                    if ui.button("Descargar").clicked() {
                        start = true;
                    }
                });
            });

        if start {
            self.start_downloads();
        } else if cancelled || !open {
            self.download_targets.clear();
        }
    }

    fn help_modal(&mut self, ctx: &egui::Context) {
        if !self.help_open {
            return;
        }

        let mut open = self.help_open;
        egui::Window::new("Atajos de teclado")
            .open(&mut open)
            .default_width(430.0)
            .resizable(false)
            .show(ctx, |ui| {
                egui::Grid::new("keyboard-help")
                    .num_columns(2)
                    .spacing([18.0, 6.0])
                    .show(ui, |ui| {
                        for (keys, action) in [
                            ("/", "Buscar por nombre"),
                            ("Ctrl+K", "Enfocar el buscador global"),
                            ("j / k", "Mover el foco abajo / arriba"),
                            ("h / l", "Mover el foco izquierda / derecha"),
                            ("Shift+←/→", "Página anterior / siguiente"),
                            ("Flechas", "Mover el foco por la grilla"),
                            ("Enter", "Reproducir o ir al resultado del buscador"),
                            ("Espacio", "Seleccionar o deseleccionar el enfocado"),
                            ("i", "Ver información del enfocado"),
                            ("Esc", "Salir del buscador, cerrar modal o limpiar"),
                            ("?", "Mostrar esta ayuda"),
                        ] {
                            ui.label(RichText::new(keys).strong().monospace());
                            ui.label(action);
                            ui.end_row();
                        }
                    });
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "En modo Nombre, Enter reproduce el enfocado. En Categoría o Ruta, usá ↑/↓ y Enter para ir al resultado.",
                    )
                    .small()
                    .weak(),
                );
            });

        self.help_open = open;
    }

    fn log_modal(&mut self, ctx: &egui::Context) {
        if !self.log_open {
            return;
        }

        let mut open = true;
        let mut close = false;
        let mut clear = false;

        egui::Window::new("Registro")
            .id(egui::Id::unique("log-modal"))
            .open(&mut open)
            .default_width(760.0)
            .resizable(true)
            .show(ctx, |ui| {
                if self.log.is_empty() {
                    ui.label(RichText::new("Sin actividad").small().weak());
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(420.0)
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for entry in &self.log {
                                let text = RichText::new(entry.text.as_str()).monospace().small();
                                let text = if entry.is_error {
                                    text.color(ui.visuals().error_fg_color)
                                } else {
                                    text
                                };
                                ui.add(egui::Label::new(text).wrap().selectable(true));
                            }
                        });
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Limpiar").clicked() {
                        clear = true;
                    }
                    if ui.button("Cerrar").clicked() {
                        close = true;
                    }
                });
            });

        if close {
            open = false;
        }
        if clear {
            self.log.clear();
            self.log_alert = false;
        }
        self.log_open = open;
    }

    fn open_info(&mut self, tab: MediaTab, id: &str) {
        self.info_target = Some((tab, id.to_string()));
    }

    fn info_rows(&self) -> Option<Vec<(String, String)>> {
        let (tab, id) = self.info_target.clone()?;
        let mut rows: Vec<(String, String)> = Vec::new();

        match tab {
            MediaTab::Videos => {
                let video = self.videos.iter().find(|video| video.id == id)?;
                rows.push(("Título".to_string(), video.title.clone()));
                rows.push(("Estado".to_string(), video.status.clone()));
                if let Some(path) = video.path(&self.config.root) {
                    rows.push(("Archivo".to_string(), path.display().to_string()));
                    if let Ok(metadata) = fs::metadata(&path) {
                        rows.push(("Tamaño".to_string(), format_size(metadata.len())));
                        if let Ok(modified) = metadata.modified() {
                            rows.push(("Modificado".to_string(), format_system_time(modified)));
                        }
                    }
                } else if let Some(rel) = &video.rel_path {
                    rows.push(("Ruta".to_string(), rel.clone()));
                }
                if let Some(url) = &video.url {
                    rows.push(("URL".to_string(), url.clone()));
                }
                if let Some(channel) = &video.channel {
                    rows.push(("Canal".to_string(), channel.clone()));
                }
                if let Some(duration) = video.duration_s {
                    rows.push(("Duración".to_string(), format_duration(duration)));
                }
                if let Some(entries) = self
                    .video_categories
                    .get(&video.id)
                    .filter(|entries| !entries.is_empty())
                {
                    rows.push((
                        "Categorías".to_string(),
                        entries
                            .iter()
                            .map(|(_, path)| path.as_str())
                            .collect::<Vec<_>>()
                            .join(" · "),
                    ));
                }
            }
            MediaTab::Photos => {
                let photo = self.photos.iter().find(|photo| photo.id == id)?;
                rows.push(("Título".to_string(), photo.title.clone()));
                rows.push(("Estado".to_string(), photo.status.clone()));
                if let Some(path) = photo.path(&self.config.pictures_root) {
                    rows.push(("Archivo".to_string(), path.display().to_string()));
                    if let Ok(metadata) = fs::metadata(&path) {
                        rows.push(("Tamaño".to_string(), format_size(metadata.len())));
                        if let Ok(modified) = metadata.modified() {
                            rows.push(("Modificado".to_string(), format_system_time(modified)));
                        }
                    }
                } else if let Some(rel) = &photo.rel_path {
                    rows.push(("Ruta".to_string(), rel.clone()));
                }
                if let (Some(width), Some(height)) = (photo.width, photo.height) {
                    rows.push(("Dimensiones".to_string(), format!("{width}×{height}")));
                }
                if let Some(entries) = self
                    .photo_assignments
                    .get(&photo.id)
                    .filter(|entries| !entries.is_empty())
                {
                    rows.push((
                        "Categorías".to_string(),
                        entries
                            .iter()
                            .map(|(_, path)| path.as_str())
                            .collect::<Vec<_>>()
                            .join(" · "),
                    ));
                }
            }
        }

        Some(rows)
    }

    fn info_modal(&mut self, ctx: &egui::Context) {
        let Some(rows) = self.info_rows() else {
            self.info_target = None;
            return;
        };
        let title = match self.info_target.as_ref().map(|(tab, _)| tab) {
            Some(MediaTab::Photos) => "Información de la foto",
            _ => "Información del video",
        };

        let mut open = true;
        egui::Window::new(title)
            .id(egui::Id::unique("info-modal"))
            .open(&mut open)
            .default_width(560.0)
            .resizable(true)
            .show(ctx, |ui| {
                egui::Grid::new("info-grid")
                    .num_columns(2)
                    .spacing([14.0, 6.0])
                    .show(ui, |ui| {
                        for (key, value) in &rows {
                            ui.label(RichText::new(key).strong());
                            ui.add(
                                egui::Label::new(RichText::new(value))
                                    .wrap()
                                    .selectable(true),
                            );
                            ui.end_row();
                        }
                    });
            });

        if !open {
            self.info_target = None;
        }
    }

    fn confirm_modal(&mut self, ctx: &egui::Context) {
        let Some(action) = self.confirm_action.clone() else {
            return;
        };

        let (title, heading, note, verb) = match &action {
            ConfirmAction::DeleteFiles(targets) => {
                let local = targets
                    .iter()
                    .filter(|target| target.path.is_some())
                    .count();
                let online = targets.len() - local;
                let heading = if local > 0 && online > 0 {
                    format!("¿Borrar {local} archivo(s) y quitar {online} entrada(s) online?")
                } else if online > 0 {
                    format!("¿Quitar {online} entrada(s) online de la biblioteca?")
                } else {
                    format!("¿Borrar {local} archivo(s) del disco?")
                };
                (
                    "Borrar del disco",
                    heading,
                    "Los archivos locales se borran del disco; las entradas online solo se quitan de la biblioteca (y su miniatura).",
                    "Borrar",
                )
            }
            ConfirmAction::RemoveRows(targets) => (
                "Quitar de la biblioteca",
                format!("¿Quitar {} video(s) de la biblioteca?", targets.len()),
                "Los archivos locales no se borran del disco.",
                "Quitar",
            ),
            ConfirmAction::DeletePhotos(targets) => (
                "Borrar del disco",
                format!("¿Borrar {} foto(s) del disco?", targets.len()),
                "También se quitarán de la biblioteca y se borrarán sus miniaturas.",
                "Borrar",
            ),
            ConfirmAction::RemovePhotoRows(targets) => (
                "Quitar de la biblioteca",
                format!("¿Quitar {} foto(s) de la biblioteca?", targets.len()),
                "Los archivos no se borran del disco.",
                "Quitar",
            ),
        };

        let mut open = true;
        let mut confirm = false;
        let mut cancelled = false;

        egui::Window::new(title)
            .open(&mut open)
            .default_width(420.0)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(heading);
                ui.label(RichText::new(note).small().weak());
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .max_height(140.0)
                    .show(ui, |ui| match &action {
                        ConfirmAction::DeleteFiles(targets)
                        | ConfirmAction::DeletePhotos(targets) => {
                            for target in targets {
                                if let Some(path) = &target.path {
                                    ui.label(RichText::new(path.display().to_string()).small());
                                } else {
                                    ui.label(RichText::new(&target.title).small());
                                }
                            }
                        }
                        ConfirmAction::RemoveRows(targets)
                        | ConfirmAction::RemovePhotoRows(targets) => {
                            for target in targets {
                                ui.label(RichText::new(&target.title).small());
                            }
                        }
                    });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancelar").clicked() {
                        cancelled = true;
                    }
                    let button = if matches!(
                        action,
                        ConfirmAction::DeleteFiles(_) | ConfirmAction::DeletePhotos(_)
                    ) {
                        egui::Button::new(verb).fill(ui.visuals().error_fg_color)
                    } else {
                        egui::Button::new(verb)
                    };
                    if ui.add(button).clicked() {
                        confirm = true;
                    }
                });
            });

        if confirm {
            self.apply_confirm(action);
        } else if cancelled || !open {
            self.confirm_action = None;
        }
    }

    fn apply_confirm(&mut self, action: ConfirmAction) {
        self.confirm_action = None;
        let (targets, delete_files, is_photo) = match action {
            ConfirmAction::DeleteFiles(targets) => (targets, true, false),
            ConfirmAction::RemoveRows(targets) => (targets, false, false),
            ConfirmAction::DeletePhotos(targets) => (targets, true, true),
            ConfirmAction::RemovePhotoRows(targets) => (targets, false, true),
        };

        let mut done = 0;
        let mut errors = 0;

        for target in targets {
            if delete_files {
                let file_ok = match &target.path {
                    Some(path) => match fs::remove_file(path) {
                        Ok(()) => true,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                        Err(_) => false,
                    },
                    None => true,
                };
                if !file_ok {
                    errors += 1;
                    continue;
                }
            }

            if is_photo {
                self.remove_photo_thumb_files(&target.id, target.thumb_key.as_deref());
                match self.db.delete_photo(&target.id) {
                    Ok(()) => done += 1,
                    Err(_) => errors += 1,
                }
                self.photo_selected.remove(&target.id);
            } else {
                self.remove_thumb_files(&target.id, target.thumb_key.as_deref());
                match self.db.delete_video(&target.id) {
                    Ok(()) => done += 1,
                    Err(_) => errors += 1,
                }
                self.selected.remove(&target.id);
            }
        }

        if is_photo {
            self.reload_photos();
        } else {
            self.reload();
        }

        self.status = if errors == 0 {
            if delete_files {
                format!("{done} elemento(s) borrados")
            } else {
                format!("{done} elemento(s) quitados de la biblioteca")
            }
        } else {
            format!("{done} completados · {errors} error(es)")
        };
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_scan(&ctx);
        self.poll_photo_scan(&ctx);
        self.poll_import();
        self.poll_download();
        self.poll_thumbs();
        self.poll_photo_thumbs();
        self.poll_thumb_regen();
        self.poll_player_errors();

        if self.scanning || self.importing || self.downloading.is_some() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }

        let playback = self.player.snapshot();
        if playback.active {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        self.refresh_filter();
        self.refresh_photo_filter();
        self.handle_keyboard(&ctx);
        self.top_bar(ui);
        self.status_bar(ui);
        self.player_bar(ui, &playback);
        self.download_bar(ui);
        self.grid(ui);

        self.import_modal(&ctx);
        self.categories_modal(&ctx);
        self.photo_categories_modal(&ctx);
        self.category_dialog_modal(&ctx);
        self.download_modal(&ctx);
        self.help_modal(&ctx);
        self.log_modal(&ctx);
        self.info_modal(&ctx);
        self.confirm_modal(&ctx);
    }
}

fn vertical_focus_target(
    current: usize,
    columns: usize,
    row_delta: isize,
    len: usize,
) -> Option<usize> {
    if len == 0 || columns == 0 || current >= len {
        return None;
    }

    let row = current / columns;
    let column = current % columns;
    let target_row = row as isize + row_delta;
    if target_row < 0 || target_row as usize > (len - 1) / columns {
        return None;
    }

    Some((target_row as usize * columns + column).min(len - 1))
}

fn build_category_children(categories: &[Category]) -> HashMap<Option<String>, Vec<Category>> {
    let mut children: HashMap<Option<String>, Vec<Category>> = HashMap::new();
    for category in categories {
        children
            .entry(category.parent_id.clone())
            .or_default()
            .push(category.clone());
    }
    for entries in children.values_mut() {
        entries.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.name.cmp(&right.name))
        });
    }
    children
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn path_parent(path: &str) -> Option<String> {
    path.rsplit_once('/').map(|(parent, _)| parent.to_string())
}

fn selection_state(flags: &[bool]) -> CategorySelection {
    let assigned = flags.iter().filter(|flag| **flag).count();
    if assigned == 0 {
        CategorySelection::None
    } else if assigned == flags.len() {
        CategorySelection::All
    } else {
        CategorySelection::Some
    }
}

fn path_search_label(path: &str) -> String {
    if path.is_empty() {
        "Vídeos (raíz)".to_string()
    } else {
        path.to_string()
    }
}

fn rank_search(query: &str, entries: Vec<(String, String)>) -> Vec<(String, String)> {
    const MAX_RESULTS: usize = 20;

    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(MatcherConfig::DEFAULT.match_paths());
    let mut buffer = Vec::new();
    let mut hits: Vec<(u32, String, String)> = Vec::new();

    for (label, target) in entries {
        if let Some(score) = pattern.score(Utf32Str::new(&label, &mut buffer), &mut matcher) {
            hits.push((score, label, target));
        }
    }

    hits.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    hits.truncate(MAX_RESULTS);
    hits.into_iter()
        .map(|(_, label, target)| (label, target))
        .collect()
}

fn build_category_paths(categories: &[Category]) -> HashMap<String, String> {
    let by_id: HashMap<&str, &Category> = categories
        .iter()
        .map(|category| (category.id.as_str(), category))
        .collect();
    let mut paths = HashMap::new();

    for category in categories {
        let mut segments = Vec::new();
        let mut current = Some(category);
        let mut depth = 0;

        while let Some(node) = current {
            segments.push(node.name.clone());
            current = node
                .parent_id
                .as_deref()
                .and_then(|parent| by_id.get(parent).copied());
            depth += 1;
            if depth > 128 {
                break;
            }
        }

        segments.reverse();
        paths.insert(category.id.clone(), segments.join("/"));
    }

    paths
}

fn list_existing_dirs(root: &Path) -> Vec<String> {
    let mut dirs = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .filter_entry(|entry| !is_ignored_dir(entry))
        .filter_map(|entry| entry.ok())
    {
        if !entry.file_type().is_dir() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        dirs.push(relative.to_string_lossy().replace('\\', "/"));
    }

    dirs.sort_by(|left, right| {
        left.to_lowercase()
            .cmp(&right.to_lowercase())
            .then_with(|| left.cmp(right))
    });
    dirs
}

fn is_ignored_dir(entry: &walkdir::DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    name.starts_with('.') || name == "node_modules" || name == "lost+found"
}

fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.2} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

fn format_system_time(time: std::time::SystemTime) -> String {
    let seconds = time
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    format_timestamp(seconds)
}

fn format_timestamp(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

fn format_duration(seconds: i64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CategoryFilter, CategorySelection, Quality, SourceFilter, build_category_paths,
        format_duration, list_existing_dirs, path_matches_prefix, path_parent, rank_search,
        selection_state, vertical_focus_target,
    };
    use crate::domain::Category;

    #[test]
    fn formats_short_and_long_durations() {
        assert_eq!(format_duration(65), "1:05");
        assert_eq!(format_duration(3725), "1:02:05");
    }

    #[test]
    fn computes_category_selection_state() {
        assert_eq!(selection_state(&[false, false]), CategorySelection::None);
        assert_eq!(selection_state(&[true, true]), CategorySelection::All);
        assert_eq!(selection_state(&[true, false]), CategorySelection::Some);
        assert_eq!(selection_state(&[false, true]), CategorySelection::Some);
    }

    #[test]
    fn lists_existing_download_dirs() {
        let base = std::env::temp_dir().join(format!(
            "video-player-dirs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(base.join("Pelis/Accion")).unwrap();
        std::fs::create_dir_all(base.join(".oculto")).unwrap();
        std::fs::create_dir_all(base.join("node_modules/x")).unwrap();

        let dirs = list_existing_dirs(&base);
        assert!(dirs.contains(&"Pelis".to_string()));
        assert!(dirs.contains(&"Pelis/Accion".to_string()));
        assert!(!dirs.iter().any(|dir| dir.starts_with('.')));
        assert!(!dirs.iter().any(|dir| dir.starts_with("node_modules")));

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn maps_quality_formats() {
        assert_eq!(Quality::Preferred.format("pref"), "pref");
        assert_eq!(Quality::Maximum.format("pref"), "bv*+ba/b");
        assert!(Quality::P720.format("pref").contains("height<=720"));
    }

    #[test]
    fn moves_focus_vertically_in_grid() {
        assert_eq!(vertical_focus_target(0, 7, 1, 14), Some(7));
        assert_eq!(vertical_focus_target(13, 7, -1, 14), Some(6));
        assert_eq!(vertical_focus_target(7, 7, 1, 14), None);
        assert_eq!(vertical_focus_target(0, 7, -1, 14), None);

        assert_eq!(vertical_focus_target(2, 7, 1, 10), Some(9));
        assert_eq!(vertical_focus_target(9, 7, -1, 10), Some(2));
        assert_eq!(vertical_focus_target(6, 7, 1, 10), Some(9));
    }

    #[test]
    fn matches_local_path_prefixes_at_boundaries() {
        assert!(path_matches_prefix(
            "Pelis/Accion/video.mkv",
            "Pelis/Accion"
        ));
        assert!(path_matches_prefix("Pelis/Accion", "Pelis/Accion"));
        assert!(path_matches_prefix("Pelis/Accion/video.mkv", ""));
        assert!(!path_matches_prefix(
            "Pelis/Accion2/video.mkv",
            "Pelis/Accion"
        ));
        assert_eq!(
            path_parent("Pelis/Accion/video.mkv").as_deref(),
            Some("Pelis/Accion")
        );
        assert_eq!(path_parent("video.mkv"), None);
    }

    #[test]
    fn ranks_fuzzy_search_results() {
        let hits = rank_search(
            "accion",
            vec![
                ("Pelis/Drama".to_string(), "drama".to_string()),
                ("Pelis/Accion".to_string(), "accion".to_string()),
            ],
        );
        assert_eq!(
            hits.first().map(|(_, target)| target.as_str()),
            Some("accion")
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn filters_categories_by_branch() {
        let parent = CategoryFilter::Path("Pelis".to_string());
        assert!(parent.matches(&["Pelis/Accion"]));
        assert!(parent.matches(&["Pelis"]));
        assert!(!parent.matches(&["Pelis2/Accion"]));
        assert!(!parent.matches(&[]));

        assert!(CategoryFilter::Uncategorized.matches(&[]));
        assert!(!CategoryFilter::Uncategorized.matches(&["Pelis"]));
        assert!(CategoryFilter::All.matches(&[]));
    }

    #[test]
    fn filters_local_and_online_sources() {
        assert!(SourceFilter::Local.matches(true, true));
        assert!(!SourceFilter::Local.matches(false, true));

        assert!(SourceFilter::Online.matches(false, true));
        assert!(!SourceFilter::Online.matches(true, true));
        assert!(!SourceFilter::Online.matches(false, false));

        assert!(SourceFilter::All.matches(false, false));
    }

    #[test]
    fn builds_category_paths() {
        let categories = vec![
            Category {
                id: "1".to_string(),
                parent_id: None,
                name: "Pelis".to_string(),
            },
            Category {
                id: "2".to_string(),
                parent_id: Some("1".to_string()),
                name: "Accion".to_string(),
            },
        ];
        let paths = build_category_paths(&categories);
        assert_eq!(paths.get("2").map(String::as_str), Some("Pelis/Accion"));
    }
}
