use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;
use std::time::SystemTime;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use walkdir::WalkDir;

#[derive(Debug)]
struct Entry {
    name: String,
    path: PathBuf,
    size: u64,
    is_dir: bool,
    is_symlink: bool,
    /// False for a directory whose walk has not finished. Size 0 then means
    /// "unknown", not "empty". Files and symlinks are known immediately.
    scanned: bool,
    scan_error: Option<String>,
    mtime: Option<SystemTime>,
    delta: Option<i64>,
}

fn list_children(root: &Path) -> Result<(Vec<Entry>, usize), String> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut skipped = 0usize;
    let read = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(e) => return Err(format!("cannot read {}: {}", root.display(), e)),
    };
    for child in read {
        let child = match child {
            Ok(c) => c,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let path = child.path();
        // lstat, not follow. WalkDir does not follow links either, so treating
        // a symlink as a directory reported it as a successful 0-byte folder
        // and could double-count the target.
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let is_symlink = meta.file_type().is_symlink();
        let is_dir = meta.is_dir();
        let mtime = meta.modified().ok();
        let size = if is_dir { 0 } else { meta.len() };
        entries.push(Entry {
            name: child.file_name().to_string_lossy().into_owned(),
            path,
            size,
            is_dir,
            is_symlink,
            scanned: !is_dir,
            scan_error: None,
            mtime,
            delta: None,
        });
    }
    entries.sort_by_key(|e| e.name.to_lowercase());
    Ok((entries, skipped))
}

fn walk_size(root: &Path, cancel: &AtomicBool) -> (u64, usize) {
    let mut total = 0u64;
    let mut errors = 0usize;
    for entry in WalkDir::new(root).into_iter() {
        if cancel.load(Ordering::Relaxed) {
            return (total, errors);
        }
        match entry {
            Ok(e) => match e.metadata() {
                Ok(m) if m.is_file() => total += m.len(),
                Ok(_) => {}
                Err(_) => errors += 1,
            },
            Err(_) => errors += 1,
        }
    }
    (total, errors)
}

enum ScanMsg {
    /// One result per directory. `error` is set when some children could not be read.
    Item {
        index: usize,
        size: u64,
        error: Option<String>,
    },
    Finished,
}

struct Scan {
    rx: mpsc::Receiver<ScanMsg>,
    cancel: Arc<AtomicBool>,
    done: usize,
    total: usize,
}

fn human(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.1} {}", size, UNITS[unit])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    SizeDesc,
    SizeAsc,
    Name,
    Time,
}

impl SortMode {
    fn next(self) -> Self {
        match self {
            SortMode::SizeDesc => SortMode::SizeAsc,
            SortMode::SizeAsc => SortMode::Name,
            SortMode::Name => SortMode::Time,
            SortMode::Time => SortMode::SizeDesc,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortMode::SizeDesc => "size desc",
            SortMode::SizeAsc => "size asc",
            SortMode::Name => "name",
            SortMode::Time => "time",
        }
    }

    fn as_config_str(self) -> &'static str {
        match self {
            SortMode::SizeDesc => "size_desc",
            SortMode::SizeAsc => "size_asc",
            SortMode::Name => "name",
            SortMode::Time => "time",
        }
    }
}

struct App {
    cwd: PathBuf,
    entries: Vec<Entry>,
    state: ListState,
    goto: Option<String>,
    filter: String,
    filter_input: bool,
    error: Option<String>,
    scan: Option<Scan>,
    overlay: Overlay,
    sort_mode: SortMode,
    info: Option<String>,
    skipped: usize,
    /// False when `list_children` failed. An empty `entries` vec is then not
    /// proof that the directory is empty, so the snapshot must not be pruned.
    listing_ok: bool,
    bookmarks: Vec<PathBuf>,
    bookmark_cursor: usize,
    config: Config,
    theme: Theme,
    list_offset: usize,
    visible_cache: Vec<usize>,
    visible_dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Overlay {
    None,
    Help,
    Types,
    Bookmarks,
    ConfirmDelete(PathBuf),
}

impl Default for Overlay {
    fn default() -> Self {
        Overlay::None
    }
}

fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    let mut hi = haystack.chars();
    for nc in needle.chars() {
        loop {
            match hi.next() {
                Some(c) if c == nc => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

fn home_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        return Some(PathBuf::from(h));
    }
    if let Some(p) = std::env::var_os("USERPROFILE") {
        return Some(PathBuf::from(p));
    }
    None
}

fn expand_tilde(input: &str) -> PathBuf {
    // Only "~" and "~/rest". "~user" is someone else's home, not $HOME/user.
    let Some(rest) = input.strip_prefix('~') else {
        return PathBuf::from(input);
    };
    if !rest.is_empty() && !rest.starts_with('/') && !rest.starts_with(std::path::MAIN_SEPARATOR) {
        return PathBuf::from(input);
    }
    let Some(home) = home_dir() else {
        return PathBuf::from(input);
    };
    let mut p = home;
    let trimmed = rest
        .trim_start_matches('/')
        .trim_start_matches(std::path::MAIN_SEPARATOR);
    if !trimmed.is_empty() {
        p.push(trimmed);
    }
    p
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemePreset {
    Default,
    Dark,
    Light,
}

impl ThemePreset {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "default" => Some(ThemePreset::Default),
            "dark" => Some(ThemePreset::Dark),
            "light" => Some(ThemePreset::Light),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            ThemePreset::Default => "default",
            ThemePreset::Dark => "dark",
            ThemePreset::Light => "light",
        }
    }
}

#[derive(Debug, Clone)]
struct Theme {
    size: Color,
    bar: Color,
    dir: Color,
    file: Color,
    link: Color,
    delta_up: Color,
    delta_down: Color,
    error: Color,
    info: Color,
    border: Color,
    footer: Color,
    /// `Reset` leaves the terminal background alone. Light paints one so black
    /// text is readable and popups are not black-on-black.
    bg: Color,
    fg: Color,
    popup_bg: Color,
    popup_fg: Color,
}

impl Theme {
    fn preset(p: ThemePreset) -> Self {
        match p {
            ThemePreset::Default => Theme {
                size: Color::Yellow,
                bar: Color::Cyan,
                dir: Color::Blue,
                file: Color::White,
                link: Color::LightCyan,
                delta_up: Color::Green,
                delta_down: Color::Red,
                error: Color::Red,
                info: Color::Green,
                border: Color::White,
                footer: Color::DarkGray,
                bg: Color::Reset,
                fg: Color::White,
                popup_bg: Color::Black,
                popup_fg: Color::White,
            },
            ThemePreset::Dark => Theme {
                size: Color::Cyan,
                bar: Color::Magenta,
                dir: Color::Blue,
                file: Color::Gray,
                link: Color::LightMagenta,
                delta_up: Color::Green,
                delta_down: Color::Red,
                error: Color::Red,
                info: Color::Green,
                border: Color::Gray,
                footer: Color::DarkGray,
                bg: Color::Reset,
                fg: Color::Gray,
                popup_bg: Color::Black,
                popup_fg: Color::Gray,
            },
            ThemePreset::Light => Theme {
                size: Color::Blue,
                bar: Color::Magenta,
                dir: Color::Blue,
                file: Color::Black,
                link: Color::Magenta,
                delta_up: Color::Green,
                delta_down: Color::Red,
                error: Color::Red,
                info: Color::Blue,
                border: Color::Black,
                footer: Color::DarkGray,
                bg: Color::White,
                fg: Color::Black,
                popup_bg: Color::White,
                popup_fg: Color::Black,
            },
        }
    }
}

#[derive(Debug, Clone)]
struct Config {
    theme: ThemePreset,
    bar_width: usize,
    idle_tick_ms: u64,
    scan_tick_ms: u64,
    default_sort: SortMode,
}

#[derive(serde::Deserialize)]
#[serde(default)]
struct ConfigFile {
    theme: String,
    bar_width: i64,
    idle_tick_ms: i64,
    scan_tick_ms: i64,
    default_sort: String,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            theme: "default".to_string(),
            bar_width: 20,
            idle_tick_ms: 200,
            scan_tick_ms: 50,
            default_sort: "size_desc".to_string(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: ThemePreset::Default,
            bar_width: 20,
            idle_tick_ms: 200,
            scan_tick_ms: 50,
            default_sort: SortMode::SizeDesc,
        }
    }
}

