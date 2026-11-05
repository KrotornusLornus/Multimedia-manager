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
use crate::domain::{Category, Document, Photo, Video};
use crate::player::{PlaybackState, Player, spawn_detached};
use crate::scan::{self, MediaScanEvent, ScanEvent};
use crate::thumbs::{self, DocumentThumbs, PhotoThumbs, Thumbs};
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

struct SettingsDraft {
    root: String,
    pictures_root: String,
    documents_root: String,
    mpv: String,
    ytdlp: String,
    ffmpeg: String,
    ytdl_format: String,
    image_viewer: String,
    wallpaper_setter: String,
    python: String,
    thumb_width: String,
    photo_thumb_width: String,
    document_thumb_width: String,
    extensions: String,
    photo_extensions: String,
    document_extensions: String,
}

impl SettingsDraft {
    fn from_config(config: &Config) -> Self {
        Self {
            root: config.root.display().to_string(),
            pictures_root: config.pictures_root.display().to_string(),
            documents_root: config.documents_root.display().to_string(),
            mpv: config.mpv.clone(),
            ytdlp: config.ytdlp.clone(),
            ffmpeg: config.ffmpeg.clone(),
            ytdl_format: config.ytdl_format.clone(),
            image_viewer: config.image_viewer.clone(),
            wallpaper_setter: config.wallpaper_setter.clone(),
            python: config.python.clone(),
            thumb_width: config.thumb_width.to_string(),
            photo_thumb_width: config.photo_thumb_width.to_string(),
            document_thumb_width: config.document_thumb_width.to_string(),
            extensions: config.extensions.join(", "),
            photo_extensions: config.photo_extensions.join(", "),
            document_extensions: config.document_extensions.join(", "),
        }
    }

    fn apply_to(self, config: &mut Config) {
        config.root = PathBuf::from(self.root.trim());
        config.pictures_root = PathBuf::from(self.pictures_root.trim());
        config.documents_root = PathBuf::from(self.documents_root.trim());
        if !self.mpv.trim().is_empty() {
            config.mpv = self.mpv.trim().to_string();
        }
        if !self.ytdlp.trim().is_empty() {
            config.ytdlp = self.ytdlp.trim().to_string();
        }
        if !self.ffmpeg.trim().is_empty() {
            config.ffmpeg = self.ffmpeg.trim().to_string();
        }
        if !self.ytdl_format.trim().is_empty() {
            config.ytdl_format = self.ytdl_format.trim().to_string();
        }
        if !self.image_viewer.trim().is_empty() {
            config.image_viewer = self.image_viewer.trim().to_string();
        }
        if !self.wallpaper_setter.trim().is_empty() {
            config.wallpaper_setter = self.wallpaper_setter.trim().to_string();
        }
        if !self.python.trim().is_empty() {
            config.python = self.python.trim().to_string();
        }
        if let Ok(value) = self.thumb_width.trim().parse() {
            config.thumb_width = value;
        }
        if let Ok(value) = self.photo_thumb_width.trim().parse() {
            config.photo_thumb_width = value;
        }
        if let Ok(value) = self.document_thumb_width.trim().parse() {
            config.document_thumb_width = value;
        }
        let extensions = parse_extension_list(&self.extensions);
        if !extensions.is_empty() {
            config.extensions = extensions;
        }
        let photo_extensions = parse_extension_list(&self.photo_extensions);
        if !photo_extensions.is_empty() {
            config.photo_extensions = photo_extensions;
        }
        let document_extensions = parse_extension_list(&self.document_extensions);
        if !document_extensions.is_empty() {
            config.document_extensions = document_extensions;
        }
    }
}

fn hidden_category_ids(categories: &[Category]) -> HashSet<String> {
    let mut hidden: HashSet<String> = categories
        .iter()
        .filter(|category| category.hidden)
        .map(|category| category.id.clone())
        .collect();
    let mut changed = true;
    while changed {
        changed = false;
        for category in categories {
            if let Some(parent) = &category.parent_id
                && hidden.contains(parent)
                && !hidden.contains(&category.id)
            {
                hidden.insert(category.id.clone());
                changed = true;
            }
        }
    }
    hidden
}

fn shuffle_targets(targets: &mut [String]) {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(1);
    for index in (1..targets.len()).rev() {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let swap = ((seed >> 33) as usize) % (index + 1);
        targets.swap(index, swap);
    }
}

fn parse_extension_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|item| item.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|item| !item.is_empty())
        .collect()
}

fn settings_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.add(egui::TextEdit::singleline(value).desired_width(340.0));
    ui.end_row();
}

#[derive(Clone)]
enum ConfirmAction {
    DeleteFiles(Vec<VideoTarget>),
    RemoveRows(Vec<VideoTarget>),
    DeletePhotos(Vec<VideoTarget>),
    RemovePhotoRows(Vec<VideoTarget>),
    DeleteDocuments(Vec<VideoTarget>),
    RemoveDocumentRows(Vec<VideoTarget>),
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
            Quality::Preferred => "Default",
            Quality::Maximum => "Maximum",
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
            SourceFilter::All => "All",
            SourceFilter::Local => "Local",
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
            SearchMode::Name => "Name",
            SearchMode::Category => "Category",
            SearchMode::Path => "Path",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            SearchMode::Name => "Search by name…",
            SearchMode::Category => "Search category…",
            SearchMode::Path => "Search local folder…",
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
    Documents,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum CategoryScope {
    #[default]
    Video,
    Photo,
    Document,
}

#[derive(Clone, PartialEq, Eq, Default)]
struct PhotoFilters {
    name: String,
    category: CategoryFilter,
    path_prefix: Option<String>,
}

impl PhotoFilters {
    fn active(&self) -> bool {
        !self.name.trim().is_empty()
            || self.category != CategoryFilter::All
            || self.path_prefix.is_some()
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
    photo_scan_rx: Option<Receiver<MediaScanEvent>>,
    document_scan_rx: Option<Receiver<MediaScanEvent>>,
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

    documents: Vec<Document>,
    document_filters: PhotoFilters,
    document_applied_filters: PhotoFilters,
    document_filtered: Vec<usize>,
    document_filter_dirty: bool,
    document_selected: HashSet<String>,
    document_focus_id: Option<String>,
    document_grid_columns: usize,
    document_page: usize,
    document_page_size: usize,
    document_scroll_to_focus: bool,
    document_scanning: bool,
    document_thumbs: DocumentThumbs,
    document_thumbs_dir: PathBuf,
    document_pending_thumbs: HashSet<String>,
    document_failed_thumbs: HashSet<String>,
    document_categories: Vec<Category>,
    document_category_paths: HashMap<String, String>,
    document_category_ids_by_path: HashMap<String, String>,
    document_category_children: HashMap<Option<String>, Vec<Category>>,
    document_assignments: HashMap<String, Vec<(String, String)>>,
    document_category_targets: Vec<String>,
    document_category_filter: String,

    show_hidden: bool,
    hidden_videos: HashSet<String>,
    hidden_photos: HashSet<String>,
    hidden_documents: HashSet<String>,

    play_quality: Quality,
    audio_only: bool,
    detached_playback: bool,
    shuffle: bool,
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
    settings_draft: Option<SettingsDraft>,
    info_target: Option<(MediaTab, String)>,

    confirm_action: Option<ConfirmAction>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, config: Config, db_path: PathBuf, db: Db) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);