fn config_file_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.toml"))
}

fn save_config(cfg: &Config) -> Result<(), String> {
    let p = config_file_path().ok_or_else(|| "config directory unavailable".to_string())?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("config: {e}"))?;
    }
    let body = format!(
        "# dush configuration. Delete this file to regenerate defaults.\n\
theme = \"{}\"           # default | dark | light\n\
bar_width = {}           # 1..100\n\
idle_tick_ms = {}       # UI refresh delay while idle\n\
scan_tick_ms = {}        # UI refresh delay while scanning\n\
default_sort = \"{}\"  # size_desc | size_asc | name | time\n",
        cfg.theme.as_str(),
        cfg.bar_width,
        cfg.idle_tick_ms,
        cfg.scan_tick_ms,
        cfg.default_sort.as_config_str(),
    );
    std::fs::write(p, body).map_err(|e| format!("config: {e}"))
}

fn load_config() -> (Config, Option<String>) {
    let mut cfg = Config::default();
    let Some(p) = config_file_path() else {
        return (cfg, None);
    };
    if !p.exists() {
        let err = save_config(&cfg).err();
        return (cfg, err);
    }
    let s = match std::fs::read_to_string(&p) {
        Ok(s) => s,
        Err(e) => return (cfg, Some(format!("config: {e}"))),
    };
    let raw: ConfigFile = match toml::from_str(&s) {
        Ok(r) => r,
        Err(e) => return (cfg, Some(format!("config: {e}"))),
    };
    if let Some(preset) = ThemePreset::parse(&raw.theme) {
        cfg.theme = preset;
    }
    if raw.bar_width > 0 && raw.bar_width <= 100 {
        cfg.bar_width = raw.bar_width as usize;
    }
    if raw.idle_tick_ms > 0 {
        cfg.idle_tick_ms = raw.idle_tick_ms as u64;
    }
    if raw.scan_tick_ms > 0 {
        cfg.scan_tick_ms = raw.scan_tick_ms as u64;
    }
    cfg.default_sort = match raw.default_sort.as_str() {
        "size_desc" => SortMode::SizeDesc,
        "size_asc" => SortMode::SizeAsc,
        "name" => SortMode::Name,
        "time" => SortMode::Time,
        _ => cfg.default_sort,
    };
    (cfg, None)
}

fn config_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        let mut p = PathBuf::from(x);
        p.push("dush");
        return Some(p);
    }
    #[cfg(windows)]
    {
        if let Some(a) = std::env::var_os("APPDATA") {
            let mut p = PathBuf::from(a);
            p.push("dush");
            return Some(p);
        }
    }
    if let Some(h) = home_dir() {
        #[cfg(windows)]
        {
            let mut p = h;
            p.push("AppData");
            p.push("Roaming");
            p.push("dush");
            return Some(p);
        }
        #[cfg(not(windows))]
        {
            let mut p = h;
            p.push(".config");
            p.push("dush");
            return Some(p);
        }
    }
    None
}

fn cache_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        let mut p = PathBuf::from(x);
        p.push("dush");
        return Some(p);
    }
    #[cfg(windows)]
    {
        if let Some(a) = std::env::var_os("LOCALAPPDATA") {
            let mut p = PathBuf::from(a);
            p.push("dush");
            return Some(p);
        }
    }
    if let Some(h) = home_dir() {
        #[cfg(windows)]
        {
            let mut p = h;
            p.push("AppData");
            p.push("Local");
            p.push("dush");
            return Some(p);
        }
        #[cfg(not(windows))]
        {
            let mut p = h;
            p.push(".cache");
            p.push("dush");
            return Some(p);
        }
    }
    None
}

fn bookmarks_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("bookmarks"))
}

fn load_bookmarks() -> Vec<PathBuf> {
    let Some(p) = bookmarks_path() else {
        return Vec::new();
    };
    match std::fs::read_to_string(&p) {
        Ok(s) => s
            .lines()
            .map(|l| PathBuf::from(l.trim()))
            .filter(|p| !p.as_os_str().is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn save_bookmarks(bookmarks: &[PathBuf]) {
    let Some(p) = bookmarks_path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body: String = bookmarks
        .iter()
        .map(|b| format!("{}\n", b.display()))
        .collect();
    let _ = std::fs::write(&p, body);
}

fn snapshot_path() -> Option<PathBuf> {
    cache_dir().map(|d| d.join("snapshot.json"))
}

fn load_snapshot() -> std::collections::HashMap<String, u64> {
    let Some(p) = snapshot_path() else {
        return Default::default();
    };
    match std::fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Default::default(),
    }
}

fn save_snapshot(map: &std::collections::HashMap<String, u64>) {
    let Some(p) = snapshot_path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(map) {
        let _ = std::fs::write(&p, s);
    }
}

impl App {
    fn new(root: PathBuf) -> Self {
        let (config, config_error) = load_config();
        let theme = Theme::preset(config.theme);
        let mut app = Self {
            cwd: root,
            entries: Vec::new(),
            state: ListState::default(),
            goto: None,
            filter: String::new(),
            filter_input: false,
            error: None,
            scan: None,
            overlay: Overlay::None,
            sort_mode: config.default_sort,
            info: None,
            skipped: 0,
            listing_ok: false,
            bookmarks: load_bookmarks(),
            bookmark_cursor: 0,
            config,
            theme,
            list_offset: 0,
            visible_cache: Vec::new(),
            visible_dirty: true,
        };
        app.refresh();
        // refresh clears a successful directory read; keep a config problem visible.
        if app.error.is_none() {
            app.error = config_error;
        }
        app
    }

    fn cycle_theme(&mut self) {
        self.config.theme = match self.config.theme {
            ThemePreset::Default => ThemePreset::Dark,
            ThemePreset::Dark => ThemePreset::Light,
            ThemePreset::Light => ThemePreset::Default,
        };
        self.theme = Theme::preset(self.config.theme);
        match save_config(&self.config) {
            Ok(()) => self.info = Some(format!("theme: {}", self.config.theme.as_str())),
            Err(e) => self.error = Some(e),
        }
    }

    fn clamp_selection(&mut self) {
        let n = self.visible().len();
        let cur = self.state.selected().unwrap_or(0);
        if n == 0 {
            self.state.select(None);
        } else if cur >= n {
            self.state.select(Some(n - 1));
        }
    }

    fn visible(&mut self) -> &[usize] {
        if !self.visible_dirty {
            return &self.visible_cache;
        }
        if self.filter.is_empty() {
            self.visible_cache = (0..self.entries.len()).collect();
        } else {
            let q = self.filter.to_lowercase();
            self.visible_cache = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| fuzzy_match(&e.name.to_lowercase(), &q))
                .map(|(i, _)| i)
                .collect();
        }
        self.visible_dirty = false;
        &self.visible_cache
    }

    fn selected_index(&mut self) -> Option<usize> {
        let sel = self.state.selected();
        let v = self.visible();
        sel.and_then(|i| v.get(i).copied())
    }

    fn refilter(&mut self) {
        self.visible_dirty = true;
        let empty = self.visible().is_empty();
        self.state.select(if empty { None } else { Some(0) });
    }

    fn jump_to(&mut self, raw: &str) {
        let expanded = expand_tilde(raw.trim());
        match expanded.canonicalize() {
            Ok(p) if p.is_dir() => {
                self.cwd = p;
                self.refresh();
                self.error = None;
            }
            Ok(_) => self.error = Some(format!("not a directory: {}", raw)),
            Err(e) => self.error = Some(format!("{}: {}", raw, e)),
        }
    }

    fn cancel_scan(&mut self) {
        if let Some(scan) = self.scan.take() {
            scan.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn refresh(&mut self) {
        self.cancel_scan();
        self.filter.clear();
        self.filter_input = false;
        self.list_offset = 0;
        self.visible_dirty = true;
        let (entries, skipped) = match list_children(&self.cwd) {
            Ok(v) => {
                self.error = None;
                self.listing_ok = true;
                v
            }
            Err(e) => {
                self.error = Some(e);
                self.listing_ok = false;
                (Vec::new(), 0)
            }
        };
        self.skipped = skipped;
        self.state
            .select(if entries.is_empty() { None } else { Some(0) });
        self.entries = entries;

        let dir_jobs: Vec<(usize, PathBuf)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.is_dir)
            .map(|(i, e)| (i, e.path.clone()))
            .collect();

        if dir_jobs.is_empty() {
            self.sort_entries();
            self.apply_snapshot();
            return;
        }

        let total = dir_jobs.len();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = cancel.clone();
        let (tx, rx) = mpsc::channel();
        let jobs = Arc::new(dir_jobs);
        let next = Arc::new(AtomicUsize::new(0));
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(total);
        thread::spawn(move || {
            std::thread::scope(|s| {
                for _ in 0..workers {
                    let tx = tx.clone();
                    let jobs = jobs.clone();
                    let next = next.clone();
                    let cancel = &cancel_thread;
                    s.spawn(move || {
                        loop {
                            if cancel.load(Ordering::Relaxed) {
                                return;
                            }
                            let idx = next.fetch_add(1, Ordering::Relaxed);
                            let Some((index, path)) = jobs.get(idx).cloned() else {
                                return;
                            };
                            let (size, errs) = walk_size(&path, cancel);
                            if cancel.load(Ordering::Relaxed) {
                                return;
                            }
                            let error = (errs > 0).then(|| format!("{errs} entries unreadable"));
                            if tx.send(ScanMsg::Item { index, size, error }).is_err() {
                                return;
                            }
                        }
                    });
                }
            });
            let _ = tx.send(ScanMsg::Finished);
        });
        self.scan = Some(Scan {
            rx,
            cancel,
            done: 0,
            total,
        });
    }

    fn poll_scan(&mut self) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };
        let mut finished = false;
        loop {
            match scan.rx.try_recv() {
                Ok(ScanMsg::Item { index, size, error }) => {
                    if let Some(e) = self.entries.get_mut(index) {
                        e.size = size;
                        e.scanned = true;
                        e.scan_error = error;
                    }
                    scan.done += 1;
                }
                Ok(ScanMsg::Finished) => {
                    finished = true;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            self.scan = None;
            self.sort_entries();
            self.apply_snapshot();
        }
    }

    fn sort_entries(&mut self) {
        // Results are applied by the index captured when the scan started.
        // Reordering `entries` before Finished would write sizes onto the wrong rows.
        if self.scan.is_some() {
            return;
        }
        let selected_path = self
            .selected_index()
            .and_then(|i| self.entries.get(i))
            .map(|e| e.path.clone());
        match self.sort_mode {
            SortMode::SizeDesc => {
                self.entries.sort_by_key(|e| std::cmp::Reverse(e.size));
            }
            SortMode::SizeAsc => {
                self.entries.sort_by_key(|e| e.size);
            }
            SortMode::Name => {
                self.entries.sort_by_key(|e| e.name.to_lowercase());
            }
            SortMode::Time => {
                self.entries.sort_by(|a, b| {
                    let a_time = a.mtime.unwrap_or(SystemTime::UNIX_EPOCH);
                    let b_time = b.mtime.unwrap_or(SystemTime::UNIX_EPOCH);
                    b_time.cmp(&a_time)
                });
            }
        }
        self.visible_dirty = true;
        if let Some(p) = selected_path {
            let v: Vec<usize> = self.visible().to_vec();
            if let Some(pos) = v.iter().position(|&i| self.entries[i].path == p) {
                self.state.select(Some(pos));
            }
        }
    }

    fn down(&mut self) {
        let n = self.visible().len();
        let next = match self.state.selected() {
            Some(i) if i + 1 < n => i + 1,
            Some(i) => i,
            None if n > 0 => 0,
            None => return,
        };
        self.state.select(Some(next));
    }

    fn up(&mut self) {
        let next = match self.state.selected() {
            Some(0) | None => 0,
            Some(i) => i - 1,
        };
        self.state.select(Some(next));
    }

    fn descend(&mut self) {
        let Some(ei) = self.selected_index() else {
            return;
        };
        let Some(e) = self.entries.get(ei) else {
            return;
        };
        if e.is_dir {
            self.cwd = e.path.clone();
            self.refresh();
            return;
        }
        // Navigation follows a symlink. Size accounting does not.
        if e.is_symlink {
            let path = e.path.clone();
            match path.canonicalize() {
                Ok(target) if target.is_dir() => {
                    self.cwd = target;
                    self.refresh();
                }
                Ok(_) => self.error = Some(format!("not a directory: {}", path.display())),
                Err(err) => self.error = Some(format!("{}: {err}", path.display())),
            }
        }
    }

    fn ascend(&mut self) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            self.cwd = parent;
            self.refresh();
        }
    }

    fn toggle_sort(&mut self) {
        self.sort_mode = self.sort_mode.next();
        self.sort_entries();
    }

    fn type_breakdown(&self) -> Vec<(String, u64, usize)> {
        use std::collections::HashMap;
        let mut totals: HashMap<String, (u64, usize)> = HashMap::new();
        for e in &self.entries {
            let key = if e.is_symlink {
                "(symlink)".to_string()
            } else if e.is_dir {
                "(dirs)".to_string()
            } else {
                Path::new(&e.name)
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|s| format!(".{}", s.to_lowercase()))
                    .unwrap_or_else(|| "(no ext)".to_string())
            };
            let slot = totals.entry(key).or_insert((0, 0));
            slot.0 += e.size;
            slot.1 += 1;
        }
        let mut v: Vec<(String, u64, usize)> =
            totals.into_iter().map(|(k, (sz, n))| (k, sz, n)).collect();
        v.sort_by_key(|row| std::cmp::Reverse(row.1));
        v
    }

    fn selected_path(&mut self) -> Option<PathBuf> {
        self.selected_index()
            .and_then(|i| self.entries.get(i))
            .map(|e| e.path.clone())
    }

    fn request_delete(&mut self) {
        if let Some(p) = self.selected_path() {
            self.overlay = Overlay::ConfirmDelete(p);
        }
    }

    fn do_delete(&mut self) {
        let Overlay::ConfirmDelete(p) = self.overlay.clone() else {
            return;
        };
        self.overlay = Overlay::None;
        // symlink_metadata so a link to a directory is unlinked, not followed.
        let res = match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => std::fs::remove_file(&p),
            Ok(_) => std::fs::remove_dir_all(&p),
            Err(e) => Err(e),
        };
        match res {
            Ok(()) => {
                self.info = Some(format!("deleted {}", p.display()));
                self.error = None;
                self.refresh();
            }
            Err(e) => {
                self.error = Some(format!("delete failed: {}", e));
            }
        }
    }

    fn copy_path(&mut self) {
        if let Some(p) = self.selected_path() {
            let s = p.to_string_lossy().into_owned();
            match arboard::Clipboard::new().and_then(|mut c| c.set_text(s.clone())) {
                Ok(()) => {
                    self.info = Some(format!("copied {}", s));
                    self.error = None;
                }
                Err(e) => {
                    self.error = Some(format!("clipboard: {}", e));
                }
            }
        }
    }

    fn open_selected(&mut self) {
        if let Some(p) = self.selected_path() {
            #[cfg(target_os = "macos")]
            let prog = "open";
            #[cfg(all(unix, not(target_os = "macos")))]
            let prog = "xdg-open";
            #[cfg(target_os = "windows")]
            let prog = "explorer";
            match std::process::Command::new(prog).arg(&p).spawn() {
                Ok(_) => {
                    self.info = Some(format!("opened {}", p.display()));
                    self.error = None;
                }
                Err(e) => {
                    self.error = Some(format!("open failed: {}", e));
                }
            }
        }
    }

    fn export_json(&mut self) {
        let Some(dir) = cache_dir() else {
            self.error = Some("export: cache directory unavailable".to_string());
            return;
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.error = Some(format!("export: {e}"));
            return;
        }
        let out = dir.join("export.json");
        let scanning = self.scan.is_some();
        let items: Vec<serde_json::Value> = self
            .entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "name": e.name,
                    "path": e.path.display().to_string(),
                    "size": e.size,
                    "is_dir": e.is_dir,
                    "is_symlink": e.is_symlink,
                    "scanned": e.scanned,
                })
            })
            .collect();
        let payload = serde_json::json!({
            "cwd": self.cwd.display().to_string(),
            "scan_complete": !scanning,
            "entries": items,
        });
        match serde_json::to_string_pretty(&payload) {
            Ok(s) => match std::fs::write(&out, s) {
                Ok(()) => {
                    let mut msg = format!("exported to {}", out.display());
                    if scanning {
                        msg.push_str(" (scan still running; unscanned sizes are incomplete)");
                    }
                    self.info = Some(msg);
                    self.error = None;
                }
                Err(e) => self.error = Some(format!("export: {e}")),
            },
            Err(e) => self.error = Some(format!("export: {e}")),
        }
    }

    fn add_bookmark(&mut self) {
        let p = self.cwd.clone();
        if !self.bookmarks.contains(&p) {
            self.bookmarks.push(p.clone());
            save_bookmarks(&self.bookmarks);
            self.info = Some(format!("bookmarked {}", p.display()));
        } else {
            self.info = Some(format!("already bookmarked: {}", p.display()));
        }
    }

    fn jump_bookmark(&mut self, idx: usize) {
        if let Some(p) = self.bookmarks.get(idx).cloned() {
            match p.canonicalize() {
                Ok(c) if c.is_dir() => {
                    self.cwd = c;
                    self.refresh();
                    self.error = None;
                }
                Ok(_) => self.error = Some(format!("not a directory: {}", p.display())),
                Err(e) => self.error = Some(format!("bookmark: {}", e)),
            }
        }
    }

    fn apply_snapshot(&mut self) {
        let snap = load_snapshot();
        for e in &mut self.entries {
            if !e.scanned {
                continue;
            }
            let key = e.path.to_string_lossy().into_owned();
            if let Some(prev) = snap.get(&key) {
                let d = e.size as i64 - *prev as i64;
                if d != 0 {
                    e.delta = Some(d);
                }
            }
        }
    }

    fn write_snapshot(&self) {
        // A failed listing looks like an empty directory. Leave the file alone.
        if !self.listing_ok {
            return;
        }
        let (upserts, retain) = classify_snapshot(&self.entries);
        let mut upserted: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut map = load_snapshot();
        for (key, size) in upserts {
            upserted.insert(key.clone());
            map.insert(key, size);
        }
        // Unreadable children were omitted from `entries`. Pruning would forget
        // them even though they are still on disk.
        if self.skipped == 0 {
            let cwd = &self.cwd;
            map.retain(|k, _| !should_prune(k, cwd, &upserted, &retain));
        }
        save_snapshot(&map);
    }
}