        let thumbs_dir = config::thumbs_dir();
        let photo_thumbs_dir = config::photo_thumbs_dir();
        let photo_thumb_width = config.photo_thumb_width;
        let document_thumbs_dir = config::document_thumbs_dir();
        let document_thumb_width = config.document_thumb_width;
        let python = config.python.clone();
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
            documents: Vec::new(),
            document_filters: PhotoFilters::default(),
            document_applied_filters: PhotoFilters::default(),
            document_filtered: Vec::new(),
            document_filter_dirty: true,
            document_selected: HashSet::new(),
            document_focus_id: None,
            document_grid_columns: 1,
            document_page: 0,
            document_page_size: 1,
            document_scroll_to_focus: false,
            document_scan_rx: None,
            document_scanning: false,
            document_thumbs: DocumentThumbs::new(
                document_thumbs_dir.clone(),
                document_thumb_width,
                python,
                2,
            ),
            document_thumbs_dir,
            document_pending_thumbs: HashSet::new(),
            document_failed_thumbs: HashSet::new(),
            document_categories: Vec::new(),
            document_category_paths: HashMap::new(),
            document_category_ids_by_path: HashMap::new(),
            document_category_children: HashMap::new(),
            document_assignments: HashMap::new(),
            document_category_targets: Vec::new(),
            document_category_filter: String::new(),
            show_hidden: false,
            hidden_videos: HashSet::new(),
            hidden_photos: HashSet::new(),
            hidden_documents: HashSet::new(),
            play_quality: Quality::Preferred,
            audio_only: false,
            detached_playback: false,
            shuffle: false,
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
            settings_draft: None,
            info_target: None,
            confirm_action: None,
        };

        app.reload();
        app.reload_photos();
        app.reload_documents();
        app
    }

    fn report_error(&mut self, source: &str, detail: impl std::fmt::Display) {
        self.push_log_entry(format!("[{source}] {detail}"), true);
        self.status = "An error occurred (see Log)".to_string();
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
                let hidden = if self.show_hidden {
                    HashSet::new()
                } else {
                    hidden_category_ids(&categories)
                };
                let visible: Vec<Category> = categories
                    .iter()
                    .filter(|category| !hidden.contains(&category.id))
                    .cloned()
                    .collect();
                self.category_paths = build_category_paths(&visible);
                self.category_ids_by_path = self
                    .category_paths
                    .iter()
                    .map(|(id, path)| (path.clone(), id.clone()))
                    .collect();
                self.category_children = build_category_children(&visible);
                self.categories = categories;

                if let CategoryFilter::Path(path) = &self.filters.category
                    && !self.category_ids_by_path.contains_key(path)
                {
                    self.filters.category = CategoryFilter::All;
                }

                let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
                let mut hidden_media = HashSet::new();
                for (video_id, category_id) in assignments {
                    if hidden.contains(&category_id) {
                        hidden_media.insert(video_id.clone());
                    }
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
                self.hidden_videos = hidden_media;
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
                let hidden = if self.show_hidden {
                    HashSet::new()
                } else {
                    hidden_category_ids(&categories)
                };
                let visible: Vec<Category> = categories
                    .iter()
                    .filter(|category| !hidden.contains(&category.id))
                    .cloned()
                    .collect();
                self.photo_category_paths = build_category_paths(&visible);
                self.photo_category_ids_by_path = self
                    .photo_category_paths
                    .iter()
                    .map(|(id, path)| (path.clone(), id.clone()))
                    .collect();
                self.photo_category_children = build_category_children(&visible);
                self.photo_categories = categories;

                if let CategoryFilter::Path(path) = &self.photo_filters.category
                    && !self.photo_category_ids_by_path.contains_key(path)
                {
                    self.photo_filters.category = CategoryFilter::All;
                }

                let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
                let mut hidden_media = HashSet::new();
                for (photo_id, category_id) in assignments {
                    if hidden.contains(&category_id) {
                        hidden_media.insert(photo_id.clone());
                    }
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
                self.hidden_photos = hidden_media;
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
        if self.search_mode == SearchMode::Name {
            for raw in self.photo_filters.name.split_whitespace() {
                patterns.push(Pattern::parse(
                    raw,
                    CaseMatching::Ignore,
                    Normalization::Smart,
                ));
            }
        }

        let filters_changed = self.photo_applied_filters != self.photo_filters;
        let mut matcher = Matcher::new(MatcherConfig::DEFAULT.match_paths());
        let mut buffer = Vec::new();
        let mut scored: Vec<(u32, usize)> = Vec::new();

        for (index, photo) in self.photos.iter().enumerate() {
            if self.hidden_photos.contains(&photo.id) {
                continue;
            }
            let category_paths: Vec<&str> = self
                .photo_assignments
                .get(&photo.id)
                .map(|entries| entries.iter().map(|(_, path)| path.as_str()).collect())
                .unwrap_or_default();
            if !self.photo_filters.category.matches(&category_paths) {
                continue;
            }

            if let Some(prefix) = &self.photo_filters.path_prefix {
                let Some(rel_path) = &photo.rel_path else {
                    continue;
                };
                if !path_matches_prefix(rel_path, prefix) {
                    continue;
                }
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
        self.status = "Scanning photos…".to_string();

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
                    Ok(MediaScanEvent::Progress(found)) => {
                        self.status = format!("Scanning photos… {found} files");
                    }
                    Ok(MediaScanEvent::Finished { found, error }) => {
                        finished = Some((found, error));
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = Some((0, Some("the photo scan stopped".to_string())));
                        break;
                    }
                }
            }

            if let Some((found, error)) = finished {
                self.photo_scanning = false;
                self.photo_scan_rx = None;
                self.reload_photos();
                match error {
                    Some(error) => self.report_error("photos", error),
                    None => {
                        self.push_log(format!("Photo scan: {found} photos"));
                        self.status = format!("{found} photos found");
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
                    self.status = "Thumbnail regenerated".to_string();
                    self.thumb_regen_rx = None;
                }
                Ok(Err(error)) => {
                    self.report_error("yt-dlp", error);
                    self.thumb_regen_rx = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.report_error("yt-dlp", "regeneration was interrupted");
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
            self.report_error("yt-dlp", "the video has no URL");
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
        self.status = "Regenerating thumbnail with yt-dlp…".to_string();
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
            self.report_error("ffmpeg", "the video has no local file");
            return;
        };

        self.remove_thumb_files(id, video.thumb_key.as_deref());
        if let Some(video) = self.videos.iter_mut().find(|video| video.id == id) {
            video.thumb_key = None;
        }
        self.pending_thumbs.insert(id.to_string());
        self.thumbs.enqueue(id, &path);
        self.status = "Regenerating thumbnail with ffmpeg…".to_string();
    }

    fn regenerate_photo_thumb(&mut self, id: &str) {
        let Some(photo) = self.photos.iter().find(|photo| photo.id == id) else {
            return;
        };
        let Some(path) = photo.path(&self.config.pictures_root) else {
            self.report_error("photos", "the photo has no local file");
            return;
        };

        self.remove_photo_thumb_files(id, photo.thumb_key.as_deref());
        if let Some(photo) = self.photos.iter_mut().find(|photo| photo.id == id) {
            photo.thumb_key = None;
        }
        self.photo_pending_thumbs.insert(id.to_string());
        self.photo_thumbs.enqueue(id, &path);
        self.status = "Regenerating thumbnail…".to_string();
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

    fn set_wallpaper(&mut self, id: &str) {
        let Some(photo) = self.photos.iter().find(|photo| photo.id == id) else {
            return;
        };
        let Some(path) = photo.path(&self.config.pictures_root) else {
            self.report_error("wallpaper", "the photo has no local file");
            return;
        };

        // Same behaviour as the swayimg config: remember the path and replace the
        // running wallpaper process.
        let cache_file = config::wallpaper_cache_file();
        if let Some(parent) = cache_file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&cache_file, path.display().to_string());

        let _ = Command::new("pkill")
            .arg(&self.config.wallpaper_setter)
            .status();

        match Command::new(&self.config.wallpaper_setter)
            .arg("-s")
            .arg(&path)
            .spawn()
        {
            Ok(_) => self.status = format!("Wallpaper set: {}", photo.title),
            Err(error) => self.report_error(
                "wallpaper",
                format!("could not run {}: {error}", self.config.wallpaper_setter),
            ),
        }
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
            self.report_error("viewer", "no image files found");
            return;
        }

        match Command::new(&self.config.image_viewer)
            .arg("-v")
            .args(&paths)
            .spawn()
        {
            Ok(_) => self.status = format!("Opening {} photo(s)…", paths.len()),
            Err(error) => self.report_error(
                "viewer",
                format!("could not open {}: {error}", self.config.image_viewer),
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
        self.status = "Scanning…".to_string();

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
                        self.status = format!("Scanning… {found} files");
                    }
                    Ok(ScanEvent::Finished { found, error }) => {
                        finished = Some((found, error));
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = Some((0, Some("the scan stopped".to_string())));
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
                        self.push_log(format!("Scan: {found} videos"));
                        self.status = format!("{found} files found");
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
        self.status = "Importing…".to_string();

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
                        self.push_log(format!("Imported: {title}"));
                    }
                    Ok(ImportEvent::Skipped(url)) => {
                        self.import_log.push(format!("Already exists · {url}"));
                        self.push_log(format!("Already exists: {url}"));
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
                            "Import: {imported} imported · {skipped} skipped · {failed} failed"
                        ));
                        self.status =
                            format!("{imported} imported · {skipped} skipped · {failed} failed");
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.importing = false;
                        self.import_rx = None;
                        self.report_error("yt-dlp", "connection interrupted");
                        self.status = "Import stopped".to_string();
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
            self.status = "No downloadable videos in the selection".to_string();
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
            self.status = "Nothing to download".to_string();
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
            format!("Downloading «{title}» · {pending} queued")
        } else {
            format!("Downloading «{title}»")
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
                            Some(percent) => format!("Downloading… {percent:.1}%"),
                            None => "Downloading…".to_string(),
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
                                    "Downloads finished with {} failure(s)",
                                    self.download_errors
                                )
                            } else {
                                "Download finished".to_string()
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
                            self.status = "A download failed; you can retry it below".to_string();
                        } else {
                            self.start_next_download();
                        }
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if let Some(task) = self.downloading.take() {
                            let error = "connection to yt-dlp interrupted".to_string();
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
            if self.hidden_videos.contains(&video.id) {
                continue;
            }
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
            || !self.document_category_targets.is_empty()
            || !self.download_targets.is_empty()
            || self.confirm_action.is_some()
            || self.category_dialog.is_some()
            || self.info_target.is_some()
            || self.log_open
            || self.settings_draft.is_some()
    }

    fn close_modals(&mut self) {
        self.help_open = false;
        self.import_open = false;
        self.category_targets.clear();
        self.photo_category_targets.clear();
        self.document_category_targets.clear();
        self.download_targets.clear();
        self.confirm_action = None;
        self.category_dialog = None;
        self.info_target = None;
        self.log_open = false;
        self.settings_draft = None;
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
        if !self.photo_filters.name.is_empty() {
            self.photo_filters.name.clear();
            self.photo_filter_dirty = true;
        }
        if !self.document_filters.name.is_empty() {
            self.document_filters.name.clear();
            self.document_filter_dirty = true;
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

    fn navigate_to_photo_category(&mut self, path: String) {
        self.photo_filters = PhotoFilters {
            name: String::new(),
            category: CategoryFilter::Path(path),
            path_prefix: None,
        };
        self.search_selected = 0;
        self.photo_filter_dirty = true;
    }

    fn navigate_to_photo_path(&mut self, prefix: String) {
        self.photo_filters = PhotoFilters {
            name: String::new(),
            category: CategoryFilter::All,
            path_prefix: Some(prefix),
        };
        self.search_selected = 0;
        self.photo_filter_dirty = true;
    }

    fn clear_filters_and_search(&mut self) {
        self.filters.clear();
        self.search_query.clear();
        self.search_selected = 0;
        self.filter_dirty = true;
    }

    fn search_results(&self) -> Vec<(String, String)> {
        let query = match self.tab {
            MediaTab::Videos => self.search_query.trim(),
            MediaTab::Photos => self.photo_filters.name.trim(),
            MediaTab::Documents => self.document_filters.name.trim(),
        };
        if query.is_empty() {
            return Vec::new();
        }

        match self.search_mode {
            SearchMode::Name => Vec::new(),
            SearchMode::Category => {
                let entries = match self.tab {
                    MediaTab::Videos => &self.category_paths,
                    MediaTab::Photos => &self.photo_category_paths,
                    MediaTab::Documents => &self.document_category_paths,
                }
                .values()
                .map(|path| (path.clone(), path.clone()))
                .collect();
                rank_search(query, entries)
            }
            SearchMode::Path => {
                let mut folders: HashSet<String> = HashSet::new();
                match self.tab {
                    MediaTab::Videos => {
                        for video in &self.videos {
                            if let Some(rel_path) = &video.rel_path {
                                folders.insert(path_parent(rel_path).unwrap_or_default());
                            }
                        }
                    }
                    MediaTab::Photos => {
                        for photo in &self.photos {
                            if let Some(rel_path) = &photo.rel_path {
                                folders.insert(path_parent(rel_path).unwrap_or_default());
                            }
                        }
                    }
                    MediaTab::Documents => {
                        for document in &self.documents {
                            if let Some(rel_path) = &document.rel_path {
                                folders.insert(path_parent(rel_path).unwrap_or_default());
                            }
                        }
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
            SearchMode::Category => match self.tab {
                MediaTab::Videos => self.navigate_to_category(target),
                MediaTab::Photos => self.navigate_to_photo_category(target),
                MediaTab::Documents => self.navigate_to_document_category(target),
            },
            SearchMode::Path => match self.tab {
                MediaTab::Videos => self.navigate_to_path(target),
                MediaTab::Photos => self.navigate_to_photo_path(target),
                MediaTab::Documents => self.navigate_to_document_path(target),
            },
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
        match self.category_dialog_scope {
            CategoryScope::Photo => {
                self.submit_photo_category_dialog();
                return;
            }
            CategoryScope::Document => {
                self.submit_document_category_dialog();
                return;
            }
            CategoryScope::Video => {}
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
                    self.status = format!("Category created: {path}");
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
                        self.status = format!("Category renamed: {new_path}");
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
                    self.status =
                        format!("Category deleted: {categories} category(ies), {videos} video(s)");
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
                    self.status = format!("Photo category created: {path}");
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
                        self.status = format!("Photo category renamed: {new_path}");
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
                        "Photo category deleted: {categories} category(ies), {videos} photo(s)"
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

        if self.tab == MediaTab::Documents {
            self.handle_document_keyboard(ctx);
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

        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::A)) {
            self.select_all_current();
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

        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::A)) {
            self.select_all_current();
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
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, Key::W)) {
            if let Some(id) = self.photo_focus_id.clone() {
                self.set_wallpaper(&id);
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
            self.status = "Nothing playable in the selection".to_string();
            return;
        }

        if self.shuffle && targets.len() > 1 {
            shuffle_targets(&mut targets);
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
                        format!("Playing {count} videos (separate window)")
                    } else {
                        "Playing (separate window)".to_string()
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
                let suffix = if self.audio_only { " (audio only)" } else { "" };
                self.status = if count > 1 {
                    format!("Playing {count} videos{suffix}")
                } else {
                    format!("Playing{suffix}")
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

    fn select_all_current(&mut self) {
        match self.tab {
            MediaTab::Videos => {
                self.selected = self
                    .filtered
                    .iter()
                    .map(|&index| self.videos[index].id.clone())
                    .collect();
            }
            MediaTab::Photos => {
                self.photo_selected = self
                    .photo_filtered
                    .iter()
                    .map(|&index| self.photos[index].id.clone())
                    .collect();
            }
            MediaTab::Documents => {
                self.document_selected = self
                    .document_filtered
                    .iter()
                    .map(|&index| self.documents[index].id.clone())
                    .collect();
            }
        }
        self.status = "Selected all".to_string();
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
            self.status = "No items in the selection".to_string();
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
                        self.status = "URL unlinked".to_string();
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
                self.status = format!("Playback quality: {}", quality.label());
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
                        ui.label(RichText::new("No results").small().weak());
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
            CategoryScope::Document => (
                self.current_document_category_path().map(str::to_string),
                self.current_document_category_id().map(str::to_string),
                self.document_filters.category == CategoryFilter::All,
                self.document_filters.category == CategoryFilter::Uncategorized,
                self.current_document_children(),
            ),
        };

        let current_hidden = current_id.as_ref().is_some_and(|id| match scope {
            CategoryScope::Video => self
                .categories
                .iter()
                .any(|category| &category.id == id && category.hidden),
            CategoryScope::Photo => self
                .photo_categories
                .iter()
                .any(|category| &category.id == id && category.hidden),
            CategoryScope::Document => self
                .document_categories
                .iter()
                .any(|category| &category.id == id && category.hidden),
        });

        let mut category_click = None;
        let mut create = false;
        let mut rename = false;
        let mut delete = false;
        let mut toggle_hidden = false;
        let mut show_hidden = None;

        if ui.selectable_label(is_all, "Home").clicked() {
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
                .selectable_label(is_uncategorized, "Uncategorized")
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
                        CategoryScope::Document => {
                            self.document_category_paths.get(&child.id).cloned()
                        }
                    };
                    category_click = path.map(CategoryFilter::Path);
                }
            }
        }

        ui.separator();
        if !is_uncategorized && ui.button("+ Subcategory").clicked() {
            create = true;
        }
        if current_id.is_some() {
            if ui.button("Rename").clicked() {
                rename = true;
            }
            if ui.button("Delete category").clicked() {
                delete = true;
            }
            let hidden_label = if current_hidden {
                "Show category"
            } else {
                "Hide category"
            };
            if ui.button(hidden_label).clicked() {
                toggle_hidden = true;
            }
        }

        ui.separator();
        let mut show_hidden_value = self.show_hidden;
        if ui
            .checkbox(&mut show_hidden_value, "Show hidden categories")
            .changed()
        {
            show_hidden = Some(show_hidden_value);
        }

        if let Some(category) = category_click {
            match scope {
                CategoryScope::Video => self.set_category_filter(category),
                CategoryScope::Photo => self.set_photo_category_filter(category),
                CategoryScope::Document => self.set_document_category_filter(category),
            }
        }
        if create {
            match scope {
                CategoryScope::Video => self.open_create_category(),
                CategoryScope::Photo => self.open_create_photo_category(),
                CategoryScope::Document => self.open_create_document_category(),
            }
        }
        if rename {
            match scope {
                CategoryScope::Video => self.open_rename_category(),
                CategoryScope::Photo => self.open_rename_photo_category(),
                CategoryScope::Document => self.open_rename_document_category(),
            }
        }
        if delete {
            match scope {
                CategoryScope::Video => self.open_delete_category(),
                CategoryScope::Photo => self.open_delete_photo_category(),
                CategoryScope::Document => self.open_delete_document_category(),
            }
        }
        if toggle_hidden && let Some(id) = current_id {
            self.set_category_hidden(scope, &id, !current_hidden);
        }
        if let Some(value) = show_hidden {
            self.set_show_hidden(value);
        }
    }

    fn set_category_hidden(&mut self, scope: CategoryScope, id: &str, hidden: bool) {
        let result = match scope {
            CategoryScope::Video => self.db.set_category_hidden(id, hidden),
            CategoryScope::Photo => self.db.set_photo_category_hidden(id, hidden),
            CategoryScope::Document => self.db.set_document_category_hidden(id, hidden),
        };
        if let Err(error) = result {
            self.report_error("app", error);
            return;
        }
        match scope {
            CategoryScope::Video => self.reload_categories(),
            CategoryScope::Photo => self.reload_photo_categories(),
            CategoryScope::Document => self.reload_document_categories(),
        }
        self.status = if hidden {
            "Category hidden".to_string()
        } else {
            "Category shown".to_string()
        };
    }

    fn set_show_hidden(&mut self, show: bool) {
        self.show_hidden = show;
        self.reload_categories();
        self.reload_photo_categories();
        self.reload_document_categories();
        self.status = if show {
            "Showing hidden categories".to_string()
        } else {
            "Hiding hidden categories".to_string()
        };
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(8.0);

            if self.tab == MediaTab::Photos {
                self.photo_top_bar(ui);
                ui.add_space(8.0);
                return;
            }
            if self.tab == MediaTab::Documents {
                self.document_top_bar(ui);
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
                    .selectable_label(self.tab == MediaTab::Photos, "Photos")
                    .clicked()
                {
                    self.tab = MediaTab::Photos;
                }
                if ui
                    .selectable_label(self.tab == MediaTab::Documents, "Documents")
                    .clicked()
                {
                    self.tab = MediaTab::Documents;
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

                ui.menu_button(format!("Search: {}", self.search_mode.label()), |ui| {
                    for mode in SearchMode::ALL {
                        if ui
                            .selectable_label(self.search_mode == mode, mode.label())
                            .clicked()
                        {
                            chosen_mode = mode;
                        }
                    }
                });
                ui.menu_button(format!("Source: {}", self.filters.source.label()), |ui| {
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
                    .map(|path| format!("Category: {path}"))
                    .unwrap_or_else(|| "Category: Home".to_string());
                ui.menu_button(category_label, |ui| {
                    self.category_menu_contents(ui, CategoryScope::Video);
                });

                ui.menu_button("Playback", |ui| {
                    ui.checkbox(&mut self.audio_only, "Audio only");
                    ui.checkbox(&mut self.detached_playback, "Separate window (no IPC)");
                    ui.checkbox(&mut self.shuffle, "Shuffle");
                    ui.separator();
                    ui.menu_button("Quality", |ui| {
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

                if self.filters.active() && ui.small_button("Clear").clicked() {
                    clear_filters = true;
                }

                if search_focused {
                    search_rect = Some(response_rect);
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let scan_label = if self.scanning { "Scanning…" } else { "Scan" };

                    if ui
                        .add_enabled(!self.scanning, egui::Button::new(scan_label))
                        .clicked()
                    {
                        self.start_scan();
                    }

                    if ui.button("Import").clicked() {
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
        let mut chosen_mode = self.search_mode;
        let mut search_rect = None;

        ui.horizontal(|ui| {
            if ui
                .selectable_label(self.tab == MediaTab::Videos, "Videos")
                .clicked()
            {
                self.tab = MediaTab::Videos;
            }
            if ui
                .selectable_label(self.tab == MediaTab::Photos, "Photos")
                .clicked()
            {
                self.tab = MediaTab::Photos;
            }
            if ui
                .selectable_label(self.tab == MediaTab::Documents, "Documents")
                .clicked()
            {
                self.tab = MediaTab::Documents;
            }
            ui.separator();

            let response = ui.add(
                egui::TextEdit::singleline(&mut self.photo_filters.name)
                    .id(egui::Id::unique(NAME_FILTER_ID))
                    .hint_text("Search photo…")
                    .desired_width(240.0),
            );
            if response.changed() {
                self.search_selected = 0;
                self.photo_filter_dirty = true;
            }
            let search_focused = response.has_focus();
            let response_rect = response.rect;

            ui.menu_button(format!("Search: {}", self.search_mode.label()), |ui| {
                for mode in SearchMode::ALL {
                    if ui
                        .selectable_label(self.search_mode == mode, mode.label())
                        .clicked()
                    {
                        chosen_mode = mode;
                    }
                }
            });
            if chosen_mode != self.search_mode {
                self.set_search_mode(chosen_mode);
                self.focus_search(ui.ctx());
            }

            let category_label = self
                .current_photo_category_path()
                .map(|path| format!("Category: {path}"))
                .unwrap_or_else(|| "Category: Home".to_string());
            ui.menu_button(category_label, |ui| {
                self.category_menu_contents(ui, CategoryScope::Photo);
            });

            if let Some(prefix) = &self.photo_filters.path_prefix {
                let label = path_search_label(prefix);
                if ui.small_button(format!("Folder: {label}  ×")).clicked() {
                    self.photo_filters.path_prefix = None;
                    self.photo_filter_dirty = true;
                }
            }

            if self.photo_filters.active() && ui.small_button("Clear").clicked() {
                clear_filters = true;
            }

            if search_focused {
                search_rect = Some(response_rect);
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let scan_label = if self.photo_scanning {
                    "Scanning…"
                } else {
                    "Scan photos"
                };
                if ui
                    .add_enabled(!self.photo_scanning, egui::Button::new(scan_label))
                    .clicked()
                {
                    self.start_photo_scan();
                }
            });
        });

        if let Some(rect) = search_rect
            && self.search_mode != SearchMode::Name
            && !self.photo_filters.name.trim().is_empty()
        {
            self.search_results_popup(ui, rect);
        }

        if clear_filters {
            self.photo_filters.clear();
            self.photo_filter_dirty = true;
        }
    }

    fn document_top_bar(&mut self, ui: &mut egui::Ui) {
        let mut clear_filters = false;
        let mut chosen_mode = self.search_mode;
        let mut search_rect = None;

        ui.horizontal(|ui| {
            if ui
                .selectable_label(self.tab == MediaTab::Videos, "Videos")
                .clicked()
            {
                self.tab = MediaTab::Videos;
            }
            if ui
                .selectable_label(self.tab == MediaTab::Photos, "Photos")
                .clicked()
            {
                self.tab = MediaTab::Photos;
            }
            if ui
                .selectable_label(self.tab == MediaTab::Documents, "Documents")
                .clicked()
            {
                self.tab = MediaTab::Documents;
            }
            ui.separator();

            let response = ui.add(
                egui::TextEdit::singleline(&mut self.document_filters.name)
                    .id(egui::Id::unique(NAME_FILTER_ID))
                    .hint_text("Search document…")
                    .desired_width(240.0),
            );
            if response.changed() {
                self.search_selected = 0;
                self.document_filter_dirty = true;
            }
            let search_focused = response.has_focus();
            let response_rect = response.rect;

            ui.menu_button(format!("Search: {}", self.search_mode.label()), |ui| {
                for mode in SearchMode::ALL {
                    if ui
                        .selectable_label(self.search_mode == mode, mode.label())
                        .clicked()
                    {
                        chosen_mode = mode;
                    }
                }
            });
            if chosen_mode != self.search_mode {
                self.set_search_mode(chosen_mode);
                self.focus_search(ui.ctx());
            }

            let category_label = self
                .current_document_category_path()
                .map(|path| format!("Category: {path}"))
                .unwrap_or_else(|| "Category: Home".to_string());
            ui.menu_button(category_label, |ui| {
                self.category_menu_contents(ui, CategoryScope::Document);
            });

            if let Some(prefix) = &self.document_filters.path_prefix {
                let label = path_search_label(prefix);
                if ui.small_button(format!("Folder: {label}  ×")).clicked() {
                    self.document_filters.path_prefix = None;
                    self.document_filter_dirty = true;
                }
            }

            if self.document_filters.active() && ui.small_button("Clear").clicked() {
                clear_filters = true;
            }

            if search_focused {
                search_rect = Some(response_rect);
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let scan_label = if self.document_scanning {
                    "Scanning…"
                } else {
                    "Scan documents"
                };
                if ui
                    .add_enabled(!self.document_scanning, egui::Button::new(scan_label))
                    .clicked()
                {
                    self.start_document_scan();
                }
            });
        });

        if let Some(rect) = search_rect
            && self.search_mode != SearchMode::Name
            && !self.document_filters.name.trim().is_empty()
        {
            self.search_results_popup(ui, rect);
        }

        if clear_filters {
            self.document_filters.clear();
            self.document_filter_dirty = true;
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
                    ui.label(RichText::new("No photos").weak());
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!(
                            "Press Scan photos to index {}",
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
                    ui.label(RichText::new("No results").weak());
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
                                    "PHOTO",
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
                                if ui.button("Open in viewer").clicked() {
                                    menu_action = Some("open");
                                    ui.close();
                                }
                                if ui.button("Assign categories…").clicked() {
                                    menu_action = Some("categories");
                                    ui.close();
                                }
                                if ui.button("Info…").clicked() {
                                    menu_action = Some("info");
                                    ui.close();
                                }
                                if local_path.is_some()
                                    && ui.button("Regenerate thumbnail").clicked()
                                {
                                    menu_action = Some("regen");
                                    ui.close();
                                }
                                if local_path.is_some() && ui.button("Set as wallpaper").clicked() {
                                    menu_action = Some("wallpaper");
                                    ui.close();
                                }
                                ui.separator();
                                if ui.button("Remove from library").clicked() {
                                    menu_action = Some("remove");
                                    ui.close();
                                }
                                if local_path.is_some() && ui.button("Delete from disk").clicked() {
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
                                _ => "Photo".to_string(),
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
                    "wallpaper" => self.set_wallpaper(&photo_id),
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

    fn document_viewer_targets(&self, clicked: &str) -> Vec<String> {
        if self.document_selected.contains(clicked) {
            let mut ids = vec![clicked.to_string()];
            for id in self.document_selected_ids() {
                if id != clicked {
                    ids.push(id);
                }
            }
            ids
        } else {
            vec![clicked.to_string()]
        }
    }

    fn document_grid(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default_margins().show(ui, |ui| {
            if self.documents.is_empty() {
                ui.add_space(80.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No documents").weak());
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!(
                            "Press Scan documents to index {}",
                            self.config.documents_root.display()
                        ))
                        .small()
                        .weak(),
                    );
                });
                return;
            }

            if self.document_filtered.is_empty() {
                ui.add_space(80.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No results").weak());
                });
                return;
            }

            let mut selection_click: Option<(String, bool)> = None;
            let mut focus_click: Option<String> = None;
            let mut hover_click: Option<String> = None;
            let mut card_action: Option<(usize, &'static str)> = None;
            let mut open_document: Option<String> = None;
            let mut to_generate: Vec<(String, PathBuf)> = Vec::new();
            let additive_select = ui.input(|state| state.modifiers.ctrl || state.modifiers.command);
            let pointer_moved = ui.input(|state| state.pointer.delta() != egui::Vec2::ZERO);

            ui.spacing_mut().item_spacing = egui::vec2(CARD_GAP, 18.0);
            let available_width = ui.available_width();
            let columns = (((available_width + CARD_GAP) / (CARD_MIN_WIDTH + CARD_GAP)).floor()
                as usize)
                .max(1);
            self.document_grid_columns = columns;
            let card_width = ((available_width - CARD_GAP * (columns.saturating_sub(1)) as f32)
                / columns as f32)
                .max(120.0);
            let card_height = (card_width * CARD_RATIO).round();
            let available_height = ui.available_height();
            let rows = ((available_height / (card_height + 18.0)).floor() as usize).max(1);
            let page_size = (rows * columns).max(1);
            self.document_page_size = page_size;
            let page_count = self.document_filtered.len().div_ceil(page_size).max(1);
            if self.document_scroll_to_focus
                && let Some(position) = self.document_focus_index()
            {
                self.document_page = position / page_size;
            }
            self.document_page = self.document_page.min(page_count - 1);
            self.document_scroll_to_focus = false;
            let start = self.document_page * page_size;
            let end = (start + page_size).min(self.document_filtered.len());
            for row in self.document_filtered[start..end].chunks(columns) {
                ui.horizontal_top(|ui| {
                    for &index in row {
                        let document = &self.documents[index];
                        let selected = self.document_selected.contains(&document.id);
                        let focused =
                            self.document_focus_id.as_deref() == Some(document.id.as_str());
                        let local_path = document.path(&self.config.documents_root);
                        let thumb = self.document_thumb_path(document);

                        if thumb.is_none()
                            && let Some(path) = &local_path
                            && !self.document_pending_thumbs.contains(&document.id)
                            && !self.document_failed_thumbs.contains(&document.id)
                        {
                            to_generate.push((document.id.clone(), path.clone()));
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
                                    "PDF",
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
                                if self.document_scroll_to_focus {
                                    ui.scroll_to_rect(rect, Some(Align::Center));
                                }
                            }

                            if pointer_moved && response.hovered() {
                                hover_click = Some(document.id.clone());
                            }

                            if response.clicked() && !response.double_clicked() {
                                selection_click = Some((document.id.clone(), additive_select));
                                focus_click = Some(document.id.clone());
                            }

                            if response.double_clicked() {
                                selection_click = Some((document.id.clone(), false));
                                focus_click = Some(document.id.clone());
                                open_document = Some(document.id.clone());
                            }

                            let mut menu_action = None;
                            response.context_menu(|ui| {
                                if ui.button("Open with zathura").clicked() {
                                    menu_action = Some("open");
                                    ui.close();
                                }
                                if ui.button("Assign categories…").clicked() {
                                    menu_action = Some("categories");
                                    ui.close();
                                }
                                if ui.button("Info…").clicked() {
                                    menu_action = Some("info");
                                    ui.close();
                                }
                                if local_path.is_some()
                                    && ui.button("Regenerate thumbnail").clicked()
                                {
                                    menu_action = Some("regen");
                                    ui.close();
                                }
                                ui.separator();
                                if ui.button("Remove from library").clicked() {
                                    menu_action = Some("remove");
                                    ui.close();
                                }
                                if local_path.is_some() && ui.button("Delete from disk").clicked() {
                                    menu_action = Some("delete");
                                    ui.close();
                                }
                            });

                            if let Some(action) = menu_action {
                                card_action = Some((index, action));
                            }

                            ui.add(
                                egui::Label::new(RichText::new(&document.title).size(12.5))
                                    .truncate(),
                            );
                            ui.add(
                                egui::Label::new(RichText::new("PDF").size(10.0).weak()).truncate(),
                            );
                        });
                    }
                });
            }

            for (id, path) in to_generate {
                self.document_pending_thumbs.insert(id.clone());
                self.document_thumbs.enqueue(&id, &path);
            }

            if let Some(id) = hover_click {
                self.document_focus_id = Some(id);
            }
            if let Some(id) = focus_click {
                self.document_focus_id = Some(id);
            }

            if let Some((id, additive)) = selection_click {
                if additive {
                    if !self.document_selected.remove(&id) {
                        self.document_selected.insert(id);
                    }
                } else if !self.document_selected.contains(&id) {
                    self.document_selected.clear();
                    self.document_selected.insert(id);
                }
            }

            if let Some((index, action)) = card_action {
                let document_id = self.documents[index].id.clone();
                match action {
                    "open" => {
                        let ids = self.document_viewer_targets(&document_id);
                        self.open_documents(&ids);
                    }
                    "categories" => {
                        self.document_category_targets = vec![document_id];
                        self.document_category_filter.clear();
                    }
                    "info" => self.open_info(MediaTab::Documents, &document_id),
                    "regen" => self.regenerate_document_thumb(&document_id),
                    "remove" => self.request_document_remove(&[document_id]),
                    "delete" => self.request_document_delete(&[document_id]),
                    _ => {}
                }
            }

            if let Some(id) = open_document {
                let ids = self.document_viewer_targets(&id);
                self.open_documents(&ids);
            }
        });
    }

    fn regenerate_document_thumb(&mut self, id: &str) {
        let Some(document) = self.documents.iter().find(|document| document.id == id) else {
            return;
        };
        let Some(path) = document.path(&self.config.documents_root) else {
            self.report_error("documents", "the document has no local file");
            return;
        };

        self.remove_document_thumb_files(id, document.thumb_key.as_deref());
        if let Some(document) = self.documents.iter_mut().find(|document| document.id == id) {
            document.thumb_key = None;
        }
        self.document_failed_thumbs.remove(id);
        self.document_pending_thumbs.insert(id.to_string());
        self.document_thumbs.enqueue(id, &path);
        self.status = "Regenerating thumbnail…".to_string();
    }

    fn reload_documents(&mut self) {
        match self.db.list_documents() {
            Ok(documents) => {
                self.documents = documents;
                self.document_selected
                    .retain(|id| self.documents.iter().any(|document| &document.id == id));
            }
            Err(error) => self.report_error("app", error),
        }
        self.reload_document_categories();
        self.document_filter_dirty = true;
    }

    fn reload_document_categories(&mut self) {
        let categories = self.db.list_document_categories();
        let assignments = self.db.list_document_assignments();

        match (categories, assignments) {
            (Ok(categories), Ok(assignments)) => {
                let hidden = if self.show_hidden {
                    HashSet::new()
                } else {
                    hidden_category_ids(&categories)
                };
                let visible: Vec<Category> = categories
                    .iter()
                    .filter(|category| !hidden.contains(&category.id))
                    .cloned()
                    .collect();
                self.document_category_paths = build_category_paths(&visible);
                self.document_category_ids_by_path = self
                    .document_category_paths
                    .iter()
                    .map(|(id, path)| (path.clone(), id.clone()))
                    .collect();
                self.document_category_children = build_category_children(&visible);
                self.document_categories = categories;

                if let CategoryFilter::Path(path) = &self.document_filters.category
                    && !self.document_category_ids_by_path.contains_key(path)
                {
                    self.document_filters.category = CategoryFilter::All;
                }

                let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
                let mut hidden_media = HashSet::new();
                for (document_id, category_id) in assignments {
                    if hidden.contains(&category_id) {
                        hidden_media.insert(document_id.clone());
                    }
                    if let Some(path) = self.document_category_paths.get(&category_id) {
                        map.entry(document_id)
                            .or_default()
                            .push((category_id, path.clone()));
                    }
                }
                for paths in map.values_mut() {
                    paths.sort_by(|left, right| left.1.cmp(&right.1));
                }
                self.document_assignments = map;
                self.hidden_documents = hidden_media;
            }
            (Err(error), _) | (_, Err(error)) => self.report_error("app", error),
        }

        self.document_filter_dirty = true;
    }

    fn refresh_document_filter(&mut self) {
        if !self.document_filter_dirty && self.document_applied_filters == self.document_filters {
            return;
        }

        let mut patterns = Vec::new();
        if self.search_mode == SearchMode::Name {
            for raw in self.document_filters.name.split_whitespace() {
                patterns.push(Pattern::parse(
                    raw,
                    CaseMatching::Ignore,
                    Normalization::Smart,
                ));
            }
        }

        let filters_changed = self.document_applied_filters != self.document_filters;
        let mut matcher = Matcher::new(MatcherConfig::DEFAULT.match_paths());
        let mut buffer = Vec::new();
        let mut scored: Vec<(u32, usize)> = Vec::new();

        for (index, document) in self.documents.iter().enumerate() {
            if self.hidden_documents.contains(&document.id) {
                continue;
            }
            let category_paths: Vec<&str> = self
                .document_assignments
                .get(&document.id)
                .map(|entries| entries.iter().map(|(_, path)| path.as_str()).collect())
                .unwrap_or_default();
            if !self.document_filters.category.matches(&category_paths) {
                continue;
            }
            if let Some(prefix) = &self.document_filters.path_prefix {
                let Some(rel_path) = &document.rel_path else {
                    continue;
                };
                if !path_matches_prefix(rel_path, prefix) {
                    continue;
                }
            }
            let mut score = 0u32;
            let mut matched = true;
            for pattern in &patterns {
                match pattern.score(Utf32Str::new(&document.title, &mut buffer), &mut matcher) {
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
        self.document_filtered = scored.into_iter().map(|(_, index)| index).collect();
        if filters_changed {
            self.document_selected.clear();
        }
        let focus_valid = self.document_focus_id.as_ref().is_some_and(|id| {
            self.document_filtered
                .iter()
                .any(|&index| &self.documents[index].id == id)
        });
        if !focus_valid {
            self.document_focus_id = self
                .document_filtered
                .first()
                .map(|&index| self.documents[index].id.clone());
        }
        self.document_applied_filters = self.document_filters.clone();
        self.document_filter_dirty = false;
    }

    fn start_document_scan(&mut self) {
        if self.document_scanning {
            return;
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        self.document_scan_rx = Some(rx);
        self.document_scanning = true;
        self.status = "Scanning documents…".to_string();
        scan::spawn_documents(
            self.config.documents_root.clone(),
            self.config.document_extensions.clone(),
            self.db_path.clone(),
            tx,
        );
    }

    fn poll_document_scan(&mut self, ctx: &egui::Context) {
        if let Some(rx) = self.document_scan_rx.clone() {
            let mut finished = None;
            loop {
                match rx.try_recv() {
                    Ok(MediaScanEvent::Progress(found)) => {
                        self.status = format!("Scanning documents… {found} files");
                    }
                    Ok(MediaScanEvent::Finished { found, error }) => {
                        finished = Some((found, error));
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = Some((0, Some("the document scan stopped".to_string())));
                        break;
                    }
                }
            }
            if let Some((found, error)) = finished {
                self.document_scanning = false;
                self.document_scan_rx = None;
                self.reload_documents();
                match error {
                    Some(error) => self.report_error("documents", error),
                    None => {
                        self.push_log(format!("Document scan: {found} documents"));
                        self.status = format!("{found} documents found");
                    }
                }
            }
        }
        if self.document_scanning {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }

    fn poll_document_thumbs(&mut self, ctx: &egui::Context) {
        let finished = self.document_thumbs.poll();
        if finished.is_empty() {
            return;
        }
        ctx.request_repaint();
        for id in finished {
            self.document_pending_thumbs.remove(&id);
            if let Some(key) = self.existing_document_thumb_key(&id) {
                let _ = self.db.set_document_thumb_key(&id, &key);
                if let Some(document) = self.documents.iter_mut().find(|document| document.id == id)
                {
                    document.thumb_key = Some(key);
                }
            } else {
                self.document_failed_thumbs.insert(id);
            }
        }
    }

    fn existing_document_thumb_key(&self, id: &str) -> Option<String> {
        for extension in ["png", "jpg"] {
            let key = format!("{id}.{extension}");
            if self.document_thumbs_dir.join(&key).is_file() {
                return Some(key);
            }
        }
        None
    }

    fn document_thumb_path(&self, document: &Document) -> Option<PathBuf> {
        if let Some(key) = &document.thumb_key
            && let Some(path) = thumbs::find(&self.document_thumbs_dir, key)
        {
            return Some(path);
        }
        let key = self.existing_document_thumb_key(&document.id)?;
        Some(self.document_thumbs_dir.join(key))
    }

    fn document_selected_ids(&self) -> Vec<String> {
        self.document_filtered
            .iter()
            .map(|&index| &self.documents[index])
            .filter(|document| self.document_selected.contains(&document.id))
            .map(|document| document.id.clone())
            .collect()
    }

    fn current_document_category_path(&self) -> Option<&str> {
        match &self.document_filters.category {
            CategoryFilter::Path(path) => Some(path.as_str()),
            _ => None,
        }
    }

    fn current_document_category_id(&self) -> Option<&str> {
        self.current_document_category_path()
            .and_then(|path| self.document_category_ids_by_path.get(path))
            .map(String::as_str)
    }

    fn current_document_children(&self) -> Vec<Category> {
        let parent_id = match &self.document_filters.category {
            CategoryFilter::All => None,
            CategoryFilter::Uncategorized => return Vec::new(),
            CategoryFilter::Path(_) => self.current_document_category_id().map(str::to_string),
        };
        self.document_category_children
            .get(&parent_id)
            .cloned()
            .unwrap_or_default()
    }

    fn set_document_category_filter(&mut self, category: CategoryFilter) {
        self.document_filters.category = category;
        self.document_filter_dirty = true;
    }

    fn navigate_to_document_category(&mut self, path: String) {
        self.document_filters = PhotoFilters {
            name: String::new(),
            category: CategoryFilter::Path(path),
            path_prefix: None,
        };
        self.search_selected = 0;
        self.document_filter_dirty = true;
    }

    fn navigate_to_document_path(&mut self, prefix: String) {
        self.document_filters = PhotoFilters {
            name: String::new(),
            category: CategoryFilter::All,
            path_prefix: Some(prefix),
        };
        self.search_selected = 0;
        self.document_filter_dirty = true;
    }

    fn open_create_document_category(&mut self) {
        self.category_dialog_scope = CategoryScope::Document;
        self.category_dialog = Some(CategoryDialog::Create {
            parent_id: self.current_document_category_id().map(str::to_string),
            parent_path: self.current_document_category_path().map(str::to_string),
            name: String::new(),
        });
    }

    fn open_rename_document_category(&mut self) {
        let (Some(id), Some(path)) = (
            self.current_document_category_id().map(str::to_string),
            self.current_document_category_path().map(str::to_string),
        ) else {
            return;
        };
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        self.category_dialog_scope = CategoryScope::Document;
        self.category_dialog = Some(CategoryDialog::Rename {
            id,
            old_path: path,
            name,
        });
    }

    fn open_delete_document_category(&mut self) {
        let (Some(id), Some(path)) = (
            self.current_document_category_id().map(str::to_string),
            self.current_document_category_path().map(str::to_string),
        ) else {
            return;
        };
        let (categories, documents) = self.document_category_subtree_stats(&id);
        self.category_dialog_scope = CategoryScope::Document;
        self.category_dialog = Some(CategoryDialog::Delete {
            id,
            path,
            categories,
            videos: documents,
        });
    }

    fn document_category_subtree_stats(&self, id: &str) -> (usize, usize) {
        let mut ids = HashSet::new();
        let mut stack = vec![id.to_string()];
        while let Some(current) = stack.pop() {
            if !ids.insert(current.clone()) {
                continue;
            }
            if let Some(children) = self.document_category_children.get(&Some(current)) {
                for child in children {
                    stack.push(child.id.clone());
                }
            }
        }
        let documents = self
            .document_assignments
            .values()
            .filter(|entries| {
                entries
                    .iter()
                    .any(|(category_id, _)| ids.contains(category_id))
            })
            .count();
        (ids.len(), documents)
    }

    fn document_category_selection_state(
        &self,
        targets: &[String],
        category_id: &str,
    ) -> CategorySelection {
        let flags: Vec<bool> = targets
            .iter()
            .map(|target| {
                self.document_assignments
                    .get(target)
                    .is_some_and(|entries| entries.iter().any(|(id, _)| id == category_id))
            })
            .collect();
        selection_state(&flags)
    }

    fn document_targets(&self, ids: &[String]) -> Vec<VideoTarget> {
        ids.iter()
            .filter_map(|id| self.documents.iter().find(|document| &document.id == id))
            .map(|document| VideoTarget {
                id: document.id.clone(),
                title: document.title.clone(),
                path: document.path(&self.config.documents_root),
                thumb_key: document.thumb_key.clone(),
            })
            .collect()
    }

    fn request_document_delete(&mut self, ids: &[String]) {
        let targets = self.document_targets(ids);
        if targets.is_empty() {
            return;
        }
        self.confirm_action = Some(ConfirmAction::DeleteDocuments(targets));
    }

    fn request_document_remove(&mut self, ids: &[String]) {
        let targets = self.document_targets(ids);
        if targets.is_empty() {
            return;
        }
        self.confirm_action = Some(ConfirmAction::RemoveDocumentRows(targets));
    }

    fn remove_document_thumb_files(&self, id: &str, thumb_key: Option<&str>) {
        if let Some(key) = thumb_key {
            let _ = fs::remove_file(self.document_thumbs_dir.join(key));
        }
        for extension in ["png", "jpg"] {
            let _ = fs::remove_file(self.document_thumbs_dir.join(format!("{id}.{extension}")));
        }
    }

    fn open_documents(&mut self, ids: &[String]) {
        let mut opened = 0;
        for id in ids {
            let Some(document) = self.documents.iter().find(|document| &document.id == id) else {
                continue;
            };
            let Some(path) = document.path(&self.config.documents_root) else {
                continue;
            };
            if let Err(error) = Command::new("zathura").arg(&path).spawn() {
                self.report_error("viewer", format!("could not run zathura: {error}"));
                return;
            }
            opened += 1;
        }
        if opened > 0 {
            self.status = format!("Opening {opened} document(s)…");
        }
    }

    fn document_change_page(&mut self, delta: isize) {
        let page_size = self.document_page_size.max(1);
        let page_count = self.document_filtered.len().div_ceil(page_size).max(1);
        let page = (self.document_page as isize + delta).clamp(0, page_count as isize - 1) as usize;
        self.document_page = page;
        if let Some(&index) = self.document_filtered.get(page * page_size) {
            self.document_focus_id = Some(self.documents[index].id.clone());
        }
    }

    fn document_focus_index(&self) -> Option<usize> {
        let id = self.document_focus_id.as_ref()?;
        self.document_filtered
            .iter()
            .position(|&index| &self.documents[index].id == id)
    }

    fn set_document_focus(&mut self, index: usize) {
        self.document_focus_id = self
            .document_filtered
            .get(index)
            .map(|&index| self.documents[index].id.clone());
        self.document_scroll_to_focus = true;
    }

    fn document_move_vertical(&mut self, delta: isize) {
        let Some(index) = self.document_focus_index() else {
            return;
        };
        if let Some(target) = vertical_focus_target(
            index,
            self.document_grid_columns,
            delta,
            self.document_filtered.len(),
        ) {
            self.set_document_focus(target);
        }
    }

    fn document_move_horizontal(&mut self, delta: isize) {
        let Some(index) = self.document_focus_index() else {
            return;
        };
        let target = index as isize + delta;
        if target >= 0 && (target as usize) < self.document_filtered.len() {
            self.set_document_focus(target as usize);
        }
    }

    fn open_document_focus(&mut self) {
        let Some(id) = self.document_focus_id.clone() else {
            return;
        };
        let ids = if self.document_selected.contains(&id) {
            self.document_selected_ids()
        } else {
            vec![id]
        };
        self.open_documents(&ids);
    }

    fn submit_document_category_dialog(&mut self) {
        let Some(dialog) = self.category_dialog.take() else {
            return;
        };
        match dialog {
            CategoryDialog::Create {
                parent_id,
                parent_path,
                name,
            } => match self
                .db
                .create_document_category(parent_id.as_deref(), &name)
            {
                Ok(_) => {
                    let path = parent_path
                        .map(|parent| format!("{parent}/{}", name.trim()))
                        .unwrap_or_else(|| name.trim().to_string());
                    self.reload_document_categories();
                    self.set_document_category_filter(CategoryFilter::Path(path.clone()));
                    self.status = format!("Document category created: {path}");
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
                match self.db.rename_document_category(&id, &name) {
                    Ok(()) => {
                        let new_path = match path_parent(&old_path) {
                            Some(parent) => format!("{parent}/{}", name.trim()),
                            None => name.trim().to_string(),
                        };
                        let filter_path = self.current_document_category_path().map(str::to_string);
                        self.reload_document_categories();
                        if let Some(path) = filter_path {
                            let updated = if path == old_path {
                                Some(new_path.clone())
                            } else {
                                path.strip_prefix(&format!("{old_path}/"))
                                    .map(|rest| format!("{new_path}/{rest}"))
                            };
                            if let Some(updated) = updated {
                                self.document_filters.category = CategoryFilter::Path(updated);
                            }
                        }
                        self.document_filter_dirty = true;
                        self.status = format!("Document category renamed: {new_path}");
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
            } => match self.db.delete_document_category_subtree(&id) {
                Ok(()) => {
                    self.reload_document_categories();
                    let parent = path_parent(&path)
                        .map(CategoryFilter::Path)
                        .unwrap_or(CategoryFilter::All);
                    self.set_document_category_filter(parent);
                    self.status = format!(
                        "Document category deleted: {categories} category(ies), {videos} document(s)"
                    );
                }
                Err(error) => self.report_error("app", error),
            },
        }
    }

    fn handle_document_keyboard(&mut self, ctx: &egui::Context) {
        let search_id = egui::Id::unique(NAME_FILTER_ID);

        if ctx.memory(|memory| memory.has_focus(search_id)) {
            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                if self.document_filters.name.is_empty() {
                    ctx.memory_mut(|memory| memory.surrender_focus(search_id));
                } else {
                    self.document_filters.name.clear();
                    self.document_filter_dirty = true;
                }
                return;
            }
            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Enter)) {
                ctx.memory_mut(|memory| memory.surrender_focus(search_id));
                self.open_document_focus();
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

        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::A)) {
            self.select_all_current();
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
            self.document_change_page(1);
            return;
        }
        if page_prev {
            self.document_change_page(-1);
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
            self.document_filters.clear();
            self.document_filter_dirty = true;
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Enter)) {
            self.open_document_focus();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Space)) {
            if let Some(id) = self.document_focus_id.clone()
                && !self.document_selected.remove(&id)
            {
                self.document_selected.insert(id);
            }
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::I)) {
            if let Some(id) = self.document_focus_id.clone() {
                self.open_info(MediaTab::Documents, &id);
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
            self.document_move_vertical(1);
        } else if up {
            self.document_move_vertical(-1);
        } else if right {
            self.document_move_horizontal(1);
        } else if left {
            self.document_move_horizontal(-1);
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, playback: &PlaybackState) {
        let mut open_help = false;
        let mut open_log = false;
        let mut open_settings = false;
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let count = match self.tab {
                    MediaTab::Videos => {
                        let total = self.videos.len();
                        if self.applied_filters.active() {
                            format!("{} of {total}", self.filtered.len())
                        } else {
                            format!("{total} videos")
                        }
                    }
                    MediaTab::Photos => {
                        let total = self.photos.len();
                        if self.photo_applied_filters.active() {
                            format!("{} of {total}", self.photo_filtered.len())
                        } else {
                            format!("{total} photos")
                        }
                    }
                    MediaTab::Documents => {
                        let total = self.documents.len();
                        if self.document_applied_filters.active() {
                            format!("{} of {total}", self.document_filtered.len())
                        } else {
                            format!("{total} documents")
                        }
                    }
                };

                ui.label(RichText::new(count).small().weak());

                let (page, page_size, total_items) = match self.tab {
                    MediaTab::Videos => {
                        (self.video_page, self.video_page_size, self.filtered.len())
                    }
                    MediaTab::Photos => (
                        self.photo_page,
                        self.photo_page_size,
                        self.photo_filtered.len(),
                    ),
                    MediaTab::Documents => (
                        self.document_page,
                        self.document_page_size,
                        self.document_filtered.len(),
                    ),
                };
                let page_size = page_size.max(1);
                let page_count = total_items.div_ceil(page_size).max(1);
                ui.separator();
                if ui.small_button("‹").clicked() {
                    match self.tab {
                        MediaTab::Videos => self.video_change_page(-1),
                        MediaTab::Photos => self.photo_change_page(-1),
                        MediaTab::Documents => self.document_change_page(-1),
                    }
                }
                ui.label(
                    RichText::new(format!("Page {}/{}", page + 1, page_count))
                        .small()
                        .weak(),
                );
                if ui.small_button("›").clicked() {
                    match self.tab {
                        MediaTab::Videos => self.video_change_page(1),
                        MediaTab::Photos => self.photo_change_page(1),
                        MediaTab::Documents => self.document_change_page(1),
                    }
                }
                if ui.small_button("Select all").clicked() {
                    self.select_all_current();
                }

                ui.separator();
                if playback.active {
                    if ui.small_button("⏮").on_hover_text("Previous").clicked() {
                        self.player.playlist_prev();
                    }
                    let (icon, hint) = if playback.paused {
                        ("▶", "Play")
                    } else {
                        ("⏸", "Pause")
                    };
                    if ui.small_button(icon).on_hover_text(hint).clicked() {
                        self.player.toggle_pause();
                    }
                    if ui.small_button("⏭").on_hover_text("Next").clicked() {
                        self.player.playlist_next();
                    }
                    if ui.small_button("⏹").on_hover_text("Stop").clicked() {
                        self.player.stop();
                    }

                    let playing_title = self
                        .playing_title
                        .clone()
                        .or_else(|| playback.title.clone())
                        .unwrap_or_default();
                    if !playing_title.is_empty() {
                        ui.add_sized(
                            [200.0, 16.0],
                            egui::Label::new(RichText::new(playing_title).small().strong())
                                .truncate(),
                        );
                    }

                    let has_selection = match self.tab {
                        MediaTab::Videos => !self.selected.is_empty(),
                        MediaTab::Photos => !self.photo_selected.is_empty(),
                        MediaTab::Documents => !self.document_selected.is_empty(),
                    };
                    let reserve = 80.0 + if has_selection { 170.0 } else { 0.0 } + 190.0;
                    let mut position = playback.position.clamp(0.0, playback.duration.max(0.0));
                    let slider_width = (ui.available_width() - reserve).max(120.0);
                    let response = ui.add_sized(
                        [slider_width, 16.0],
                        egui::Slider::new(&mut position, 0.0..=playback.duration.max(1.0))
                            .show_value(false),
                    );
                    if response.changed() {
                        self.player.seek(position);
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
                } else {
                    ui.label(RichText::new(&self.status).small().weak());
                }

                let selected_ids = match self.tab {
                    MediaTab::Videos => self.selected_ids(),
                    MediaTab::Photos => self.photo_selected_ids(),
                    MediaTab::Documents => self.document_selected_ids(),
                };
                if !selected_ids.is_empty() {
                    ui.separator();
                    if self.tab == MediaTab::Documents {
                        ui.menu_button(format!("Selection ({})", selected_ids.len()), |ui| {
                            if ui.button("Open with zathura").clicked() {
                                self.open_documents(&selected_ids);
                                ui.close();
                            }
                            if ui.button("Assign categories…").clicked() {
                                self.document_category_targets = selected_ids.clone();
                                self.document_category_filter.clear();
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Remove from library").clicked() {
                                self.request_document_remove(&selected_ids);
                                ui.close();
                            }
                            if ui.button("Delete from disk").clicked() {
                                self.request_document_delete(&selected_ids);
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Select all").clicked() {
                                self.select_all_current();
                                ui.close();
                            }
                            if ui.button("Deselect").clicked() {
                                self.document_selected.clear();
                                ui.close();
                            }
                        });
                    } else if self.tab == MediaTab::Photos {
                        ui.menu_button(format!("Selection ({})", selected_ids.len()), |ui| {
                            if ui.button("Open in viewer").clicked() {
                                self.open_photos(&selected_ids);
                                ui.close();
                            }
                            if ui.button("Set as wallpaper").clicked() {
                                if let Some(id) = selected_ids.first() {
                                    self.set_wallpaper(id);
                                }
                                ui.close();
                            }
                            if ui.button("Assign categories…").clicked() {
                                self.photo_category_targets = selected_ids.clone();
                                self.photo_category_filter.clear();
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Remove from library").clicked() {
                                self.request_photo_remove(&selected_ids);
                                ui.close();
                            }
                            if ui.button("Delete from disk").clicked() {
                                self.request_photo_delete(&selected_ids);
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Select all").clicked() {
                                self.select_all_current();
                                ui.close();
                            }
                            if ui.button("Deselect").clicked() {
                                self.photo_selected.clear();
                                ui.close();
                            }
                        });
                    } else {
                        ui.menu_button(format!("Selection ({})", selected_ids.len()), |ui| {
                            if ui.button("Play").clicked() {
                                self.open_play(selected_ids.clone());
                                ui.close();
                            }
                            ui.menu_button("Playback quality", |ui| {
                                for quality in Quality::ALL {
                                    if ui
                                        .selectable_label(
                                            self.play_quality == quality,
                                            quality.label(),
                                        )
                                        .clicked()
                                    {
                                        self.play_quality = quality;
                                        self.status = format!("Quality: {}", quality.label());
                                        ui.close();
                                    }
                                }
                            });
                            if ui.button("Download…").clicked() {
                                self.open_download(selected_ids.clone());
                                ui.close();
                            }
                            if ui.button("Assign categories…").clicked() {
                                self.category_targets = selected_ids.clone();
                                self.category_filter.clear();
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Delete from disk").clicked() {
                                self.request_delete(&selected_ids);
                                ui.close();
                            }
                            if ui.button("Remove from library").clicked() {
                                self.request_remove(&selected_ids);
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Select all").clicked() {
                                self.select_all_current();
                                ui.close();
                            }
                            if ui.button("Deselect").clicked() {
                                self.selected.clear();
                                ui.close();
                            }
                        });
                    }
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("?").on_hover_text("Help").clicked() {
                        open_help = true;
                    }
                    let log_text = if self.log_alert {
                        RichText::new("☰").color(ui.visuals().error_fg_color)
                    } else {
                        RichText::new("☰")
                    };
                    if ui.small_button(log_text).on_hover_text("Log").clicked() {
                        open_log = true;
                    }
                    if ui.small_button("⚙").on_hover_text("Settings").clicked() {
                        open_settings = true;
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
        if open_settings {
            self.settings_draft = Some(SettingsDraft::from_config(&self.config));
        }
    }

    fn grid(&mut self, ui: &mut egui::Ui) {
        if self.tab == MediaTab::Photos {
            self.photo_grid(ui);
            return;
        }
        if self.tab == MediaTab::Documents {
            self.document_grid(ui);
            return;
        }

        egui::CentralPanel::default_margins().show(ui, |ui| {
            if self.videos.is_empty() {
                ui.add_space(80.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No videos").weak());
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Press Scan to index your folder or Import to add URLs")
                            .small()
                            .weak(),
                    );
                });
                return;
            }

            if self.filtered.is_empty() {
                ui.add_space(80.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No results").weak());
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
                                if ui.button("Play").clicked() {
                                    menu_action = Some(CardAction::Play);
                                    ui.close();
                                }
                                ui.menu_button("Playback quality", |ui| {
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
                                    .add_enabled(can_download, egui::Button::new("Download…"))
                                    .clicked()
                                {
                                    menu_action = Some(CardAction::Download);
                                    ui.close();
                                }
                                if ui.button("Assign categories…").clicked() {
                                    menu_action = Some(CardAction::Categories);
                                    ui.close();
                                }
                                if ui.button("Info…").clicked() {
                                    menu_action = Some(CardAction::Info);
                                    ui.close();
                                }
                                if has_url || has_local {
                                    ui.menu_button("Regenerate thumbnail", |ui| {
                                        if has_url && ui.button("With yt-dlp").clicked() {
                                            menu_action = Some(CardAction::RegenerateYtdlp);
                                            ui.close();
                                        }
                                        if has_local && ui.button("With ffmpeg").clicked() {
                                            menu_action = Some(CardAction::RegenerateFfmpeg);
                                            ui.close();
                                        }
                                    });
                                }
                                if can_unlink && ui.button("Unlink URL").clicked() {
                                    menu_action = Some(CardAction::UnlinkUrl);
                                    ui.close();
                                }
                                ui.separator();
                                if ui.button("Delete from disk").clicked() {
                                    menu_action = Some(CardAction::Delete);
                                    ui.close();
                                }
                                if ui.button("Remove from library").clicked() {
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
                        RichText::new(format!("Downloading: {title}"))
                            .small()
                            .strong(),
                    );
                    if queued > 0 {
                        ui.label(RichText::new(format!("· {queued} queued")).small().weak());
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
                    RichText::new(format!("{} download(s) queued", self.download_queue.len()))
                        .small()
                        .weak(),
                );
            }

            if self.download_last_error.is_some() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("A download failed")
                            .small()
                            .color(ui.visuals().error_fg_color),
                    );

                    if !self.download_failed.is_empty()
                        && ui
                            .small_button(format!("Retry ({})", self.download_failed.len()))
                            .clicked()
                    {
                        retry = true;
                    }
                    if ui.small_button("Discard").clicked() {
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

    fn import_modal(&mut self, ctx: &egui::Context) {
        if !self.import_open {
            return;
        }

        let mut open = true;
        let mut start = false;

        if overlay_window(ctx, "import-modal", "Import URLs", 520.0, |ui| {
            ui.label(
                RichText::new("One URL per line. Only metadata and thumbnail are fetched.")
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
                    .add_enabled(!self.importing, egui::Button::new("Import"))
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
        }) {
            open = false;
        }

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

        if overlay_window(ctx, "categories-modal", "Assign categories", 400.0, |ui| {
            ui.label(
                RichText::new("Choose which categories the selected videos belong to.")
                    .small()
                    .weak(),
            );
            ui.add_space(4.0);
            if targets.len() > 1 {
                ui.label(
                    RichText::new(format!("{} videos selected", targets.len()))
                        .small()
                        .strong(),
                );
                ui.add_space(4.0);
            }

            ui.add(
                egui::TextEdit::singleline(&mut self.category_filter)
                    .hint_text("Filter categories…")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(6.0);

            if categories.is_empty() {
                ui.label(
                    RichText::new("No existing categories. Create one from the category bar.")
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
                                .on_hover_text("Checked: all selected. Indeterminate: some.");
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
        }) {
            open = false;
        }

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
                        format!("Category assigned to {} videos", targets.len())
                    } else {
                        "Category assigned".to_string()
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
                        format!("Category removed from {} videos", targets.len())
                    } else {
                        "Removed from category".to_string()
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

        if overlay_window(
            ctx,
            "photo-categories-modal",
            "Assign categories",
            400.0,
            |ui| {
                ui.label(
                    RichText::new("Choose which categories the selected photos belong to.")
                        .small()
                        .weak(),
                );
                ui.add_space(4.0);
                if targets.len() > 1 {
                    ui.label(
                        RichText::new(format!("{} photos selected", targets.len()))
                            .small()
                            .strong(),
                    );
                    ui.add_space(4.0);
                }

                ui.add(
                    egui::TextEdit::singleline(&mut self.photo_category_filter)
                        .hint_text("Filter categories…")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(6.0);

                if categories.is_empty() {
                    ui.label(
                        RichText::new("No photo categories. Create one from the category bar.")
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
            },
        ) {
            open = false;
        }

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
                        format!("Category assigned to {} photos", targets.len())
                    } else {
                        "Category assigned".to_string()
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
                        format!("Category removed from {} photos", targets.len())
                    } else {
                        "Removed from category".to_string()
                    };
                }
            }
        }

        if !open {
            self.photo_category_targets.clear();
            self.photo_category_filter.clear();
        }
    }

    fn document_categories_modal(&mut self, ctx: &egui::Context) {
        if self.document_category_targets.is_empty() {
            return;
        }

        let targets = self.document_category_targets.clone();

        let mut categories: Vec<(String, String)> = self
            .document_category_paths
            .iter()
            .map(|(id, path)| (id.clone(), path.clone()))
            .collect();
        categories.sort_by(|left, right| {
            left.1
                .to_lowercase()
                .cmp(&right.1.to_lowercase())
                .then_with(|| left.1.cmp(&right.1))
        });

        let filter = self.document_category_filter.trim().to_lowercase();
        if !filter.is_empty() {
            categories.retain(|(_, path)| path.to_lowercase().contains(&filter));
        }

        let mut open = true;
        let mut assign: Option<String> = None;
        let mut remove: Option<String> = None;

        if overlay_window(
            ctx,
            "document-categories-modal",
            "Assign categories",
            400.0,
            |ui| {
                ui.label(
                    RichText::new("Choose which categories the selected documents belong to.")
                        .small()
                        .weak(),
                );
                ui.add_space(4.0);
                if targets.len() > 1 {
                    ui.label(
                        RichText::new(format!("{} documents selected", targets.len()))
                            .small()
                            .strong(),
                    );
                    ui.add_space(4.0);
                }

                ui.add(
                    egui::TextEdit::singleline(&mut self.document_category_filter)
                        .hint_text("Filter categories…")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(6.0);

                if categories.is_empty() {
                    ui.label(
                        RichText::new("No document categories. Create one from the category bar.")
                            .small()
                            .weak(),
                    );
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(280.0)
                        .show(ui, |ui| {
                            for (category_id, path) in &categories {
                                let state =
                                    self.document_category_selection_state(&targets, category_id);
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
            },
        ) {
            open = false;
        }

        if let Some(category_id) = assign {
            let mut failed = None;
            for target in &targets {
                if let Err(error) = self.db.add_document_category(target, &category_id) {
                    failed = Some(error);
                    break;
                }
            }
            match failed {
                Some(error) => self.report_error("app", error),
                None => {
                    self.reload_document_categories();
                    self.status = if targets.len() > 1 {
                        format!("Category assigned to {} documents", targets.len())
                    } else {
                        "Category assigned".to_string()
                    };
                }
            }
        }

        if let Some(category_id) = remove {
            let mut failed = None;
            for target in &targets {
                if let Err(error) = self.db.remove_document_category(target, &category_id) {
                    failed = Some(error);
                    break;
                }
            }
            match failed {
                Some(error) => self.report_error("app", error),
                None => {
                    self.reload_document_categories();
                    self.status = if targets.len() > 1 {
                        format!("Category removed from {} documents", targets.len())
                    } else {
                        "Removed from category".to_string()
                    };
                }
            }
        }

        if !open {
            self.document_category_targets.clear();
            self.document_category_filter.clear();
        }
    }

    fn category_dialog_modal(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.category_dialog.take() else {
            return;
        };
        let media_label = match self.category_dialog_scope {
            CategoryScope::Video => "video(s)",
            CategoryScope::Photo => "photo(s)",
            CategoryScope::Document => "document(s)",
        };

        let title = match &dialog {
            CategoryDialog::Create { .. } => "New subcategory",
            CategoryDialog::Rename { .. } => "Rename category",
            CategoryDialog::Delete { .. } => "Delete category",
        };

        let mut open = true;
        let mut submit = false;
        let mut cancel = false;

        if overlay_window(
            ctx,
            "category-dialog-modal",
            title,
            430.0,
            |ui| match &mut dialog {
                CategoryDialog::Create {
                    parent_path, name, ..
                } => {
                    if let Some(parent) = parent_path {
                        ui.label(
                            RichText::new(format!("It will be created inside «{parent}»."))
                                .small()
                                .weak(),
                        );
                    } else {
                        ui.label(
                            RichText::new("It will be created at the top level.")
                                .small()
                                .weak(),
                        );
                    }
                    ui.add_space(4.0);
                    let response = ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text("Category name")
                            .desired_width(f32::INFINITY),
                    );
                    let enter =
                        response.has_focus() && ui.input(|state| state.key_pressed(Key::Enter));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(!name.trim().is_empty(), egui::Button::new("Create"))
                            .clicked()
                        {
                            submit = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                    if enter && !name.trim().is_empty() {
                        submit = true;
                    }
                }
                CategoryDialog::Rename { old_path, name, .. } => {
                    ui.label(RichText::new(old_path.as_str()).small().weak());
                    ui.add_space(4.0);
                    let response = ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text("New name")
                            .desired_width(f32::INFINITY),
                    );
                    let enter =
                        response.has_focus() && ui.input(|state| state.key_pressed(Key::Enter));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(!name.trim().is_empty(), egui::Button::new("Rename"))
                            .clicked()
                        {
                            submit = true;
                        }
                        if ui.button("Cancel").clicked() {
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
                        RichText::new(format!("Delete «{path}» and all its subcategories?"))
                            .strong(),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!(
                            "It contains {categories} category(ies) and {videos} {media_label}."
                        ))
                        .small()
                        .weak(),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(RichText::new("Delete").color(ui.visuals().error_fg_color))
                            .clicked()
                        {
                            submit = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                }
            },
        ) {
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

        if overlay_window(ctx, "download-modal", "Download", 460.0, |ui| {
            ui.label(RichText::new("Destination folder (existing inside Videos)").small());
            egui::ComboBox::from_id_salt("download-dir")
                .selected_text(if self.download_dir.is_empty() {
                    "Videos root".to_string()
                } else {
                    self.download_dir.clone()
                })
                .width(320.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.download_dir, String::new(), "Videos root");
                    for destination in &destinations {
                        ui.selectable_value(
                            &mut self.download_dir,
                            destination.clone(),
                            destination,
                        );
                    }
                });
            ui.label(
                RichText::new(format!("It will be saved in: {preview}"))
                    .small()
                    .weak(),
            );

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Quality:");
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
                    "Choose an existing folder or the Videos root. No new folders are created.",
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
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
                if ui.button("Download").clicked() {
                    start = true;
                }
            });
        }) {
            open = false;
        }

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

        if overlay_window(ctx, "help-modal", "Keyboard shortcuts", 430.0, |ui| {
            egui::Grid::new("keyboard-help")
                .num_columns(2)
                .spacing([18.0, 6.0])
                .show(ui, |ui| {
                    for (keys, action) in [
                        ("/", "Search by name"),
                        ("Ctrl+K", "Focus the global search"),
                        ("j / k", "Move focus down / up"),
                        ("h / l", "Move focus left / right"),
                        ("Shift+←/→", "Previous / next page"),
                        ("Arrows", "Move focus across the grid"),
                        ("Enter", "Play or jump to the search result"),
                        ("Space", "Select or deselect the focused item"),
                        ("i", "View info for the focused item"),
                        ("Esc", "Leave search, close modal or clear"),
                        ("?", "Show this help"),
                    ] {
                        ui.label(RichText::new(keys).strong().monospace());
                        ui.label(action);
                        ui.end_row();
                    }
                });
            ui.add_space(8.0);
            ui.label(
                    RichText::new(
                        "In Name mode, Enter plays the focused item. In Category or Path, use ↑/↓ and Enter to jump to a result.",
                    )
                    .small()
                    .weak(),
                );
        }) {
            self.help_open = false;
        }
    }

    fn log_modal(&mut self, ctx: &egui::Context) {
        if !self.log_open {
            return;
        }

        let mut close = false;
        let mut clear = false;

        if overlay_window(ctx, "log-modal", "Log", 760.0, |ui| {
            if self.log.is_empty() {
                ui.label(RichText::new("No activity").small().weak());
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
                if ui.button("Clear").clicked() {
                    clear = true;
                }
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        }) {
            close = true;
        }
        if clear {
            self.log.clear();
            self.log_alert = false;
        }
        if close {
            self.log_open = false;
        }
    }

    fn settings_modal(&mut self, ctx: &egui::Context) {
        let Some(mut draft) = self.settings_draft.take() else {
            return;
        };

        let mut save = false;
        let mut cancel = false;

        if overlay_window(ctx, "settings-modal", "Settings", 600.0, |ui| {
            egui::Grid::new("settings-grid")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    settings_row(ui, "Videos folder", &mut draft.root);
                    settings_row(ui, "Photos folder", &mut draft.pictures_root);
                    settings_row(ui, "Documents folder", &mut draft.documents_root);
                    settings_row(ui, "Player (mpv)", &mut draft.mpv);
                    settings_row(ui, "Downloader (yt-dlp)", &mut draft.ytdlp);
                    settings_row(ui, "ffmpeg", &mut draft.ffmpeg);
                    settings_row(ui, "yt-dlp format", &mut draft.ytdl_format);
                    settings_row(ui, "Image viewer", &mut draft.image_viewer);
                    settings_row(ui, "Wallpaper setter", &mut draft.wallpaper_setter);
                    settings_row(ui, "Python (PyMuPDF)", &mut draft.python);
                    settings_row(ui, "Thumbnail width (video)", &mut draft.thumb_width);
                    settings_row(ui, "Thumbnail width (photo)", &mut draft.photo_thumb_width);
                    settings_row(
                        ui,
                        "Thumbnail width (document)",
                        &mut draft.document_thumb_width,
                    );
                    settings_row(ui, "Video extensions", &mut draft.extensions);
                    settings_row(ui, "Photo extensions", &mut draft.photo_extensions);
                    settings_row(ui, "Document extensions", &mut draft.document_extensions);
                });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    save = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        }) {
            cancel = true;
        }

        if save {
            draft.apply_to(&mut self.config);
            for (key, value) in self.config.to_meta() {
                let _ = self.db.set_meta(&key, &value);
            }
            self.thumbs = Thumbs::new(
                self.thumbs_dir.clone(),
                self.config.ffmpeg.clone(),
                self.config.thumb_width,
                2,
            );
            self.photo_thumbs = PhotoThumbs::new(
                self.photo_thumbs_dir.clone(),
                self.config.photo_thumb_width,
                2,
            );
            self.document_thumbs = DocumentThumbs::new(
                self.document_thumbs_dir.clone(),
                self.config.document_thumb_width,
                self.config.python.clone(),
                2,
            );
            self.player = Player::new(self.config.mpv.clone(), self.config.ytdl_format.clone());
            self.status = "Settings saved".to_string();
            self.reload();
            self.reload_photos();
            self.reload_documents();
        } else if !cancel {
            self.settings_draft = Some(draft);
        }
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
                rows.push(("Title".to_string(), video.title.clone()));
                rows.push(("Status".to_string(), video.status.clone()));
                if let Some(path) = video.path(&self.config.root) {
                    rows.push(("File".to_string(), path.display().to_string()));
                    if let Ok(metadata) = fs::metadata(&path) {
                        rows.push(("Size".to_string(), format_size(metadata.len())));
                        if let Ok(modified) = metadata.modified() {
                            rows.push(("Modified".to_string(), format_system_time(modified)));
                        }
                    }
                } else if let Some(rel) = &video.rel_path {
                    rows.push(("Path".to_string(), rel.clone()));
                }
                if let Some(url) = &video.url {
                    rows.push(("URL".to_string(), url.clone()));
                }
                if let Some(channel) = &video.channel {
                    rows.push(("Channel".to_string(), channel.clone()));
                }
                if let Some(duration) = video.duration_s {
                    rows.push(("Duration".to_string(), format_duration(duration)));
                }
                if let Some(entries) = self
                    .video_categories
                    .get(&video.id)
                    .filter(|entries| !entries.is_empty())
                {
                    rows.push((
                        "Categories".to_string(),
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
                rows.push(("Title".to_string(), photo.title.clone()));
                rows.push(("Status".to_string(), photo.status.clone()));
                if let Some(path) = photo.path(&self.config.pictures_root) {
                    rows.push(("File".to_string(), path.display().to_string()));
                    if let Ok(metadata) = fs::metadata(&path) {
                        rows.push(("Size".to_string(), format_size(metadata.len())));
                        if let Ok(modified) = metadata.modified() {
                            rows.push(("Modified".to_string(), format_system_time(modified)));
                        }
                    }
                } else if let Some(rel) = &photo.rel_path {
                    rows.push(("Path".to_string(), rel.clone()));
                }
                if let (Some(width), Some(height)) = (photo.width, photo.height) {
                    rows.push(("Dimensions".to_string(), format!("{width}×{height}")));
                }
                if let Some(entries) = self
                    .photo_assignments
                    .get(&photo.id)
                    .filter(|entries| !entries.is_empty())
                {
                    rows.push((
                        "Categories".to_string(),
                        entries
                            .iter()
                            .map(|(_, path)| path.as_str())
                            .collect::<Vec<_>>()
                            .join(" · "),
                    ));
                }
            }
            MediaTab::Documents => {
                let document = self.documents.iter().find(|document| document.id == id)?;
                rows.push(("Title".to_string(), document.title.clone()));
                rows.push(("Status".to_string(), document.status.clone()));
                if let Some(path) = document.path(&self.config.documents_root) {
                    rows.push(("File".to_string(), path.display().to_string()));
                    if let Ok(metadata) = fs::metadata(&path) {
                        rows.push(("Size".to_string(), format_size(metadata.len())));
                        if let Ok(modified) = metadata.modified() {
                            rows.push(("Modified".to_string(), format_system_time(modified)));
                        }
                    }
                } else if let Some(rel) = &document.rel_path {
                    rows.push(("Path".to_string(), rel.clone()));
                }
                if let Some(entries) = self
                    .document_assignments
                    .get(&document.id)
                    .filter(|entries| !entries.is_empty())
                {
                    rows.push((
                        "Categories".to_string(),
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
            Some(MediaTab::Photos) => "Photo information",
            Some(MediaTab::Documents) => "Document information",
            _ => "Video information",
        };

        let mut open = true;
        if overlay_window(ctx, "info-modal", title, 560.0, |ui| {
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
        }) {
            open = false;
        }

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
                    format!("Delete {local} file(s) and remove {online} online entry(ies)?")
                } else if online > 0 {
                    format!("Remove {online} online entry(ies) from the library?")
                } else {
                    format!("Delete {local} file(s) from disk?")
                };
                (
                    "Delete from disk",
                    heading,
                    "Local files are deleted from disk; online entries are only removed from the library (and their thumbnail).",
                    "Delete",
                )
            }
            ConfirmAction::RemoveRows(targets) => (
                "Remove from library",
                format!("Remove {} video(s) from the library?", targets.len()),
                "Local files are not deleted from disk.",
                "Remove",
            ),
            ConfirmAction::DeletePhotos(targets) => (
                "Delete from disk",
                format!("Delete {} photo(s) from disk?", targets.len()),
                "They will also be removed from the library and their thumbnails deleted.",
                "Delete",
            ),
            ConfirmAction::RemovePhotoRows(targets) => (
                "Remove from library",
                format!("Remove {} photo(s) from the library?", targets.len()),
                "Files are not deleted from disk.",
                "Remove",
            ),
            ConfirmAction::DeleteDocuments(targets) => (
                "Delete from disk",
                format!("Delete {} document(s) from disk?", targets.len()),
                "They will also be removed from the library and their thumbnails deleted.",
                "Delete",
            ),
            ConfirmAction::RemoveDocumentRows(targets) => (
                "Remove from library",
                format!("Remove {} document(s) from the library?", targets.len()),
                "Files are not deleted from disk.",
                "Remove",
            ),
        };

        let mut open = true;
        let mut confirm = false;
        let mut cancelled = false;

        if overlay_window(ctx, "confirm-modal", title, 420.0, |ui| {
            ui.label(heading);
            ui.label(RichText::new(note).small().weak());
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .show(ui, |ui| match &action {
                    ConfirmAction::DeleteFiles(targets)
                    | ConfirmAction::DeletePhotos(targets)
                    | ConfirmAction::DeleteDocuments(targets) => {
                        for target in targets {
                            if let Some(path) = &target.path {
                                ui.label(RichText::new(path.display().to_string()).small());
                            } else {
                                ui.label(RichText::new(&target.title).small());
                            }
                        }
                    }
                    ConfirmAction::RemoveRows(targets)
                    | ConfirmAction::RemovePhotoRows(targets)
                    | ConfirmAction::RemoveDocumentRows(targets) => {
                        for target in targets {
                            ui.label(RichText::new(&target.title).small());
                        }
                    }
                });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
                let button = if matches!(
                    action,
                    ConfirmAction::DeleteFiles(_)
                        | ConfirmAction::DeletePhotos(_)
                        | ConfirmAction::DeleteDocuments(_)
                ) {
                    egui::Button::new(verb).fill(ui.visuals().error_fg_color)
                } else {
                    egui::Button::new(verb)
                };
                if ui.add(button).clicked() {
                    confirm = true;
                }
            });
        }) {
            open = false;
        }

        if confirm {
            self.apply_confirm(action);
        } else if cancelled || !open {
            self.confirm_action = None;
        }
    }

    fn apply_confirm(&mut self, action: ConfirmAction) {
        self.confirm_action = None;
        let (targets, delete_files, is_photo, is_document) = match action {
            ConfirmAction::DeleteFiles(targets) => (targets, true, false, false),
            ConfirmAction::RemoveRows(targets) => (targets, false, false, false),
            ConfirmAction::DeletePhotos(targets) => (targets, true, true, false),
            ConfirmAction::RemovePhotoRows(targets) => (targets, false, true, false),
            ConfirmAction::DeleteDocuments(targets) => (targets, true, false, true),
            ConfirmAction::RemoveDocumentRows(targets) => (targets, false, false, true),
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

            if is_document {
                self.remove_document_thumb_files(&target.id, target.thumb_key.as_deref());
                match self.db.delete_document(&target.id) {
                    Ok(()) => done += 1,
                    Err(_) => errors += 1,
                }
                self.document_selected.remove(&target.id);
            } else if is_photo {
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

        if is_document {
            self.reload_documents();
        } else if is_photo {
            self.reload_photos();
        } else {
            self.reload();
        }

        self.status = if errors == 0 {
            if delete_files {
                format!("{done} item(s) deleted")
            } else {
                format!("{done} item(s) removed from library")
            }
        } else {
            format!("{done} completed · {errors} error(s)")
        };
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_scan(&ctx);
        self.poll_photo_scan(&ctx);
        self.poll_document_scan(&ctx);
        self.poll_import();
        self.poll_download();
        self.poll_thumbs();
        self.poll_photo_thumbs();
        self.poll_document_thumbs(&ctx);
        self.poll_thumb_regen();
        self.poll_player_errors();

        if self.scanning
            || self.photo_scanning
            || self.document_scanning
            || self.importing
            || self.downloading.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(120));
        }

        let playback = self.player.snapshot();
        if playback.active {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        self.refresh_filter();
        self.refresh_photo_filter();
        self.refresh_document_filter();
        self.handle_keyboard(&ctx);
        self.top_bar(ui);
        self.status_bar(ui, &playback);
        self.download_bar(ui);
        self.grid(ui);

        self.import_modal(&ctx);
        self.categories_modal(&ctx);
        self.photo_categories_modal(&ctx);
        self.document_categories_modal(&ctx);
        self.category_dialog_modal(&ctx);
        self.download_modal(&ctx);
        self.help_modal(&ctx);
        self.log_modal(&ctx);
        self.settings_modal(&ctx);
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
        "Root".to_string()
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

fn modal_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(egui::Color32::from_gray(30))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(72)))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(14))
        .shadow(egui::epaint::Shadow {
            offset: [0, 8],
            blur: 24,
            spread: 0,
            color: egui::Color32::from_black_alpha(160),
        })
}

fn overlay_window(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    min_width: f32,
    content: impl FnOnce(&mut egui::Ui),
) -> bool {
    let mut close = false;
    egui::Modal::new(egui::Id::unique(id))
        .frame(modal_frame())
        .show(ctx, |ui| {
            ui.set_min_width(min_width);
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("✕").clicked() {
                        close = true;
                    }
                });
            });
            ui.separator();
            content(ui);
        });
    close
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
        format_duration, hidden_category_ids, list_existing_dirs, path_matches_prefix, path_parent,
        rank_search, selection_state, vertical_focus_target,
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
    fn hides_category_subtrees() {
        let categories = vec![
            Category {
                id: "1".to_string(),
                parent_id: None,
                name: "Hidden".to_string(),
                hidden: true,
            },
            Category {
                id: "2".to_string(),
                parent_id: Some("1".to_string()),
                name: "Child".to_string(),
                hidden: false,
            },
            Category {
                id: "3".to_string(),
                parent_id: Some("2".to_string()),
                name: "Grandchild".to_string(),
                hidden: false,
            },
            Category {
                id: "4".to_string(),
                parent_id: None,
                name: "Visible".to_string(),
                hidden: false,
            },
        ];
        let hidden = hidden_category_ids(&categories);
        assert!(hidden.contains("1"));
        assert!(hidden.contains("2"));
        assert!(hidden.contains("3"));
        assert!(!hidden.contains("4"));
    }

    #[test]
    fn builds_category_paths() {
        let categories = vec![
            Category {
                id: "1".to_string(),
                parent_id: None,
                name: "Pelis".to_string(),
                hidden: false,
            },
            Category {
                id: "2".to_string(),
                parent_id: Some("1".to_string()),
                name: "Accion".to_string(),
                hidden: false,
            },
        ];
        let paths = build_category_paths(&categories);
        assert_eq!(paths.get("2").map(String::as_str), Some("Pelis/Accion"));
    }
}