/// Upserts are measured sizes. `retain` paths were listed but not measured yet,
/// so their previous snapshot value must survive.
fn classify_snapshot(entries: &[Entry]) -> (Vec<(String, u64)>, std::collections::HashSet<String>) {
    let mut upserts = Vec::new();
    let mut retain = std::collections::HashSet::new();
    for e in entries {
        let key = e.path.to_string_lossy().into_owned();
        if e.scanned {
            upserts.push((key, e.size));
        } else {
            retain.insert(key);
        }
    }
    (upserts, retain)
}

/// Drop a remembered path only when this directory's listing replaced it.
/// Other directories stay in the file untouched, so quit does not stat them.
fn should_prune(
    key: &str,
    cwd: &Path,
    upserted: &std::collections::HashSet<String>,
    retain: &std::collections::HashSet<String>,
) -> bool {
    if upserted.contains(key) || retain.contains(key) {
        return false;
    }
    Path::new(key).parent().is_some_and(|parent| parent == cwd)
}

fn main() -> io::Result<()> {
    let root = match std::env::args().nth(1) {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir()
            .map_err(|e| io::Error::other(format!("current directory: {e}")))?,
    };
    let root = root.canonicalize()?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(root);
    let res = run(&mut terminal, &mut app);
    app.write_snapshot();

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

fn run<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> io::Result<()> {
    loop {
        app.poll_scan();
        terminal.draw(|f| ui(f, app))?;
        let tick = if app.scan.is_some() {
            app.config.scan_tick_ms
        } else {
            app.config.idle_tick_ms
        };
        if event::poll(Duration::from_millis(tick))? {
            let ev = event::read()?;
            if let Event::Resize(_, _) = ev {
                terminal.autoresize()?;
                app.clamp_selection();
                continue;
            }
            if let Event::Key(key) = ev {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                // Clear ephemeral messages on the next keypress; the handler
                // below will set a fresh one if this key produces a new result.
                app.info = None;
                app.error = None;
                match app.overlay.clone() {
                    Overlay::ConfirmDelete(_) => {
                        match key.code {
                            KeyCode::Char('q') => return Ok(()),
                            KeyCode::Char('y') | KeyCode::Char('Y') => app.do_delete(),
                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                app.overlay = Overlay::None
                            }
                            _ => {}
                        }
                        continue;
                    }
                    Overlay::Bookmarks => {
                        match key.code {
                            KeyCode::Char('q') => return Ok(()),
                            KeyCode::Esc | KeyCode::Char('b') => {
                                app.overlay = Overlay::None;
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if !app.bookmarks.is_empty() {
                                    app.bookmark_cursor =
                                        (app.bookmark_cursor + 1).min(app.bookmarks.len() - 1);
                                }
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                app.bookmark_cursor = app.bookmark_cursor.saturating_sub(1);
                            }
                            KeyCode::Enter => {
                                let idx = app.bookmark_cursor;
                                if idx < app.bookmarks.len() {
                                    app.overlay = Overlay::None;
                                    app.jump_bookmark(idx);
                                }
                            }
                            KeyCode::Char(c) if c.is_ascii_digit() => {
                                let idx = (c as u8 - b'0') as usize;
                                if idx < app.bookmarks.len() {
                                    app.overlay = Overlay::None;
                                    app.jump_bookmark(idx);
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    Overlay::Help | Overlay::Types => match key.code {
                        KeyCode::Char('q') => return Ok(()),
                        KeyCode::Esc => {
                            app.overlay = Overlay::None;
                            continue;
                        }
                        KeyCode::Char('?') if matches!(app.overlay, Overlay::Help) => {}
                        KeyCode::Char('t') if matches!(app.overlay, Overlay::Types) => {}
                        _ => {
                            app.overlay = Overlay::None;
                            continue;
                        }
                    },
                    Overlay::None => {}
                }
                if app.goto.is_some() {
                    match key.code {
                        KeyCode::Esc => app.goto = None,
                        KeyCode::Enter => {
                            if let Some(input) = app.goto.take() {
                                app.jump_to(&input);
                            }
                        }
                        KeyCode::Backspace => {
                            if let Some(buf) = app.goto.as_mut() {
                                buf.pop();
                            }
                        }
                        KeyCode::Char(c) => {
                            if let Some(buf) = app.goto.as_mut() {
                                buf.push(c);
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                if app.filter_input {
                    match key.code {
                        KeyCode::Esc => {
                            app.filter.clear();
                            app.filter_input = false;
                            app.refilter();
                        }
                        KeyCode::Enter => app.filter_input = false,
                        KeyCode::Backspace => {
                            app.filter.pop();
                            app.refilter();
                        }
                        KeyCode::Char(c) => {
                            app.filter.push(c);
                            app.refilter();
                        }
                        _ => {}
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Char('?') => {
                        app.overlay = match app.overlay {
                            Overlay::Help => Overlay::None,
                            _ => Overlay::Help,
                        }
                    }
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Esc => {
                        if !app.filter.is_empty() {
                            app.filter.clear();
                            app.refilter();
                        } else {
                            return Ok(());
                        }
                    }
                    KeyCode::Char('g') => app.goto = Some(String::new()),
                    KeyCode::Char('/') => app.filter_input = true,
                    KeyCode::Char('s') => app.toggle_sort(),
                    KeyCode::Char('t') => {
                        app.overlay = match app.overlay {
                            Overlay::Types => Overlay::None,
                            _ => Overlay::Types,
                        }
                    }
                    KeyCode::Char('b') => {
                        app.overlay = match app.overlay {
                            Overlay::Bookmarks => Overlay::None,
                            _ => {
                                app.bookmark_cursor = 0;
                                Overlay::Bookmarks
                            }
                        }
                    }
                    KeyCode::Char('B') => app.add_bookmark(),
                    KeyCode::Char('d') => app.request_delete(),
                    KeyCode::Char('y') => app.copy_path(),
                    KeyCode::Char('o') => app.open_selected(),
                    KeyCode::Char('e') => app.export_json(),
                    KeyCode::Char('T') => app.cycle_theme(),
                    KeyCode::Down | KeyCode::Char('j') => app.down(),
                    KeyCode::Up | KeyCode::Char('k') => app.up(),
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => app.descend(),
                    KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => app.ascend(),
                    _ => {}
                }
            }
        }
    }
}

fn centered_rect(area: ratatui::layout::Rect, w: u16, h: u16) -> ratatui::layout::Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    let x = area.x.saturating_add(area.width.saturating_sub(w) / 2);
    let y = area.y.saturating_add(area.height.saturating_sub(h) / 2);
    ratatui::layout::Rect::new(x, y, w, h)
}

fn render_popup(
    f: &mut Frame,
    area: ratatui::layout::Rect,
    title: &str,
    theme: &Theme,
    lines: Vec<Line>,
) {
    let panel = Style::default().bg(theme.popup_bg).fg(theme.popup_fg);
    f.render_widget(ratatui::widgets::Clear, area);
    let popup = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(theme.border).bg(theme.popup_bg))
                .style(panel),
        )
        .style(panel);
    f.render_widget(popup, area);
}

fn path_kind(path: &Path) -> &'static str {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => "symlink",
        Ok(m) if m.is_dir() => "directory",
        _ => "file",
    }
}

fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        if word_len > width {
            if current_len > 0 {
                lines.push(std::mem::take(&mut current));
            }
            let mut chunk = String::new();
            let mut chunk_len = 0usize;
            for c in word.chars() {
                if chunk_len == width {
                    lines.push(std::mem::take(&mut chunk));
                    chunk_len = 0;
                }
                chunk.push(c);
                chunk_len += 1;
            }
            current = chunk;
            current_len = chunk_len;
            continue;
        }
        if current_len > 0 && current_len + 1 + word_len > width {
            lines.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if current_len > 0 {
            current.push(' ');
            current_len += 1;
        }
        current.push_str(word);
        current_len += word_len;
    }
    if current_len > 0 {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn footer_content(app: &App) -> (String, Color) {
    if let Some(buf) = &app.goto {
        (format!("goto: {buf}_"), app.theme.info)
    } else if app.filter_input {
        (format!("filter: {}_", app.filter), app.theme.info)
    } else if !app.filter.is_empty() {
        (
            format!("filter: {}  (/ edit · esc clear)", app.filter),
            app.theme.bar,
        )
    } else if let Some(err) = &app.error {
        (err.clone(), app.theme.error)
    } else if let Some(info) = &app.info {
        (info.clone(), app.theme.info)
    } else {
        (FOOTER_HINT.to_string(), app.theme.footer)
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let base = Style::default().bg(app.theme.bg).fg(app.theme.fg);
    f.render_widget(Block::default().style(base), f.area());

    let (footer_text, footer_fg) = footer_content(app);
    let footer_lines = wrap_lines(&footer_text, f.area().width as usize);
    // Keep the header and at least a few list rows. The hint grows downward
    // instead of being clipped to a single line.
    let max_footer = f.area().height.saturating_sub(6).max(1);
    let footer_h = (footer_lines.len() as u16).clamp(1, max_footer);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(footer_h),
        ])
        .split(f.area());

    let visible: Vec<usize> = app.visible().to_vec();
    let total: u64 = visible.iter().map(|&i| app.entries[i].size).sum();
    let title = match &app.scan {
        Some(s) => format!(
            "dush — scanning {}/{} · {}",
            s.done,
            s.total,
            app.sort_mode.label()
        ),
        None => format!("dush — sort: {}", app.sort_mode.label()),
    };
    let count_suffix = if !app.filter.is_empty() {
        format!("  [{}/{}]", visible.len(), app.entries.len())
    } else if app.skipped > 0 {
        format!("  ({} unreadable)", app.skipped)
    } else {
        String::new()
    };
    let header = Paragraph::new(format!(
        "{}  ({}){}",
        app.cwd.display(),
        human(total),
        count_suffix
    ))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(app.theme.border).bg(app.theme.bg))
            .style(base),
    )
    .style(base);
    f.render_widget(header, chunks[0]);

    let max = visible
        .iter()
        .map(|&i| app.entries[i].size)
        .max()
        .unwrap_or(1)
        .max(1);
    let bar_width: usize = app.config.bar_width;

    // Virtualized rendering: only build ListItems for the visible window.
    // Keep the selection in view, scrolling one line at a time past the edges.
    let inner_h = chunks[1].height.saturating_sub(2) as usize; // borders
    let n = visible.len();
    let sel = app.state.selected().unwrap_or(0);
    if n == 0 || inner_h == 0 {
        app.list_offset = 0;
    } else if sel < app.list_offset {
        app.list_offset = sel;
    } else if sel >= app.list_offset + inner_h {
        app.list_offset = sel.saturating_sub(inner_h) + 1;
    }
    app.list_offset = app.list_offset.min(n.saturating_sub(1));
    let offset = app.list_offset;
    let end = (offset + inner_h).min(n);
    let window: Vec<ListItem<'_>> = visible[offset..end]
        .iter()
        .map(|&i| {
            let e = &app.entries[i];
            let filled = ((e.size as f64 / max as f64) * bar_width as f64).round() as usize;
            let filled = filled.min(bar_width);
            let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_width - filled));
            let suffix = if e.is_symlink {
                "@"
            } else if e.is_dir {
                "/"
            } else {
                ""
            };
            let name_col = if e.is_symlink {
                app.theme.link
            } else if e.is_dir {
                app.theme.dir
            } else {
                app.theme.file
            };
            let err_span = if let Some(err) = &e.scan_error {
                vec![Span::styled(
                    format!("  ({})", err),
                    Style::default().fg(app.theme.error),
                )]
            } else {
                vec![]
            };
            let delta_span = if let Some(d) = e.delta {
                let (txt, col) = if d > 0 {
                    (format!("  +{}", human(d as u64)), app.theme.delta_up)
                } else {
                    (format!("  -{}", human((-d) as u64)), app.theme.delta_down)
                };
                vec![Span::styled(txt, Style::default().fg(col))]
            } else {
                vec![]
            };
            let size_label = if e.is_dir && !e.scanned {
                format!("{:>10}  ", "...")
            } else {
                format!("{:>10}  ", human(e.size))
            };
            let line = Line::from({
                let mut v = vec![
                    Span::styled(size_label, Style::default().fg(app.theme.size)),
                    Span::styled(bar, Style::default().fg(app.theme.bar)),
                    Span::styled(
                        format!("  {}{}", e.name, suffix),
                        Style::default().fg(name_col),
                    ),
                ];
                v.extend(delta_span);
                v.extend(err_span);
                v
            });
            ListItem::new(line)
        })
        .collect();

    let list = List::new(window)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.border).bg(app.theme.bg))
                .style(base),
        )
        .style(base)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    // Render with a local state whose selection is relative to the window slice.
    let mut local = ListState::default();
    local.select(Some(sel.saturating_sub(offset)));
    f.render_stateful_widget(list, chunks[1], &mut local);

    let footer = Paragraph::new(
        footer_lines
            .iter()
            .take(footer_h as usize)
            .map(|s| Line::from(s.as_str()))
            .collect::<Vec<_>>(),
    )
    .style(Style::default().bg(app.theme.bg).fg(footer_fg));
    f.render_widget(footer, chunks[2]);

    match &app.overlay {
        Overlay::ConfirmDelete(p) => {
            let r = centered_rect(f.area(), 64, 7);
            let kind = path_kind(p);
            let lines = vec![
                Line::from(format!("Delete this {kind}?")),
                Line::from(p.display().to_string()),
                Line::from(""),
                Line::from(vec![
                    Span::styled("y", Style::default().fg(app.theme.size)),
                    Span::raw(" confirm   "),
                    Span::styled("n / esc", Style::default().fg(app.theme.size)),
                    Span::raw(" cancel"),
                ]),
            ];
            render_popup(f, r, " Confirm delete ", &app.theme, lines);
        }
        Overlay::Types => {
            let breakdown = app.type_breakdown();
            let h = (breakdown.len() as u16 + 4).min(f.area().height);
            let w = 48u16.min(f.area().width);
            let r = centered_rect(f.area(), w, h);
            let grand: u64 = app.entries.iter().map(|e| e.size).sum();
            let mut lines: Vec<Line> = Vec::new();
            for (ext, sz, n) in &breakdown {
                let pct = if grand > 0 {
                    (*sz as f64 / grand as f64) * 100.0
                } else {
                    0.0
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:>10}  ", human(*sz)),
                        Style::default().fg(app.theme.size),
                    ),
                    Span::styled(
                        format!("{:>5.1}%  ", pct),
                        Style::default().fg(app.theme.bar),
                    ),
                    Span::raw(format!("{:>5}  ", n)),
                    Span::raw(ext),
                ]));
            }
            render_popup(
                f,
                r,
                " Types (t or esc to close, q quits) ",
                &app.theme,
                lines,
            );
        }
        Overlay::Bookmarks => {
            let rows = app.bookmarks.len().max(1) as u16;
            let h = (rows + 4).min(f.area().height);
            let r = centered_rect(f.area(), 60, h);
            let mut lines: Vec<Line> = Vec::new();
            if app.bookmarks.is_empty() {
                lines.push(Line::from(
                    "No bookmarks yet. Press B to add the current directory.",
                ));
            } else {
                let cursor = app.bookmark_cursor.min(app.bookmarks.len() - 1);
                for (i, p) in app.bookmarks.iter().enumerate() {
                    let marker = if i == cursor { ">" } else { " " };
                    let row = if i == cursor {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{marker}{i:<2} "),
                            Style::default().fg(app.theme.size),
                        ),
                        Span::styled(p.display().to_string(), row),
                    ]));
                }
            }
            render_popup(
                f,
                r,
                " Bookmarks (j/k enter, 0-9, esc close) ",
                &app.theme,
                lines,
            );
        }
        Overlay::Help => {
            let r = centered_rect(f.area(), 50, 28);
            let size = app.theme.size;
            let bar = app.theme.bar;
            let footer_col = app.theme.footer;
            let mut lines: Vec<Line> = vec![Line::from("")];
            lines.push(Line::from(vec![Span::styled(
                "Navigation",
                Style::default().add_modifier(Modifier::BOLD).fg(bar),
            )]));
            for (key, desc) in HELP_NAV {
                lines.push(help_line(key, desc, size));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::styled(
                "Actions",
                Style::default().add_modifier(Modifier::BOLD).fg(bar),
            )]));
            for (key, desc) in HELP_ACTIONS {
                lines.push(help_line(key, desc, size));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::styled(
                "Sort applies when the scan finishes",
                Style::default().fg(footer_col),
            )]));
            render_popup(f, r, " Help ", &app.theme, lines);
        }
        Overlay::None => {}
    }
}

const FOOTER_HINT: &str = "move: j/k  descend: l/enter  up: h/backspace  / filter  g goto  d delete  y copy  o open  e export  t types  T theme  b bookmarks  B add  s sort  ? help  q quit";

const HELP_NAV: &[(&str, &str)] = &[
    (" j / down", "move down"),
    (" k / up", "move up"),
    (" l / enter", "descend directory"),
    (" h / backspace", "ascend to parent"),
];

const HELP_ACTIONS: &[(&str, &str)] = &[
    (" /", "fuzzy filter"),
    (" g", "goto path"),
    (" d", "delete (with confirm)"),
    (" y", "copy path to clipboard"),
    (" o", "open with default app"),
    (" e", "export JSON to cache"),
    (" t", "type breakdown"),
    (" T", "cycle theme (saved)"),
    (" b", "bookmarks overlay"),
    (" B", "bookmark current dir"),
    (" s", "toggle sort"),
    (" ?", "toggle help"),
    (" q / esc", "quit (esc closes overlays)"),
];

fn help_line(key: &str, desc: &str, key_color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{:<18}", key), Style::default().fg(key_color)),
        Span::raw(format!("  {}", desc)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_and_units() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1024), "1.0 KB");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(1024 * 1024), "1.0 MB");
        assert_eq!(human(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(human(2 * 1024 * 1024 * 1024 * 1024), "2.0 TB");
    }

    #[test]
    fn fuzzy_match_substring_and_order() {
        assert!(fuzzy_match("hello world", "hlo"));
        assert!(fuzzy_match("hello world", "hw"));
        assert!(!fuzzy_match("hello", "hx"));
        assert!(fuzzy_match("abc", ""));
        assert!(!fuzzy_match("", "a"));
        assert!(!fuzzy_match("Hello", "h"));
        assert!(fuzzy_match("hello", "h"));
    }

    #[test]
    fn sort_mode_cycles() {
        assert_eq!(SortMode::SizeDesc.next(), SortMode::SizeAsc);
        assert_eq!(SortMode::SizeAsc.next(), SortMode::Name);
        assert_eq!(SortMode::Name.next(), SortMode::Time);
        assert_eq!(SortMode::Time.next(), SortMode::SizeDesc);
    }

    #[test]
    fn wrap_lines_breaks_on_width() {
        let lines = wrap_lines(FOOTER_HINT, 40);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| l.chars().count() <= 40));
        let joined = lines.join(" ");
        for word in FOOTER_HINT.split_whitespace() {
            assert!(joined.contains(word), "{word} missing from wrapped footer");
        }
    }

    #[test]
    fn wrap_lines_splits_a_long_word() {
        let lines = wrap_lines("abcdefghij", 4);
        assert_eq!(lines, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn sort_mode_labels() {
        assert_eq!(SortMode::SizeDesc.label(), "size desc");
        assert_eq!(SortMode::SizeAsc.label(), "size asc");
        assert_eq!(SortMode::Name.label(), "name");
        assert_eq!(SortMode::Time.label(), "time");
    }

    #[test]
    fn expand_tilde_prefix() {
        if let Some(h) = home_dir() {
            assert_eq!(expand_tilde("~"), h);
            assert_eq!(expand_tilde("~/foo"), h.join("foo"));
            assert_eq!(expand_tilde("~other"), PathBuf::from("~other"));
            assert_eq!(expand_tilde("~other/x"), PathBuf::from("~other/x"));
        }
    }

    #[test]
    fn expand_no_tilde_passthrough() {
        assert_eq!(expand_tilde("/usr/local"), PathBuf::from("/usr/local"));
        assert_eq!(
            expand_tilde("relative/path"),
            PathBuf::from("relative/path")
        );
    }

    #[test]
    fn home_dir_resolves_via_home_or_userprofile() {
        let h = home_dir();
        let via_home = std::env::var_os("HOME").map(PathBuf::from);
        let via_prof = std::env::var_os("USERPROFILE").map(PathBuf::from);
        assert_eq!(h, via_home.or(via_prof));
    }

    #[test]
    fn list_children_reads_tmpdir() {
        let dir = tempfile_dir();
        std::fs::write(dir.join("a.txt"), "aaa").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let (entries, skipped) = list_children(&dir).expect("read dir");
        assert_eq!(entries.len(), 2);
        assert_eq!(skipped, 0);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"sub"));
        let txt = entries.iter().find(|e| e.name == "a.txt").unwrap();
        assert_eq!(txt.size, 3);
        assert!(!txt.is_dir);
        assert!(txt.scanned);
        let sub = entries.iter().find(|e| e.name == "sub").unwrap();
        assert!(sub.is_dir);
        assert!(!sub.scanned);
        assert_eq!(sub.size, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn list_children_does_not_follow_symlinks() {
        let dir = tempfile_dir();
        std::fs::write(dir.join("file.txt"), "hello").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::os::unix::fs::symlink("file.txt", dir.join("link-file")).unwrap();
        std::os::unix::fs::symlink("sub", dir.join("link-dir")).unwrap();
        let (entries, skipped) = list_children(&dir).expect("read dir");
        assert_eq!(skipped, 0);
        let file_link = entries.iter().find(|e| e.name == "link-file").unwrap();
        assert!(file_link.is_symlink);
        assert!(!file_link.is_dir);
        assert!(file_link.scanned);
        assert_eq!(file_link.size, "file.txt".len() as u64);
        let dir_link = entries.iter().find(|e| e.name == "link-dir").unwrap();
        assert!(dir_link.is_symlink);
        assert!(!dir_link.is_dir);
        assert!(dir_link.scanned);
        assert_eq!(dir_link.size, "sub".len() as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_keeps_unscanned_and_records_empty() {
        let entries = vec![
            sample_entry("/work/empty", 0, true, true),
            sample_entry("/work/pending", 0, true, false),
            sample_entry("/work/file", 12, false, true),
        ];
        let (upserts, retain) = classify_snapshot(&entries);
        assert_eq!(
            upserts,
            vec![
                ("/work/empty".to_string(), 0),
                ("/work/file".to_string(), 12),
            ]
        );
        assert!(retain.contains("/work/pending"));
        assert!(!retain.contains("/work/empty"));

        let upserted: std::collections::HashSet<String> =
            upserts.iter().map(|(k, _)| k.clone()).collect();
        let cwd = Path::new("/work");
        assert!(!should_prune("/work/empty", cwd, &upserted, &retain));
        assert!(!should_prune("/work/pending", cwd, &upserted, &retain));
        assert!(should_prune("/work/gone", cwd, &upserted, &retain));
        assert!(!should_prune("/other/gone", cwd, &upserted, &retain));
    }

    fn sample_entry(path: &str, size: u64, is_dir: bool, scanned: bool) -> Entry {
        Entry {
            name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            path: PathBuf::from(path),
            size,
            is_dir,
            is_symlink: false,
            scanned,
            scan_error: None,
            mtime: None,
            delta: None,
        }
    }

    fn tempfile_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut p = std::env::temp_dir();
        p.push(format!("dush-test-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
