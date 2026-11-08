//! Aria 的 GPUI 前端(替代原 Tauri/WebView 设置窗口)。
//!
//! 结构:
//! - 曲库页:搜索 / 星级下限 / 排序 / 收藏夹 / SB 筛选 / 刷新 / 源切换,
//!   列表带封面,点击播放,右键加入播放列表;
//! - 设置页:全部会话参数(开关/步进/枚举/路径文本),改动即持久化并下发;
//! - 右侧播放列表:点击切歌、多选、右键菜单(难度/上移下移/移除);
//! - 底部播放条:上一首/暂停/下一首、±10s、进度、时长、音量;
//! - 托盘:见 `tray` 模块(独立 GTK 线程)。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{StatefulInteractiveElement, img as gpui_img};
use serde_json::json;
use gpui::{
    div, px, rgb, rgba, Application, Context, FocusHandle, KeyDownEvent, MouseButton, Render,
    Window, WindowOptions,
};

/// 应用数据目录(Linux XDG,与旧 Tauri app_data_dir 同口径)。
pub fn data_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(x).join("com.ohdmire.aria");
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".local")
        .join("share")
        .join("com.ohdmire.aria")
}

static WINDOW: Mutex<Option<gpui::WindowHandle<App>>> = Mutex::new(None);

/// 关闭主窗口时是否最小化到托盘(由 settings.close_action 同步)。
static CLOSE_TO_TRAY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CLOSE_TO_EXIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CLOSE_ASK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

static PICKED: Mutex<Vec<(Field, PathBuf)>> = Mutex::new(Vec::new());
static SHOW_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static REPLACE_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 策略:询问 / 最小化 / 退出。
fn sync_close_mode(s: &crate::settings::Settings) {
    CLOSE_TO_TRAY.store(s.close_action.as_deref() == Some("tray"), std::sync::atomic::Ordering::Relaxed);
    CLOSE_TO_EXIT.store(s.close_action.as_deref() == Some("exit"), std::sync::atomic::Ordering::Relaxed);
}

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const GOLD: u32 = 0xe0c060;

#[derive(Clone, Copy)]
struct Palette {
    bg: u32,
    panel: u32,
    line: u32,
    line2: u32,
    text: u32,
    dim: u32,
    accent: u32,
    ctl: u32,
    ctl_hover: u32,
    input: u32,
    tint_play: u32,
    tint_preview: u32,
    thumb: u32,
    row_hover: u32,
    ok_bg: u32,
    ok_fg: u32,
    mod_bg: u32,
    mod_fg: u32,
    warn: u32,
    toast_bg: u32,
    toast_fg: u32,
}

static PALETTE: Mutex<Palette> = Mutex::new(Palette {
    bg: 0x141418,
    panel: 0x1b1b20,
    line: 0x26262d,
    line2: 0x222228,
    text: 0xe6e6ea,
    dim: 0x8a8a94,
    accent: 0x30507a,
    ctl: 0x26262d,
    ctl_hover: 0x3a3f4a,
    input: 0x222228,
    tint_play: 0x252b36,
    tint_preview: 0x1e2530,
    thumb: 0x4a4a55,
    row_hover: 0x262b34,
    ok_bg: 0x21402a,
    ok_fg: 0x7fd49a,
    mod_bg: 0x2a3a2a,
    mod_fg: 0x9ad09a,
    warn: 0xf0a040,
    toast_bg: 0x3a2a12,
    toast_fg: 0xf0c080,
});

fn pal() -> Palette {
    *PALETTE.lock().unwrap()
}

fn parse_hex_color(s: &str) -> Option<u32> {
    let t = s.trim().trim_start_matches('#');
    if t.len() != 6 {
        return None;
    }
    u32::from_str_radix(t, 16).ok()
}

/// 线性混色(0 = 全 a,1 = 全 b)。
fn blend(a: u32, b: u32, t: f32) -> u32 {
    let ch = |sh: u32| {
        let x = ((a >> sh) & 0xff) as f32;
        let y = ((b >> sh) & 0xff) as f32;
        (x + (y - x) * t).round() as u32
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

fn accent_color(accent: &str) -> u32 {
    if let Some(v) = parse_hex_color(accent) {
        return v;
    }
    match accent {
        "purple" => 0x5b3a86,
        "green" => 0x2f6f52,
        "pink" => 0x8a3a63,
        "orange" => 0x8a5a2a,
        _ => 0x30507a,
    }
}

fn apply_palette(dark: bool, accent: &str) {
    let accent = accent_color(accent);
    let p = if dark {
        Palette {
            bg: 0x141418,
            panel: 0x1b1b20,
            line: 0x26262d,
            line2: 0x222228,
            text: 0xe6e6ea,
            dim: 0x8a8a94,
            accent,
            ctl: 0x26262d,
            ctl_hover: blend(accent, 0x3a3f4a, 0.55),
            input: 0x222228,
            tint_play: 0x252b36,
            tint_preview: 0x1e2530,
            thumb: 0x4a4a55,
            row_hover: blend(accent, 0x1b1b20, 0.80),
            ok_bg: 0xd7f0df,
            ok_fg: 0x1f7a45,
            mod_bg: 0xdcefdc,
            mod_fg: 0x2f6b35,
            warn: 0xb06a10,
            toast_bg: 0xfff2d8,
            toast_fg: 0x7a4a00,
        }
    } else {
        Palette {
            bg: 0xf4f4f7,
            panel: 0xffffff,
            line: 0xd2d2da,
            line2: 0xdedee6,
            text: 0x1f1f26,
            dim: 0x5e5e6a,
            accent,
            ctl: 0xe2e2ea,
            ctl_hover: blend(accent, 0xffffff, 0.72),
            input: 0xffffff,
            tint_play: 0xdbe7f7,
            tint_preview: 0xe9f0fa,
            thumb: 0xb4b4c0,
            row_hover: blend(accent, 0xffffff, 0.88),
            ok_bg: 0xd7f0df,
            ok_fg: 0x1f7a45,
            mod_bg: 0xdcefdc,
            mod_fg: 0x2f6b35,
            warn: 0xb06a10,
            toast_bg: 0xfff2d8,
            toast_fg: 0x7a4a00,
        }
    };
    *PALETTE.lock().unwrap() = p;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Field {
    Search,
    LazerDir,
    StableDir,
    Ffmpeg,
    Ffprobe,
    Skin,
    NumVolume,
    NumBgm,
    NumHits,
    NumSpeed,
    NumStar,
    NumOffset,
    NumBg,
    NumCursor,
    NumStarMin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Queue,
    Library,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Added,
    Title,
    Star,
    Artist,
    Creator,
    Size,
    CollectionOrder,
}

#[derive(Clone)]
struct Row {
    set_id: String,
    sha2: String,
    title: String,
    artist: String,
    star: f64,
    star_lo: f64,
    diffs: usize,
    /// 星级过滤后该谱面集剩余的难度数。
    diffs_in_range: usize,
    has_sb: bool,
    added: i64,
    creator: String,
    size: i64,
    status: i32,
}

#[derive(Clone)]
struct QueueTrack {
    set_id: String,
    sha2: Option<String>,
    title: String,
    artist: String,
    length_ms: f64,
    mods: u32,
}

#[derive(Default)]
struct PlayStatus {
    t: f64,
    playing: bool,
    duration: f64,
    ended: bool,
}

struct Editor {
    field: Field,
    buf: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropKind {
    Collection,
    Source,
    Sort,
    PlayMode,
    Theme,
    Accent,
    CloseAction,
    Skin,
    Monitor,
    AudioDevice,
    Upscale,
    UpscaleQuality,
    QuickMods,
    Fps,
    Diff,
    RenderMode,
    QueueDiff,
    Speed,
}

struct LibBarDrag;
struct ListBarDrag;
struct StarMinDrag;
struct StarMaxDrag;
struct SeekDrag;
struct VolumeDrag;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SliderKind {
    Seek,
    Volume,
    StarMin,
    StarMax,
}

/// 队列拖动时跟随光标的半透明预览卡片。
struct DragPreview {
    title: String,
    artist: String,
    cover: Option<Arc<gpui::Image>>,
}

impl Render for DragPreview {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(360.0))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .p_2()
            .rounded_lg()
            .bg(gpui::rgba(0x2a2f3aff))
            .border_1()
            .border_color(rgb(pal().accent))
            .opacity(0.85)
            .child(
                div()
                    .w(px(40.0))
                    .h(px(40.0))
                    .rounded_md()
                    .overflow_hidden()
                    .child(match &self.cover {
                        Some(img) => gpui::img(gpui::ImageSource::Image(img.clone()))
                            .w_full()
                            .h_full()
                            .object_fit(gpui::ObjectFit::Cover)
                            .into_any_element(),
                        None => div().size_full().bg(rgb(pal().line2)).into_any_element(),
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .child(div().text_sm().child(self.title.clone()))
                    .child(div().text_xs().text_color(rgb(pal().dim)).child(self.artist.clone())),
            )
    }
}

/// 拖动时 GPUI 要求一个预览实体;滑块不需要可见 ghost。
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size(px(1.0))
    }
}

/// 亲一个谱面难度的解析结果(参照旧版 pathcache):曲库未就绪/集合被删时
/// 仍能直接开播。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct PathEntry {
    path: String,
    manifest: Option<Vec<crate::ipc::VFile>>,
}

fn pathcache_file() -> PathBuf {
    data_dir().join("pathcache.json")
}

fn load_path_cache() -> HashMap<String, PathEntry> {
    std::fs::read_to_string(pathcache_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_path_cache(cache: &HashMap<String, PathEntry>) {
    if let Ok(json) = serde_json::to_string(cache) {
        let _ = std::fs::write(pathcache_file(), json);
    }
}

struct LibPayload {
    thumbs: HashMap<String, Option<Arc<gpui::Image>>>,
    lib: Arc<crate::lazer::LazerLibrary>,
    all_rows: Vec<Row>,
    collections: Vec<String>,
    cover_paths: HashMap<String, PathBuf>,
    skins: Vec<(String, Option<String>)>,
    status_line: String,
}

struct ModsDialog {
    set_id: String,
    sha2: Option<String>,
    mods: u32,
    /// Some(i) = 编辑已有队列条目;None = 新加入。
    queue_index: Option<usize>,
    /// 多选批量改 mods 的目标下标。
    bulk: Option<Vec<usize>>,
}

struct OverrideEdit {
    set_id: String,
    ov: crate::settings::SetOverride,
}

struct App {
    view: View,
    settings: crate::settings::Settings,
    lib: Option<Arc<crate::lazer::LazerLibrary>>,
    all_rows: Vec<Row>,
    rows: Vec<Row>,
    cover_paths: HashMap<String, PathBuf>,
    collections: Vec<String>,
    // 过滤/排序
    search: String,
    star_min: f64,
    star_max: f64,
    sort: SortKey,
    desc: bool,
    sb_only: bool,
    // 队列
    queue: Vec<QueueTrack>,
    pos: usize,
    playing: bool,
    selected: HashSet<usize>,
    select_mode: bool,
    ctx: Option<usize>,
    ctx_pos: (f32, f32),
    ctx_multi: bool,
    sel_anchor: Option<usize>,
    // 子进程 / 状态
    child: Option<crate::ctl::ChildCtl>,
    status: Arc<Mutex<PlayStatus>>,
    // 文本编辑
    edit: Option<Editor>,
    focus: HashMap<Field, FocusHandle>,
    // 反馈
    toast: Option<(String, Instant)>,
    about: bool,
    overrides: Option<OverrideEdit>,
    mods_dialog: Option<ModsDialog>,
    clear_confirm: bool,
    /// 曲库中展开难度的谱面集。
    expanded: Option<String>,
    dragging: Option<usize>,
    drag_src: Option<usize>,
    drag_over: Option<usize>,
    drag_last_y: f32,
    drag_start_y: f32,
    last_size: (f32, f32),
    settings_rev: u64,
    settings_rev_seen: std::cell::Cell<u64>,
    seek_guard: std::time::Instant,
    star_popup: bool,
    star_draft: (f64, f64),
    close_ask: bool,
    quick_opts: bool,
    edit_armed: bool,
    last_settings_off: f32,
    prog_ms: f64,
    prog_at: std::time::Instant,
    close_remember: bool,

    menu_list: gpui::ListState,
    menu_scroll: gpui::ScrollHandle,
    queue_list: gpui::ListState,
    settings_list: gpui::ListState,
    settings_scroll: gpui::ScrollHandle,
    info_scroll: gpui::ScrollHandle,
    /// 曲库还没加载完时的待播请求。
    pending_play: Option<(String, Option<String>, u32)>,
    path_cache: HashMap<String, PathEntry>,
    status_line: String,
    monitors: Vec<(String, String)>,
    audio_devs: Vec<(String, String)>,
    skins: Vec<(String, Option<String>)>,
    hero: HashMap<String, Option<Arc<gpui::Image>>>,
    hero_order: std::collections::VecDeque<String>,
    thumbs: Mutex<HashMap<String, Option<Arc<gpui::Image>>>>,
    thumb_order: Mutex<std::collections::VecDeque<String>>,
    lib_loading: Arc<Mutex<Option<LibPayload>>>,
    set_idx: HashMap<String, usize>,
    loading: bool,
    /// 右侧信息卡预览(单击行临时查看,不影响播放/队列)。
    preview: Option<(String, Option<String>)>,
    /// 上一次通知 UI 的播放状态(秒/播放中/时长),用于节流重绘。
    last_ui: (u64, bool, u64),
    /// 当前展开的下拉菜单。
    open_menu: Option<DropKind>,
    lib_scroll: gpui::UniformListScrollHandle,
    lib_list: gpui::ListState,
    queue_scroll: gpui::UniformListScrollHandle,
}

impl App {
    fn new(cx: &mut Context<Self>) -> Self {
        let dir = data_dir();
        let s = crate::settings::load_from(&dir);
        apply_palette(s.theme_mode != "light", &s.accent);
        let mut focus = HashMap::new();
        for f in [
            Field::Search,
            Field::LazerDir,
            Field::StableDir,
            Field::Ffmpeg,
            Field::Ffprobe,
            Field::Skin,
            Field::NumVolume,
            Field::NumBgm,
            Field::NumHits,
            Field::NumSpeed,
            Field::NumStar,
            Field::NumOffset,
            Field::NumBg,
            Field::NumCursor,
            Field::NumStarMin,
        ] {
            focus.insert(f, cx.focus_handle());
        }

        let mut app = Self {
            view: View::Queue,
            star_min: s.star_min,
            star_max: 0.0,
            settings: s,
            lib: None,
            all_rows: Vec::new(),
            rows: Vec::new(),
            cover_paths: HashMap::new(),
            collections: Vec::new(),
            search: String::new(),
            sort: SortKey::Added,
            desc: true,
            sb_only: false,
            queue: Vec::new(),
            pos: 0,
            playing: false,
            selected: HashSet::new(),
            select_mode: false,
            ctx: None,
            ctx_pos: (0.0, 0.0),
            ctx_multi: false,
            sel_anchor: None,
            child: None,
            status: Arc::new(Mutex::new(PlayStatus::default())),
            edit: None,
            focus,
            toast: None,
            about: false,
            overrides: None,
            mods_dialog: None,
            clear_confirm: false,
            expanded: None,
            dragging: None,
            drag_src: None,
            drag_over: None,
            drag_last_y: 0.0,
            drag_start_y: 0.0,
            last_size: (0.0, 0.0),
            settings_rev: 0,
            settings_rev_seen: std::cell::Cell::new(0),
            seek_guard: std::time::Instant::now(),
            star_popup: false,
            star_draft: (0.0, 0.0),
            close_ask: false,
            quick_opts: false,
            edit_armed: false,
            last_settings_off: 0.0,
            prog_ms: 0.0,
            prog_at: std::time::Instant::now(),
            close_remember: false,

            menu_list: gpui::ListState::new(0, gpui::ListAlignment::Top, px(100.0)),
            menu_scroll: gpui::ScrollHandle::new(),
            queue_list: gpui::ListState::new(0, gpui::ListAlignment::Top, px(300.0)),
            settings_list: gpui::ListState::new(1, gpui::ListAlignment::Top, px(200.0)),
            settings_scroll: gpui::ScrollHandle::new(),
            info_scroll: gpui::ScrollHandle::new(),
            pending_play: None,
            path_cache: load_path_cache(),
            status_line: String::new(),
            monitors: crate::win::list_monitors()
                .into_iter()
                .map(|m| {
                    (
                        format!(
                            "显示器 {}({}×{}{})",
                            m.model,
                            m.w,
                            m.h,
                            if m.primary { ",主屏" } else { "" }
                        ),
                        m.device,
                    )
                })
                .collect(),
            audio_devs: crate::audio::output_devices(),
            skins: vec![("内置 Argon".to_string(), None)],
            hero: HashMap::new(),
            hero_order: std::collections::VecDeque::new(),
            thumbs: Mutex::new(HashMap::new()),
            thumb_order: Mutex::new(std::collections::VecDeque::new()),
            lib_loading: Arc::new(Mutex::new(None)),
            set_idx: HashMap::new(),
            loading: false,
            preview: None,
            last_ui: (u64::MAX, false, u64::MAX),
            open_menu: None,
            lib_scroll: gpui::UniformListScrollHandle::default(),
            lib_list: gpui::ListState::new(0, gpui::ListAlignment::Top, px(300.0)),
            queue_scroll: gpui::UniformListScrollHandle::default(),
        };
        sync_close_mode(&app.settings);
        app.spawn_library_load();
        app.load_queue();
        // 队列为空时默认落在曲库,避免一进来只看到空列表
        if app.queue.is_empty() {
            app.view = View::Library;
        } else if app.settings.play_on_start {
            let i = app.settings.playlist_pos.min(app.queue.len() - 1);
            app.play_index(i);
        }
        app.persist();

        app
    }

    fn load_queue(&mut self) {
        self.queue = self
            .settings
            .playlist
            .iter()
            .map(|t| QueueTrack {
                set_id: t.set_id.clone(),
                sha2: t.sha2.clone(),
                title: if t.title.is_empty() { t.set_id.clone() } else { t.title.clone() },
                artist: t.artist.clone(),
                length_ms: t.length_ms,
                mods: t.mods,
            })
            .collect();
        self.pos = self.settings.playlist_pos.min(self.queue.len().saturating_sub(1));
    }

    /// 后台解析曲库(不阻塞 UI):结果在 poll 里应用。
    fn spawn_library_load(&mut self) {
        self.loading = true;
        self.status_line = "曲库加载中…".to_string();
        let source = self.settings.source.clone();
        let lazer_dir = self.settings.lazer_dir.clone();
        let stable_dir = self.settings.stable_dir.clone();
        let unicode = self.settings.display_unicode;
        let slot = self.lib_loading.clone();
        std::thread::spawn(move || {
            let _ = crate::lazer::set_custom_data_dir(lazer_dir.as_deref().map(Path::new));
            crate::stable::set_custom_stable_dir(stable_dir.as_deref().map(Path::new));
            let lib = if source == "stable" {
                crate::stable::stable_root().map(|root| crate::stable::library(&root))
            } else {
                crate::lazer::realm_path().map(|p| crate::lazer::parse(&p))
            };
            let payload = match lib {
                Some(Ok(lib)) => {
                    let lib = Arc::new(lib);
                    let root = crate::lazer::data_root();
                    let mut all = Vec::with_capacity(lib.sets.len());
                    let mut cover_paths = HashMap::new();
                    for set in lib.sets.iter() {
                        let star =
                            set.beatmaps.iter().map(|b| b.star_rating).fold(0.0, f64::max);
                        let star_lo = set
                            .beatmaps
                            .iter()
                            .map(|b| b.star_rating)
                            .filter(|v| *v > 0.0)
                            .fold(f64::INFINITY, f64::min);
                        let sha2 = set
                            .beatmaps
                            .iter()
                            .max_by(|a, b| a.star_rating.total_cmp(&b.star_rating))
                            .map(|b| b.sha2.clone())
                            .unwrap_or_default();
                        if let Some(p) = cover_path(set, root.as_deref()) {
                            cover_paths.insert(set.id.clone(), p);
                        }
                        all.push(Row {
                            set_id: set.id.clone(),
                            sha2,
                            title: pick_text(&set.title, &set.title_unicode, unicode),
                            artist: pick_text(&set.artist, &set.artist_unicode, unicode),
                            star,
                            star_lo: if star_lo.is_finite() { star_lo } else { star },
                            diffs: set.beatmaps.len(),
                            diffs_in_range: set.beatmaps.len(),
                            has_sb: set.sb_video,
                            added: set.date_added_ms,
                            creator: set.creator.clone(),
                            size: set.files.iter().map(|f| f.size as i64).sum(),
                            status: set.status,
                        });
                    }
                    let collections =
                        lib.collections.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
                    let mut skins = vec![("内置 Argon".to_string(), None)];
                    for sk in &lib.skins {
                        let label = if sk.creator.is_empty() {
                            sk.name.clone()
                        } else {
                            format!("{} ({})", sk.name, sk.creator)
                        };
                        skins.push((label, Some(format!("realm:skin:{}", sk.id))));
                    }
                    if let Some(root) = crate::stable::stable_root() {
                        if let Ok(rd) = std::fs::read_dir(root.join("Skins")) {
                            for e in rd.flatten() {
                                if e.path().is_dir() {
                                    let name = e.file_name().to_string_lossy().into_owned();
                                    skins.push((
                                        name,
                                        Some(e.path().to_string_lossy().into_owned()),
                                    ));
                                }
                            }
                        }
                    }
                    LibPayload {
                        thumbs: HashMap::new(),
                        status_line: format!("{} 个谱面集", all.len()),
                        lib,
                        all_rows: all,
                        collections,
                        cover_paths,
                        skins,
                    }
                }
                Some(Err(e)) => LibPayload {
                    thumbs: HashMap::new(),
                    lib: Arc::new(crate::lazer::LazerLibrary::default()),
                    all_rows: Vec::new(),
                    collections: Vec::new(),
                    cover_paths: HashMap::new(),
                    skins: vec![("内置 Argon".to_string(), None)],
                    status_line: format!("曲库解析失败:{e}"),
                },
                None => LibPayload {
                    thumbs: HashMap::new(),
                    lib: Arc::new(crate::lazer::LazerLibrary::default()),
                    all_rows: Vec::new(),
                    collections: Vec::new(),
                    cover_paths: HashMap::new(),
                    skins: vec![("内置 Argon".to_string(), None)],
                    status_line: "未找到 osu! 数据目录".to_string(),
                },
            };
            *slot.lock().unwrap() = Some(payload);
        });
    }

    /// 缩略图按需生成(64px),结果缓存;渲染时只处理可见行。
    fn thumb_of(&self, set_id: &str) -> Option<Arc<gpui::Image>> {
        if let Some(v) = self.thumbs.lock().unwrap().get(set_id) {
            return v.clone();
        }
        // 只解码当前可见行;超出上限按插入顺序淘汰最旧的
        let img = self.cover_paths.get(set_id).and_then(|p| thumb_at(p, 64));
        if img.is_some() {
            let mut cache = self.thumbs.lock().unwrap();
            let mut order = self.thumb_order.lock().unwrap();
            if cache.len() >= 120 {
                if let Some(old) = order.pop_front() {
                    cache.remove(&old);
                }
            }
            cache.insert(set_id.to_string(), img.clone());
            order.push_back(set_id.to_string());
        }
        img
    }

    fn apply_filter(&mut self) {
        let q = self.search.to_lowercase();
        let coll_md5: Option<HashSet<String>> = self.settings.collection.as_ref().and_then(|name| {
            let lib = self.lib.as_ref()?;
            lib.collections.iter().find(|c| &c.name == name).map(|c| c.md5s.iter().cloned().collect())
        });
        let mut rows: Vec<Row> = self
            .all_rows
            .iter()
            .filter(|r| {
                if !q.is_empty()
                    && !r.title.to_lowercase().contains(&q)
                    && !r.artist.to_lowercase().contains(&q)
                {
                    return false;
                }
                if r.star < self.star_min {
                    return false;
                }
                if self.star_max > 0.0 && r.star > self.star_max {
                    return false;
                }
                if self.sb_only && !r.has_sb {
                    return false;
                }
                if let Some(md5s) = &coll_md5 {
                    let Some(lib) = &self.lib else { return false };
                    let Some(set) = lib.sets.iter().find(|s| s.id == r.set_id) else {
                        return false;
                    };
                    if !set.beatmaps.iter().any(|b| md5s.contains(&b.md5)) {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect();
        // 星级过滤后每个谱面集剩余多少难度
        if self.star_min > 0.0 || self.star_max > 0.0 {
            if let Some(lib) = &self.lib {
                for r in rows.iter_mut() {
                    if let Some(set) = self.set_idx.get(&r.set_id).and_then(|i| lib.sets.get(*i)) {
                        r.diffs_in_range = set
                            .beatmaps
                            .iter()
                            .filter(|b| {
                                b.star_rating >= self.star_min
                                    && (self.star_max <= 0.0 || b.star_rating <= self.star_max)
                            })
                            .count();
                    }
                }
            }
        }
        match self.sort {
            SortKey::Added => rows.sort_by_key(|r| r.added),
            SortKey::Title => rows.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase())),
            SortKey::Star => rows.sort_by(|a, b| a.star.total_cmp(&b.star)),
            SortKey::Artist => rows.sort_by(|a, b| a.artist.to_lowercase().cmp(&b.artist.to_lowercase())),
            SortKey::Creator => rows.sort_by(|a, b| a.creator.to_lowercase().cmp(&b.creator.to_lowercase())),
            SortKey::Size => rows.sort_by_key(|r| r.size),
            SortKey::CollectionOrder => {
                let order: HashMap<String, usize> = self
                    .settings
                    .collection
                    .as_ref()
                    .and_then(|name| self.lib.as_ref()?.collections.iter().find(|c| &c.name == name))
                    .map(|c| {
                        let idx: HashMap<&str, usize> =
                            c.md5s.iter().enumerate().map(|(i, m)| (m.as_str(), i)).collect();
                        self.all_rows
                            .iter()
                            .filter_map(|r| {
                                let set = self.lib.as_ref()?.sets.iter().find(|s| s.id == r.set_id)?;
                                let best = set
                                    .beatmaps
                                    .iter()
                                    .filter_map(|b| idx.get(b.md5.as_str()).copied())
                                    .min()?;
                                Some((r.set_id.clone(), best))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                rows.sort_by_key(|r| order.get(&r.set_id).copied().unwrap_or(usize::MAX));
            }
        }
        if self.desc {
            rows.reverse();
        }
        self.rows = rows;
    }

    // ---- 播放 ----

    fn spawn_if_needed(&mut self) {
        let dead = self.child.as_mut().map(|c| !c.alive()).unwrap_or(true);
        if dead {
            self.child = spawn_wallpaper(self.status.clone());
        }
    }

    fn resolve_path(&self, set_id: &str, sha2: &str) -> Option<(String, Option<Vec<crate::ipc::VFile>>)> {
        let lib = self.lib.as_ref()?;
        let set = lib.sets.iter().find(|s| s.id == set_id)?;
        match &set.root {
            Some(root) => Some((format!("{root}/{sha2}"), None)),
            None => {
                let root = crate::lazer::data_root()?;
                let osu = crate::lazer::beatmap_blob(&root, sha2);
                let files = set
                    .files
                    .iter()
                    .map(|f| crate::ipc::VFile {
                        name: f.filename.clone(),
                        path: root
                            .join("files")
                            .join(crate::lazer::blob_relative_path(&f.hash))
                            .to_string_lossy()
                            .into_owned(),
                    })
                    .collect();
                Some((osu.to_string_lossy().into_owned(), Some(files)))
            }
        }
    }

    fn play(&mut self, set_id: &str, sha2: Option<&str>, mods: u32) {
        // 曲库还没解析完:若该难度已有路径缓存则直接开播,否则挂起等曲库
        let cached_explicit = sha2
            .filter(|s| !s.is_empty())
            .map(|s| format!("{set_id}/{s}"))
            .and_then(|k| self.path_cache.get(&k).cloned())
            .filter(|e| Path::new(&e.path).is_file());
        if self.lib.is_none() && cached_explicit.is_none() {
            self.pending_play = Some((set_id.to_string(), sha2.map(|s| s.to_string()), mods));
            self.toast("曲库加载中…完成后自动播放");
            return;
        }
        // 难度策略:显式难度优先,否则按设置(最难 / 最容易 / 指定星级)
        let chosen: String = match sha2 {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                let policy = match self.settings.target_star {
                    None => crate::playlist::PickPolicy::Hardest,
                    Some(t) if t < 0.0 => crate::playlist::PickPolicy::Easiest,
                    Some(t) => crate::playlist::PickPolicy::Custom(t),
                };
                self.lib
                    .as_ref()
                    .and_then(|l| self.set_idx.get(set_id).and_then(|i| l.sets.get(*i)))
                    .filter(|set| !set.beatmaps.is_empty())
                    .map(|set| crate::playlist::pick_diff(set, policy).sha2.clone())
                    .unwrap_or_default()
            }
        };
        let key = format!("{set_id}/{chosen}");
        let cached = self
            .path_cache
            .get(&key)
            .filter(|e| Path::new(&e.path).is_file())
            .cloned();
        let (path, manifest) = match cached {
            Some(e) => (e.path, e.manifest),
            None => {
                let Some((path, manifest)) = self.resolve_path(set_id, &chosen) else {
                    self.toast("找不到谱面文件(曲库可能已更新,刷新曲库试试)");
                    return;
                };
                if !Path::new(&path).is_file() {
                    self.toast("谱面文件不存在,刷新曲库后重试");
                    return;
                }
                self.path_cache.insert(
                    key.clone(),
                    PathEntry { path: path.clone(), manifest: manifest.clone() },
                );
                save_path_cache(&self.path_cache);
                (path, manifest)
            }
        };
        // 谱面集个性设置覆盖全局
        let eff = self.settings.effective_for(set_id);
        self.spawn_if_needed();
        let Some(child) = &self.child else {
            self.toast("壁纸进程启动失败");
            return;
        };
        let skin = self.resolve_skin();
        let monitor = self.monitor_bounds();
        let cmd = serde_json::json!({
            "cmd": "load", "path": path,
            "speed": self.settings.speed,
            "manifest": manifest,
            "storyboard": eff.storyboard,
            "video": eff.storyboard,
            "mods": mods,
            "skin": skin,
            "force_colours": self.settings.force_skin_colours,
            "hidden": self.settings.hidden,
            "fail": self.settings.fail,
            "loop_playback": self.settings.play_mode == crate::playlist::PlayMode::SingleLoop,
            "beatmap_hitsounds": eff.beatmap_hitsounds,
            "upscale": self.settings.upscale,
            "upscale_quality": self.settings.upscale_quality,
            "monitor": monitor,
        });
        let sent = child.send_raw(&cmd.to_string()).is_ok();
        if sent {
            self.playing = true;
            self.current_title(set_id);
        }
        if sent {
            if let Some(child) = &self.child {
                for c in [
                    json!({"cmd": "set_bg_opacity", "v": eff.bg_opacity}),
                    json!({"cmd": "set_offset", "ms": eff.audio_offset_ms}),
                    json!({"cmd": "set_hits_volume", "v": eff.hits_volume}),
                    json!({"cmd": "set_gameplay_hidden", "on": eff.gameplay_hidden}),
                ] {
                    let _ = child.send_raw(&c.to_string());
                }
            }
        } else {
            self.toast("下发 Load 失败");
        }
    }

    /// 皮肤身份值 → 可下发的目录(realm 皮肤按需挂载到缓存目录)。
    fn resolve_skin(&self) -> Option<String> {
        let stored = self.settings.skin.as_deref()?;
        if let Some(id) = stored.strip_prefix("realm:skin:") {
            let root = crate::lazer::data_root()?;
            let lib = self.lib.as_ref()?;
            let skin = lib.skins.iter().find(|s| s.id == id)?;
            let cache = data_dir().join("skin-cache");
            return crate::lazer::mount_skin(&root, skin, &cache)
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
        }
        Path::new(stored).is_dir().then(|| stored.to_string())
    }

    /// 目标显示器 bounds((x, y, w, h));None = 主屏。
    fn monitor_bounds(&self) -> Option<(i32, i32, u32, u32)> {
        let dev = self.settings.monitor.as_deref()?;
        crate::win::list_monitors()
            .into_iter()
            .find(|m| m.device == dev)
            .map(|m| (m.x, m.y, m.w as u32, m.h as u32))
    }

    fn current_title(&mut self, set_id: &str) {
        if let Some(r) = self.all_rows.iter().find(|r| r.set_id == set_id) {
            self.settings.lazer_set = Some(set_id.to_string());
            self.persist();
        }
    }

    fn play_index(&mut self, i: usize) {
        let Some(t) = self.queue.get(i).cloned() else { return };
        // 没锁定难度时按策略选一个并写回,保证"实际播什么就是什么"
        let sha = t.sha2.clone().or_else(|| self.policy_pick(&t.set_id));
        if let (Some(sha), Some(entry)) = (sha.clone(), self.queue.get_mut(i)) {
            entry.sha2 = Some(sha);
            self.save_queue();
        }
        self.pos = i;
        self.settings.playlist_pos = i;
        self.persist();
        self.play(&t.set_id, sha.as_deref(), t.mods);
    }

    /// 按难度策略从谱面集里选一个难度 sha2。
    fn policy_pick(&self, set_id: &str) -> Option<String> {
        let policy = match self.settings.target_star {
            None => crate::playlist::PickPolicy::Hardest,
            Some(t) if t < 0.0 => crate::playlist::PickPolicy::Easiest,
            Some(t) => crate::playlist::PickPolicy::Custom(t),
        };
        let set = self
            .lib
            .as_ref()
            .and_then(|l| self.set_idx.get(set_id).and_then(|i| l.sets.get(*i)))?;
        if set.beatmaps.is_empty() {
            return None;
        }
        Some(crate::playlist::pick_diff(set, policy).sha2.clone())
    }

    fn toggle_pause(&mut self) {
        // 还没加载任何曲目:直接播放队列当前项(播放列表为空时提示)
        let loaded = {
            let st = self.status.lock().unwrap();
            st.duration > 0.0
        };
        if !loaded {
            if self.queue.is_empty() {
                self.toast("播放列表为空,先去曲库添加");
                return;
            }
            let i = self.pos.min(self.queue.len() - 1);
            self.play_index(i);
            return;
        }
        if self.child.is_none() {
            self.spawn_if_needed();
        }
        let Some(child) = &self.child else { return };
        let cmd = if self.playing { "pause" } else { "resume" };
        if child.send_raw(&format!("{{\"cmd\":\"{cmd}\"}}")).is_ok() {
            self.playing = !self.playing;
        }
    }

    fn step(&mut self, delta: isize) {
        if self.queue.is_empty() {
            return;
        }
        let n = self.queue.len() as isize;
        let next = (self.pos as isize + delta).rem_euclid(n) as usize;
        self.play_index(next);
    }

    fn seek_rel(&mut self, secs: f64) {
        let t = {
            let st = self.status.lock().unwrap();
            (st.t + secs).max(0.0)
        };
        self.send(json!({"cmd": "seek", "ms": (t * 1000.0) as f32}));
    }

    // ---- 队列操作 ----

    fn persist(&mut self) {
        self.settings_rev = self.settings_rev.wrapping_add(1);
        crate::settings::save_to(&data_dir(), &self.settings);
        apply_palette(self.settings.theme_mode != "light", &self.settings.accent);
        sync_close_mode(&self.settings);
    }

    fn save_queue(&mut self) {
        self.settings.playlist = self
            .queue
            .iter()
            .map(|t| crate::settings::SavedTrack {
                set_id: t.set_id.clone(),
                sha2: t.sha2.clone(),
                mods: t.mods,
                title: t.title.clone(),
                artist: t.artist.clone(),
                length_ms: t.length_ms,
            })
            .collect();
        self.settings.playlist_pos = self.pos.min(self.queue.len().saturating_sub(1));
        self.persist();
    }

    /// 谱面集时长(指定难度优先,否则最长难度)。
    fn set_length_ms(&self, set_id: &str, sha2: Option<&str>) -> f64 {
        let Some(set) = self
            .lib
            .as_ref()
            .and_then(|l| self.set_idx.get(set_id).and_then(|i| l.sets.get(*i)))
        else {
            return 0.0;
        };
        let bm = sha2
            .and_then(|s| set.beatmaps.iter().find(|b| b.sha2 == s))
            .or_else(|| set.beatmaps.iter().max_by(|a, b| a.length_ms.total_cmp(&b.length_ms)));
        bm.map(|b| b.length_ms).unwrap_or(0.0)
    }

    fn add_to_queue_with(&mut self, set_id: &str, sha2: Option<String>, mods: u32) {
        if self.queue.iter().any(|t| t.set_id == set_id) {
            self.toast("已在播放列表中");
            return;
        }
        let Some(r) = self.all_rows.iter().find(|r| r.set_id == set_id) else { return };
        let len = self.set_length_ms(set_id, sha2.as_deref());
        self.queue.push(QueueTrack {
            set_id: r.set_id.clone(),
            sha2,
            title: r.title.clone(),
            artist: r.artist.clone(),
            length_ms: len,
            mods,
        });
        self.save_queue();
        self.toast("已加入播放列表");
    }

    fn add_to_queue(&mut self, set_id: &str) {
        if self.queue.iter().any(|t| t.set_id == set_id) {
            self.toast("已在播放列表中");
            return;
        }
        let Some(r) = self.all_rows.iter().find(|r| r.set_id == set_id) else { return };
        // 快速加入策略:最难 / 最容易 / 最接近指定星级
        let policy = match self.settings.quick_policy.as_str() {
            "easiest" => crate::playlist::PickPolicy::Easiest,
            "custom" => {
                crate::playlist::PickPolicy::Custom(self.settings.quick_star.unwrap_or(5.0) as f64)
            }
            _ => crate::playlist::PickPolicy::Hardest,
        };
        let sha2 = self
            .lib
            .as_ref()
            .and_then(|l| self.set_idx.get(set_id).and_then(|i| l.sets.get(*i)))
            .filter(|set| !set.beatmaps.is_empty())
            .map(|set| Some(crate::playlist::pick_diff(set, policy).sha2.clone()))
            .unwrap_or(None);
        let mods = self.settings.quick_mods;
        let len = self.set_length_ms(set_id, sha2.as_deref());
        self.queue.push(QueueTrack {
            set_id: r.set_id.clone(),
            sha2,
            title: r.title.clone(),
            artist: r.artist.clone(),
            length_ms: len,
            mods,
        });
        self.save_queue();
        self.toast("已加入播放列表");
    }

    /// 把当前筛选结果全部加入播放列表(跳过已存在)。
    fn add_filtered(&mut self) {
        let ids: Vec<String> = self.rows.iter().map(|r| r.set_id.clone()).collect();
        let mut n = 0usize;
        for id in ids {
            if self.queue.iter().any(|t| t.set_id == id) {
                continue;
            }
            let Some(r) = self.all_rows.iter().find(|r| r.set_id == id) else { continue };
            let len = self.set_length_ms(&id, None);
            self.queue.push(QueueTrack {
                set_id: r.set_id.clone(),
                sha2: None,
                title: r.title.clone(),
                artist: r.artist.clone(),
                length_ms: len,
                mods: 0,
            });
            n += 1;
        }
        self.save_queue();
        self.toast(format!("已加入 {n} 个谱面集"));
    }

    fn unload(&mut self) {
        self.send(json!({"cmd": "unload"}));
        self.playing = false;
    }

    fn remove_at(&mut self, i: usize) {
        if i >= self.queue.len() {
            return;
        }
        self.queue.remove(i);
        self.selected.clear();
        if self.pos > 0 && i <= self.pos {
            self.pos -= 1;
        }
        self.save_queue();
    }

    /// 把第 from 条拖到第 to 条位置。
    fn move_to(&mut self, from: usize, to: usize) {
        if from == to || from >= self.queue.len() || to >= self.queue.len() {
            return;
        }
        let t = self.queue.remove(from);
        self.queue.insert(to, t);
        if self.pos == from {
            self.pos = to;
        } else if from < self.pos && to >= self.pos {
            self.pos -= 1;
        } else if from > self.pos && to <= self.pos {
            self.pos += 1;
        }
        self.save_queue();
    }

    fn move_at(&mut self, i: usize, up: bool) {
        if up && i > 0 {
            self.queue.swap(i, i - 1);
        } else if !up && i + 1 < self.queue.len() {
            self.queue.swap(i, i + 1);
        }
        self.save_queue();
    }

    /// 队列条目 mods 循环(无 → HD → HR → DT → NC)。
    fn cycle_queue_mods(&mut self, i: usize) {
        const MODS: [u32; 5] = [0, 8, 16, 64, 512];
        let Some(t) = self.queue.get_mut(i) else { return };
        let cur = MODS.iter().position(|v| *v == t.mods).unwrap_or(0);
        t.mods = MODS[(cur + 1) % MODS.len()];
        self.save_queue();
        if i == self.pos {
            self.play_index(i);
        }
    }

    fn set_queue_diff(&mut self, i: usize, hardest: bool) {
        let Some(t) = self.queue.get_mut(i) else { return };
        let sha2 = {
            let Some(r) = self.all_rows.iter().find(|r| r.set_id == t.set_id) else { return };
            let Some(lib) = &self.lib else { return };
            let Some(set) = lib.sets.iter().find(|s| s.id == t.set_id) else { return };
            let pick = if hardest {
                set.beatmaps.iter().max_by(|a, b| a.star_rating.total_cmp(&b.star_rating))
            } else {
                set.beatmaps.iter().min_by(|a, b| a.star_rating.total_cmp(&b.star_rating))
            };
            let _ = r;
            pick.map(|b| b.sha2.clone())
        };
        t.sha2 = sha2;
        self.save_queue();
        if i == self.pos {
            self.play_index(i);
        }
    }

    // ---- 设置 ----

    fn send(&mut self, json: serde_json::Value) {
        if let Some(c) = &self.child {
            let _ = c.send_raw(&json.to_string());
        }
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn toggle(&mut self, which: &'static str) {
        match which {
            "hud" => {
                self.settings.hud = !self.settings.hud;
                self.send(json!({"cmd": "set_hud", "on": self.settings.hud}));
            }
            "pp" => {
                self.settings.pp = !self.settings.pp;
                self.send(json!({"cmd": "set_pp", "on": self.settings.pp}));
            }
            "storyboard" => {
                self.settings.storyboard = !self.settings.storyboard;
                self.send(json!({"cmd": "set_video", "on": self.settings.storyboard}));
            }
            "beatmap_hitsounds" => {
                self.settings.beatmap_hitsounds = !self.settings.beatmap_hitsounds;
                self.send(json!({"cmd": "set_beatmap_hitsounds", "on": self.settings.beatmap_hitsounds}));
            }
            "hitsound" => {
                self.settings.hitsound = !self.settings.hitsound;
                let v = if self.settings.hitsound { self.settings.hits_volume } else { 0.0 };
                self.send(json!({"cmd": "set_hits_volume", "v": v}));
            }
            "cursor" => {
                self.settings.cursor = !self.settings.cursor;
                self.send(json!({"cmd": "set_cursor", "on": self.settings.cursor}));
            }
            "cursor_trail" => {
                self.settings.cursor_trail = !self.settings.cursor_trail;
                self.send(json!({"cmd": "set_cursor_trail", "on": self.settings.cursor_trail}));
            }
            "cursor_ripples" => {
                self.settings.cursor_ripples = !self.settings.cursor_ripples;
                self.send(json!({"cmd": "set_cursor_ripples", "on": self.settings.cursor_ripples}));
            }
            "snaking_in" => {
                self.settings.snaking_in = !self.settings.snaking_in;
                self.send(json!({"cmd": "set_snaking_in", "on": self.settings.snaking_in}));
            }
            "snaking_out" => {
                self.settings.snaking_out = !self.settings.snaking_out;
                self.send(json!({"cmd": "set_snaking_out", "on": self.settings.snaking_out}));
            }
            "gameplay_hidden" => {
                self.settings.gameplay_hidden = !self.settings.gameplay_hidden;
                self.send(json!({"cmd": "set_gameplay_hidden", "on": self.settings.gameplay_hidden}));
            }
            "break_lighten" => {
                self.settings.break_lighten = !self.settings.break_lighten;
                self.send(json!({"cmd": "set_break_lighten", "on": self.settings.break_lighten}));
            }
            "reduce_anim" => {
                self.settings.reduce_anim = !self.settings.reduce_anim;
                self.send(json!({"cmd": "set_hit_animations", "on": !self.settings.reduce_anim}));
            }
            "force_skin_colours" => self.settings.force_skin_colours = !self.settings.force_skin_colours,
            "display_unicode" => {
                self.settings.display_unicode = !self.settings.display_unicode;
                self.spawn_library_load();
            }
            "dblclick_add" => self.settings.dblclick_add = !self.settings.dblclick_add,
            "quick_add" => self.settings.quick_add = !self.settings.quick_add,
            "loop_playback" => {
                self.settings.loop_playback = !self.settings.loop_playback;
                self.send(json!({"cmd": "set_loop", "on": self.settings.loop_playback}));
            }
            "play_on_start" => {
                self.settings.play_on_start = !self.settings.play_on_start;
            }
            "autostart" => {
                let on = !self.settings.autostart;
                match crate::settings::set_autostart(on) {
                    Ok(()) => self.settings.autostart = on,
                    Err(e) => self.toast(format!("自启设置失败:{e}")),
                }
            }
            "log" => {
                self.settings.log_enabled = !self.settings.log_enabled;
                crate::logging::set_enabled(self.settings.log_enabled);
                self.send(json!({"cmd": "set_log", "on": self.settings.log_enabled}));
            }
            "hidden" => {
                self.settings.hidden = !self.settings.hidden;
                self.send(json!({"cmd": "set_hidden", "on": self.settings.hidden}));
            }
            "fail" => {
                self.settings.fail = !self.settings.fail;
                self.send(json!({"cmd": "set_fail", "on": self.settings.fail}));
            }
            "fade_audio" => {
                self.settings.fade_audio = !self.settings.fade_audio;
                self.send(json!({"cmd": "set_fade", "on": self.settings.fade_audio}));
            }
            "debug_window" => {
                self.settings.debug_window = !self.settings.debug_window;
                self.send(json!({"cmd": "set_debug_window", "on": self.settings.debug_window}));
            }
            _ => {}
        }
        self.persist();
    }

    fn cycle(&mut self, which: &'static str) {
        match which {
            "fps" => {
                const STEPS: [u32; 6] = [0, 30, 60, 120, 240, 360];
                let i = STEPS.iter().position(|v| *v == self.settings.fps).unwrap_or(0);
                self.settings.fps = STEPS[(i + 1) % STEPS.len()];
                self.send(json!({"cmd": "set_fps", "fps": self.settings.fps}));
            }
            "diff" => {
                self.settings.target_star = match self.settings.target_star {
                    None => Some(-1.0),
                    Some(_) => None,
                };
            }
            "render_mode" => {
                let order = ["autopause", "always", "off"];
                let i = order.iter().position(|v| *v == self.settings.render_mode).unwrap_or(0);
                self.settings.render_mode = order[(i + 1) % order.len()].to_string();
            }
            "play_mode" => {
                self.settings.play_mode = match self.settings.play_mode {
                    crate::playlist::PlayMode::SingleLoop => crate::playlist::PlayMode::ListOrder,
                    crate::playlist::PlayMode::ListOrder => crate::playlist::PlayMode::Random,
                    crate::playlist::PlayMode::Random => crate::playlist::PlayMode::SingleLoop,
                };
            }
            "source" => {
                self.settings.source = if self.settings.source == "stable" { "lazer".into() } else { "stable".into() };
                self.spawn_library_load();
            }
            "collection" => {
                let mut names: Vec<Option<String>> = vec![None];
                names.extend(self.collections.iter().cloned().map(Some));
                let cur = names.iter().position(|n| n == &self.settings.collection).unwrap_or(0);
                self.settings.collection = names[(cur + 1) % names.len()].clone();
                self.apply_filter();
            }
            "sort" => {
                self.sort = match self.sort {
                    SortKey::Added => SortKey::Title,
                    SortKey::Title => SortKey::Star,
                    SortKey::Star => SortKey::Artist,
                    SortKey::Artist => SortKey::Creator,
                    SortKey::Creator => SortKey::Size,
                    SortKey::Size => SortKey::CollectionOrder,
                    SortKey::CollectionOrder => SortKey::Added,
                };
                self.apply_filter();
            }
            "upscale" => {
                let order = ["off", "fsr", "anime4k"];
                let i = order.iter().position(|v| *v == self.settings.upscale).unwrap_or(0);
                self.settings.upscale = order[(i + 1) % order.len()].to_string();
                self.send(json!({"cmd": "set_upscale", "mode": self.settings.upscale, "quality": self.settings.upscale_quality}));
            }
            "upscale_quality" => {
                let order = ["s", "m", "l", "vl", "ul"];
                let i = order.iter().position(|v| *v == self.settings.upscale_quality).unwrap_or(1);
                self.settings.upscale_quality = order[(i + 1) % order.len()].to_string();
                self.send(json!({"cmd": "set_upscale", "mode": self.settings.upscale, "quality": self.settings.upscale_quality}));
            }
            "theme" => {
                self.settings.theme_mode =
                    if self.settings.theme_mode == "light" { "dark".into() } else { "light".into() };
                apply_palette(self.settings.theme_mode != "light", &self.settings.accent);
            }
            "accent" => {
                let order = ["blue", "purple", "green", "pink", "orange"];
                let i = order.iter().position(|v| *v == self.settings.accent).unwrap_or(0);
                self.settings.accent = order[(i + 1) % order.len()].to_string();
                apply_palette(self.settings.theme_mode != "light", &self.settings.accent);
            }
            "close_action" => {
                self.settings.close_action = match self.settings.close_action.as_deref() {
                    None => Some("tray".into()),
                    Some("tray") => Some("exit".into()),
                    _ => None,
                };
            }
            "quick_mods" => {
                const MODS: [u32; 7] = [0, 8, 16, 2, 64, 256, 512];
                let i = MODS.iter().position(|v| *v == self.settings.quick_mods).unwrap_or(0);
                self.settings.quick_mods = MODS[(i + 1) % MODS.len()];
            }
            "skin" => {
                let mut opts: Vec<Option<String>> = vec![None];
                opts.extend(self.skins.iter().map(|(_, v)| v.clone()));
                let cur = opts.iter().position(|v| v == &self.settings.skin).unwrap_or(0);
                self.settings.skin = opts[(cur + 1) % opts.len()].clone();
                self.send(json!({"cmd": "set_skin", "path": self.resolve_skin()}));
            }
            "monitor" => {
                let mut opts: Vec<Option<String>> = vec![None];
                opts.extend(self.monitors.iter().map(|(_, d)| Some(d.clone())));
                let cur = opts.iter().position(|v| v == &self.settings.monitor).unwrap_or(0);
                self.settings.monitor = opts[(cur + 1) % opts.len()].clone();
                if !self.queue.is_empty() {
                    self.play_index(self.pos.min(self.queue.len() - 1));
                }
            }
            "audio_device" => {
                let mut opts: Vec<Option<String>> = vec![None];
                opts.extend(self.audio_devs.iter().map(|(id, _)| Some(id.clone())));
                let cur = opts.iter().position(|v| v == &self.settings.audio_device).unwrap_or(0);
                self.settings.audio_device = opts[(cur + 1) % opts.len()].clone();
                self.send(json!({"cmd": "set_audio_device", "id": self.settings.audio_device}));
            }
            _ => {}
        }
        self.persist();
    }

    fn bump(&mut self, which: &'static str, dir: f32) {
        match which {
            "volume" => {
                self.settings.master_volume = (self.settings.master_volume + 0.05 * dir).clamp(0.0, 1.0);
                self.send(json!({"cmd": "set_master", "v": self.settings.master_volume}));
            }
            "bgm" => {
                self.settings.volume = (self.settings.volume + 0.05 * dir).clamp(0.0, 1.0);
                self.send(json!({"cmd": "set_volume", "v": self.settings.volume}));
            }
            "hits" => {
                self.settings.hits_volume = (self.settings.hits_volume + 0.05 * dir).clamp(0.0, 1.0);
                let v = if self.settings.hitsound { self.settings.hits_volume } else { 0.0 };
                self.send(json!({"cmd": "set_hits_volume", "v": v}));
            }
            "speed" => {
                self.settings.speed = (self.settings.speed + 0.05 * dir).clamp(0.25, 2.0);
                self.send(json!({"cmd": "set_speed", "x": self.settings.speed}));
            }
            "target_star" => {
                let cur = self.settings.target_star.unwrap_or(5.0);
                self.settings.target_star = Some((cur + 0.1 * f64::from(dir)).clamp(0.5, 12.0));
            }
            "offset" => {
                self.settings.audio_offset_ms = (self.settings.audio_offset_ms + 5.0 * dir).clamp(-200.0, 200.0);
                self.send(json!({"cmd": "set_offset", "ms": self.settings.audio_offset_ms}));
            }
            "bg_opacity" => {
                self.settings.bg_opacity = (self.settings.bg_opacity + 0.05 * dir).clamp(0.0, 1.0);
                self.send(json!({"cmd": "set_bg_opacity", "v": self.settings.bg_opacity}));
            }
            "cursor_size" => {
                self.settings.cursor_size = (self.settings.cursor_size + 0.05 * dir).clamp(0.1, 2.0);
                self.send(json!({"cmd": "set_cursor_size", "x": self.settings.cursor_size}));
            }
            "star_min" => {
                self.settings.star_min = (self.settings.star_min + f64::from(dir)).clamp(0.0, 10.0);
                self.star_min = self.settings.star_min;
                self.apply_filter();
            }
            _ => {}
        }
        self.persist();
    }

    /// 数值的显示单位与缩放(显示值 = 实际值 / scale)。
    fn num_unit(kind: &str) -> (&'static str, f64) {
        match kind {
            "volume" | "bgm" | "hits" | "bg_opacity" => ("%", 0.01),
            "offset" => (" ms", 1.0),
            "speed" => ("x", 1.0),
            "target_star" => ("★", 1.0),
            "cursor_size" => ("x", 1.0),
            "star_min" => ("★", 1.0),
            _ => ("", 1.0),
        }
    }

    fn num_field(kind: &str) -> Option<Field> {
        Some(match kind {
            "volume" => Field::NumVolume,
            "bgm" => Field::NumBgm,
            "hits" => Field::NumHits,
            "speed" => Field::NumSpeed,
            "target_star" => Field::NumStar,
            "offset" => Field::NumOffset,
            "bg_opacity" => Field::NumBg,
            "cursor_size" => Field::NumCursor,
            "star_min" => Field::NumStarMin,
            _ => return None,
        })
    }

    fn field_kind(field: Field) -> Option<&'static str> {
        Some(match field {
            Field::NumVolume => "volume",
            Field::NumBgm => "bgm",
            Field::NumHits => "hits",
            Field::NumSpeed => "speed",
            Field::NumStar => "target_star",
            Field::NumOffset => "offset",
            Field::NumBg => "bg_opacity",
            Field::NumCursor => "cursor_size",
            Field::NumStarMin => "star_min",
            _ => return None,
        })
    }

    fn num_value(&self, kind: &str) -> f64 {
        match kind {
            "volume" => self.settings.master_volume as f64,
            "bgm" => self.settings.volume as f64,
            "hits" => self.settings.hits_volume as f64,
            "speed" => self.settings.speed as f64,
            "target_star" => self.settings.target_star.unwrap_or(5.0),
            "offset" => self.settings.audio_offset_ms as f64,
            "bg_opacity" => self.settings.bg_opacity as f64,
            "cursor_size" => self.settings.cursor_size as f64,
            "star_min" => self.settings.star_min,
            _ => 0.0,
        }
    }

    /// 直接设定数值(与 bump 同一套范围)。
    fn set_num(&mut self, kind: &str, v: f64) {
        match kind {
            "volume" => {
                self.settings.master_volume = v.clamp(0.0, 1.0) as f32;
                self.send(json!({"cmd": "set_master", "v": self.settings.master_volume}));
            }
            "bgm" => {
                self.settings.volume = v.clamp(0.0, 1.0) as f32;
                self.send(json!({"cmd": "set_volume", "v": self.settings.volume}));
            }
            "hits" => {
                self.settings.hits_volume = v.clamp(0.0, 1.0) as f32;
                let v = if self.settings.hitsound { self.settings.hits_volume } else { 0.0 };
                self.send(json!({"cmd": "set_hits_volume", "v": v}));
            }
            "speed" => {
                self.settings.speed = v.clamp(0.25, 2.0) as f32;
                self.send(json!({"cmd": "set_speed", "x": self.settings.speed}));
            }
            "target_star" => self.settings.target_star = Some(v.clamp(0.5, 12.0)),
            "offset" => {
                self.settings.audio_offset_ms = v.clamp(-200.0, 200.0) as f32;
                self.send(json!({"cmd": "set_offset", "ms": self.settings.audio_offset_ms}));
            }
            "bg_opacity" => {
                self.settings.bg_opacity = v.clamp(0.0, 1.0) as f32;
                self.send(json!({"cmd": "set_bg_opacity", "v": self.settings.bg_opacity}));
            }
            "cursor_size" => {
                self.settings.cursor_size = v.clamp(0.1, 2.0) as f32;
                self.send(json!({"cmd": "set_cursor_size", "x": self.settings.cursor_size}));
            }
            "star_min" => {
                self.settings.star_min = v.clamp(0.0, 10.0);
                self.star_min = self.settings.star_min;
                self.apply_filter();
            }
            _ => {}
        }
        self.persist();
    }

    // ---- 文本编辑 ----

    fn begin_edit(&mut self, field: Field, window: &mut Window) {
        let value = match field {
            Field::Search => self.search.clone(),
            Field::LazerDir => self.settings.lazer_dir.clone().unwrap_or_default(),
            Field::StableDir => self.settings.stable_dir.clone().unwrap_or_default(),
            Field::Ffmpeg => self.settings.ffmpeg.clone().unwrap_or_default(),
            Field::Ffprobe => self.settings.ffprobe.clone().unwrap_or_default(),
            Field::Skin => self.settings.skin.clone().unwrap_or_default(),
            _ => {
                // 数值输入:点击后清空,直接输入新值;留空提交则保持原值
                String::new()
            }
        };
        // 切换编辑目标时,先把上一个数值输入落账
        if self.edit.is_some() && self.edit.as_ref().map(|e| e.field) != Some(field) {
            if let Some(prev) = self.edit.as_ref().map(|e| e.field) {
                if Self::field_kind(prev).is_some() {
                    self.commit_edit();
                }
            }
        }
        self.edit = Some(Editor { field, buf: value });
        if let Some(h) = self.focus.get(&field) {
            window.focus(h);
        }
    }

    /// 数值内联输入:任何按钮点击前先落账。
    fn commit_num_edit(&mut self) {
        if let Some(ed) = self.edit.as_ref() {
            if Self::field_kind(ed.field).is_some() {
                self.commit_edit();
            }
        }
    }

    fn commit_edit(&mut self) {
        let Some(ed) = self.edit.take() else { return };
        let text = ed.buf.trim().to_string();
        let opt = |t: String| if t.is_empty() { None } else { Some(t) };
        match ed.field {
            Field::Search => {
                self.search = ed.buf;
                self.apply_filter();
            }
            Field::LazerDir => {
                self.settings.lazer_dir = opt(text);
                self.spawn_library_load();
            }
            Field::StableDir => {
                self.settings.stable_dir = opt(text);
                self.spawn_library_load();
            }
            Field::Ffmpeg => {
                self.settings.ffmpeg = opt(text);
                self.send(json!({"cmd": "set_ffmpeg_bins", "ffmpeg": self.settings.ffmpeg, "ffprobe": self.settings.ffprobe}));
            }
            Field::Ffprobe => {
                self.settings.ffprobe = opt(text);
                self.send(json!({"cmd": "set_ffmpeg_bins", "ffmpeg": self.settings.ffmpeg, "ffprobe": self.settings.ffprobe}));
            }
            Field::Skin => self.settings.skin = opt(text),
            _ => {
                if let Some(kind) = Self::field_kind(ed.field) {
                    if let Ok(v) = text.parse::<f64>() {
                        let (_, scale) = Self::num_unit(kind);
                        self.set_num(kind, v * scale);
                    }
                }
            }
        }
        self.persist();
    }

    fn on_key(&mut self, field: Field, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(ed) = &mut self.edit else { return };
        if ed.field != field {
            return;
        }
        let mut commit = false;
        match ev.keystroke.key.as_str() {
            "backspace" => {
                ed.buf.pop();
            }
            "escape" => {
                self.edit = None;
            }
            "enter" => commit = true,
            _ => {
                if let Some(ch) = &ev.keystroke.key_char {
                    if Self::field_kind(field).is_some() {
                        if ch.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-') {
                            ed.buf.push_str(ch);
                        }
                    } else {
                        ed.buf.push_str(ch);
                    }
                }
            }
        }
        if field == Field::Search {
            if let Some(ed) = &self.edit {
                self.search = ed.buf.clone();
            }
            self.apply_filter();
        }
        if commit {
            self.commit_edit();
        }
        cx.notify();
    }

    fn minimize_window(&mut self, cx: &mut Context<Self>) {
        let handle = WINDOW.lock().ok().and_then(|g| *g);
        if let Some(handle) = handle {
            cx.defer(move |cx: &mut gpui::App| {
                let _ = handle.update(cx, |_, window, _| window.minimize_window());
            });
        }
    }

    /// 延后一帧激活窗口:不能在 update 内部再 update 同一个 App(会 panic)。
    fn activate_window(&mut self, cx: &mut Context<Self>) {
        let handle = WINDOW.lock().ok().and_then(|g| *g);
        if let Some(handle) = handle {
            cx.defer(move |cx: &mut gpui::App| {
                let _ = handle.update(cx, |_, window, _| window.activate_window());
            });
        }
    }

    /// 刷新播放状态(定时器调用);返回是否需要重绘。
    fn poll(&mut self, cx: &mut Context<Self>) -> bool {
        let mut dirty = false;
        // 曲终自动切歌(单曲循环 = 重播本曲;随机 = 换一首;顺序 = 下一首)
        let ended = {
            let mut st = self.status.lock().unwrap();
            std::mem::take(&mut st.ended)
        };
        if ended && !self.queue.is_empty() {
            match self.settings.play_mode {
                crate::playlist::PlayMode::SingleLoop => self.play_index(self.pos),
                crate::playlist::PlayMode::Random => {
                    let n = self.queue.len();
                    let mut idx = (self.pos + 1) % n;
                    if n > 1 {
                        let t = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.subsec_nanos() as usize)
                            .unwrap_or(0);
                        idx = t % n;
                        if idx == self.pos {
                            idx = (idx + 1) % n;
                        }
                    }
                    self.play_index(idx);
                }
                crate::playlist::PlayMode::ListOrder => self.step(1),
            }
        }
        // 设置页滚动条跟随:偏移变化才重绘
        {
            let off: f32 = self.settings_scroll.offset().y.into();
            if (off - self.last_settings_off).abs() > 0.5 {
                self.last_settings_off = off;
                dirty = true;
            }
        }
        if CLOSE_ASK.swap(false, std::sync::atomic::Ordering::Relaxed) {
            self.close_ask = true;
            self.close_remember = false;
            dirty = true;
        }
        if REPLACE_REQUESTED.swap(false, std::sync::atomic::Ordering::Relaxed) {
            #[cfg(target_os = "linux")]
            let _ = crate::kde::restore_wallpaper();
            std::process::exit(0);
        }
        if SHOW_REQUESTED.swap(false, std::sync::atomic::Ordering::Relaxed) {
            self.activate_window(cx);
        }
        // 曲库后台解析完成:应用结果
        let payload = self.lib_loading.lock().unwrap().take();
        if let Some(p) = payload {
            self.set_idx = p
                .lib
                .sets
                .iter()
                .enumerate()
                .map(|(i, s)| (s.id.clone(), i))
                .collect();
            self.lib = Some(p.lib);
            self.all_rows = p.all_rows;
            self.collections = p.collections;
            self.cover_paths = p.cover_paths;
            self.skins = p.skins;
            self.status_line = p.status_line;
            self.loading = false;
            {
                let mut cache = self.thumbs.lock().unwrap();
                for (k, v) in p.thumbs {
                    cache.insert(k, v);
                }
            }
            self.hero.clear();
            self.apply_filter();
            if let Some((set_id, sha2, mods)) = self.pending_play.take() {
                self.play(&set_id, sha2.as_deref(), mods);
            }
            dirty = true;
        }
        let picked: Vec<(Field, PathBuf)> = PICKED.lock().unwrap().drain(..).collect();
        for (field, path) in picked {
            let text = path.to_string_lossy().into_owned();
            match field {
                Field::LazerDir => {
                    self.settings.lazer_dir = Some(text);
                    self.spawn_library_load();
                }
                Field::StableDir => {
                    self.settings.stable_dir = Some(text);
                    self.spawn_library_load();
                }
                Field::Ffmpeg => {
                    self.settings.ffmpeg = Some(text);
                    self.send(json!({"cmd": "set_ffmpeg_bins", "ffmpeg": self.settings.ffmpeg, "ffprobe": self.settings.ffprobe}));
                }
                Field::Ffprobe => {
                    self.settings.ffprobe = Some(text);
                    self.send(json!({"cmd": "set_ffmpeg_bins", "ffmpeg": self.settings.ffmpeg, "ffprobe": self.settings.ffprobe}));
                }
                _ => {}
            }
            self.persist();
        }
        for action in crate::tray::poll() {
            match action {
                crate::tray::TrayAction::Prev => self.step(-1),
                crate::tray::TrayAction::Next => self.step(1),
                crate::tray::TrayAction::Toggle => self.toggle_pause(),
                crate::tray::TrayAction::Show => self.activate_window(cx),
                crate::tray::TrayAction::Quit => {
                    #[cfg(target_os = "linux")]
                    let _ = crate::kde::restore_wallpaper();
                    std::process::exit(0);
                }
            }
        }
        let (t, playing, dur) = {
            let st = self.status.lock().unwrap();
            (st.t, st.playing, st.duration)
        };
        self.playing = playing;
        // seek 后 400ms 内忽略迟到的旧状态,避免进度条被拉回去
        if self.seek_guard.elapsed().as_millis() < 400 {
            // 只同步播放状态,不重锚
        } else if (t - self.prog_ms).abs() > 0.05 {
            self.prog_ms = t;
            self.prog_at = std::time::Instant::now();
            dirty = true;
        }
        // 播放中正常刷新(250ms),进度条平滑
        if playing {
            dirty = true;
        }
        let _ = dur;
        if let Some((_, at)) = &self.toast {
            if at.elapsed() > Duration::from_secs(3) {
                self.toast = None;
                dirty = true;
            }
        }
        dirty
    }
}

/// osu! 星级渐变色带(lazer DifficultyColour 采样点,与旧 UI 一致)。
const STAR_SPECTRUM: &[(f64, [u8; 3])] = &[
    (0.1, [0xaa, 0xaa, 0xaa]),
    (0.1, [0x42, 0x90, 0xfb]),
    (1.25, [0x4f, 0xc0, 0xff]),
    (2.0, [0x4f, 0xff, 0xd5]),
    (2.5, [0x7c, 0xff, 0x4f]),
    (3.3, [0xf6, 0xf0, 0x5c]),
    (4.2, [0xff, 0x80, 0x68]),
    (4.9, [0xff, 0x4e, 0x6f]),
    (5.8, [0xc6, 0x45, 0xb8]),
    (6.7, [0x65, 0x63, 0xde]),
    (7.7, [0x18, 0x15, 0x8e]),
    (9.0, [0x00, 0x00, 0x00]),
    (10.0, [0x00, 0x00, 0x00]),
];

const STAR_TEXT_SPECTRUM: &[(f64, [u8; 3])] = &[
    (9.0, [0xf6, 0xf0, 0x5c]),
    (9.9, [0xff, 0x80, 0x68]),
    (10.6, [0xff, 0x4e, 0x6f]),
    (11.5, [0xc6, 0x45, 0xb8]),
    (12.4, [0x65, 0x63, 0xde]),
];

fn sample_gradient(spectrum: &[(f64, [u8; 3])], value: f64) -> [u8; 3] {
    if value <= spectrum[0].0 {
        return spectrum[0].1;
    }
    for i in 1..spectrum.len() {
        let (pos, colour) = spectrum[i];
        if value <= pos {
            let (prev_pos, prev) = spectrum[i - 1];
            let t = if pos == prev_pos { 0.0 } else { (value - prev_pos) / (pos - prev_pos) };
            let mut out = [0u8; 3];
            for k in 0..3 {
                out[k] = (prev[k] as f64 + (colour[k] as f64 - prev[k] as f64) * t).round() as u8;
            }
            return out;
        }
    }
    spectrum[spectrum.len() - 1].1
}

/// 谱面集状态文字标签(ranked / approved / loved / qualified / pending / graveyard)。
fn status_badge(status: i32) -> impl IntoElement {
    let (label, bg, fg) = match status {
        1 => ("ranked", 0xb3ff66u32, 0x1a2a00u32),
        2 => ("approved", 0x99ee55, 0x1a2a00),
        3 => ("qualified", 0xffd966, 0x2a2000),
        4 => ("loved", 0xff66aa, 0xffffff),
        0 => ("pending", 0x6aa0e0, 0xffffff),
        5 => ("graveyard", 0x777788, 0xffffff),
        _ => ("", 0x777788, 0xffffff),
    };
    if label.is_empty() {
        return div().into_any_element();
    }
    div()
        .flex_none()
        .px(px(5.0))
        .py(px(1.0))
        .rounded_full()
        .bg(rgb(bg))
        .text_color(rgb(fg))
        .text_xs()
        .child(label)
        .into_any_element()
}

fn rgb_of(c: [u8; 3]) -> gpui::Rgba {
    rgb(((c[0] as u32) << 16) | ((c[1] as u32) << 8) | c[2] as u32)
}

/// 星级徽章底色(lazer 难度色)。
fn star_bg(v: f64) -> gpui::Rgba {
    rgb_of(sample_gradient(STAR_SPECTRUM, v))
}

/// 徽章文字/图标颜色(深底浅字,高星走文字色带)。
fn star_fg(v: f64) -> gpui::Rgba {
    let x = (v * 100.0).round() / 100.0;
    if x < 6.5 {
        rgba(0x000000bf)
    } else if x < 9.0 {
        rgb(0xffd966)
    } else {
        rgb_of(sample_gradient(STAR_TEXT_SPECTRUM, x))
    }
}


/// 曲库未就绪时用播放列表快照兜底右侧信息卡。
fn fallback_from_queue(
    app: &App,
    set_id: &Option<String>,
) -> (String, String, String, f64, Vec<(String, String)>, String, bool) {
    let empty = (
        String::new(),
        String::new(),
        String::new(),
        0.0,
        Vec::new(),
        String::new(),
        false,
    );
    let Some(id) = set_id else { return empty };
    let Some(t) = app.queue.iter().find(|t| &t.set_id == id) else {
        return empty;
    };
    (
        t.title.clone(),
        t.artist.clone(),
        String::new(),
        0.0,
        Vec::new(),
        String::new(),
        app.settings.set_overrides.contains_key(id),
    )
}

fn fmt_ms(ms: f64) -> String {
    if ms <= 0.0 {
        return String::new();
    }
    let secs = (ms / 1000.0).round() as u64;
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn star_text(v: f64) -> String {
    if v > 0.0 { format!("★{v:.2}") } else { "★—".to_string() }
}

/// 五角星图标(canvas 直接画路径,避免依赖 SVG 资源)。
fn star_icon(size: f32, color: gpui::Rgba) -> impl IntoElement {
    gpui::canvas(
        move |_b, _w, _c| (),
        move |bounds, _state, window, _cx| {
            let cx = bounds.origin.x + bounds.size.width / 2.0;
            let cy = bounds.origin.y + bounds.size.height / 2.0;
            let r = bounds.size.width.min(bounds.size.height) * 0.46;
            let mut b = gpui::PathBuilder::default();
            for i in 0..10 {
                let ang = -std::f32::consts::FRAC_PI_2
                    + i as f32 * std::f32::consts::PI / 5.0;
                let rad = if i % 2 == 0 { r } else { r * 0.382 };
                let x = cx + rad * ang.cos();
                let y = cy + rad * ang.sin();
                if i == 0 {
                    b.move_to(gpui::point(x, y));
                } else {
                    b.line_to(gpui::point(x, y));
                }
            }
            b.close();
            if let Ok(path) = b.build() {
                window.paint_path(path, color);
            }
        },
    )
    .w(px(size))
    .h(px(size))
}

/// 库/队列/信息卡统一的星级徽章(彩色 pill + 星形 + 数值)。
fn star_badge(v: f64) -> impl IntoElement {
    if !(v > 0.0) {
        return div().px(px(2.0)).text_xs().text_color(rgb(pal().dim)).child(star_text(v));
    }
    let fg = star_fg(v);
    div()
        .w(px(72.0))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_1()
        .py(px(1.0))
        .rounded_full()
        .bg(star_bg(v))
        .child(star_icon(12.0, fg))
        .child(div().text_sm().text_color(fg).child(format!("{v:.2}")))
}

fn tri_next(v: Option<bool>) -> Option<bool> {
    match v {
        None => Some(true),
        Some(true) => Some(false),
        Some(false) => None,
    }
}

fn mods_label(bits: u32) -> String {
    let names = [(8u32, "HD"), (16, "HR"), (2, "EZ"), (64, "DT"), (256, "HT"), (512, "NC")];
    let on: Vec<&str> = names.iter().filter(|(b, _)| bits & b != 0).map(|(_, n)| *n).collect();
    if on.is_empty() { "无".to_string() } else { on.join("+") }
}

fn pick_text(a: &str, b: &str, unicode: bool) -> String {
    // a = 罗马音/英文,b = 原名(Unicode);为空时互相兜底
    let (primary, other) = if unicode { (b, a) } else { (a, b) };
    if primary.trim().is_empty() {
        other.to_string()
    } else {
        primary.to_string()
    }
}

fn cover_path(set: &crate::lazer::LazerSet, root: Option<&Path>) -> Option<PathBuf> {
    let file = crate::lazer::pick_cover_file(&set.files, None)?;
    if let Some(root) = set.root.as_deref() {
        let p = Path::new(root).join(&file.filename);
        return p.is_file().then_some(p);
    }
    let root = root?;
    Some(root.join("files").join(crate::lazer::blob_relative_path(&file.hash)))
}

/// 生成缩略图:GPUI 的 `img()` 会按原图解码成纹理,直接喂大图会
/// 把内存/显存打爆(OOM)。
fn thumb_at(path: &Path, size: u32) -> Option<Arc<gpui::Image>> {
    let data = std::fs::read(path).ok()?;
    let img = image::load_from_memory(&data).ok()?;
    let small = img.thumbnail(size, size);
    let mut out = std::io::Cursor::new(Vec::new());
    small.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(Arc::new(gpui::Image::from_bytes(
        gpui::ImageFormat::Png,
        out.into_inner(),
    )))
}

/// 拉起壁纸子进程。
fn spawn_wallpaper(status: Arc<Mutex<PlayStatus>>) -> Option<crate::ctl::ChildCtl> {
    let exe = std::env::current_exe().ok()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--wallpaper")
        .env("NO_MSAA", "1")
        .env("SB_CACHE_MB", "192")
        .env("SB_GPU_MB", "1024")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    #[cfg(target_os = "linux")]
    cmd.env("ARIA_OUTPUT", "kde");
    let mut child = cmd.spawn().ok()?;
    let stdin = child.stdin.take()?;
    if let Some(out) = child.stdout.take() {
        use std::io::BufRead;
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                match v["event"].as_str() {
                    Some("status") => {
                        let t = v["tMs"].as_f64().or_else(|| v["t_ms"].as_f64()).unwrap_or(0.0);
                        let playing = v["playing"].as_bool().unwrap_or(false);
                        if let Ok(mut st) = status.lock() {
                            st.t = t / 1000.0;
                            st.playing = playing;
                        }
                    }
                    Some("loaded") => {
                        if let Ok(mut st) = status.lock() {
                            st.duration = v["durationMs"].as_f64().unwrap_or(0.0) / 1000.0;
                        }
                    }
                    Some("ended") => {
                        if let Ok(mut st) = status.lock() {
                            st.ended = true;
                        }
                    }
                    Some("exited") => break,
                    _ => {}
                }
            }
        });
    }
    Some(crate::ctl::ChildCtl::from_child(child, stdin))
}

// ---- 视图 ----

impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 窗口尺寸变化(放大/缩小)时强制重新测量列表,否则虚拟列表沿用旧的测量结果,
        // 设置页等宽内容不会重排。
        let vs = window.viewport_size();
        let size = (f32::from(vs.width), f32::from(vs.height));
        if size != self.last_size {
            self.last_size = size;
            let n = self.settings_list.item_count();
            self.settings_list.reset(n);
            let l = self.lib_list.item_count();
            self.lib_list.reset(l);
            let q = self.queue_list.item_count();
            self.queue_list.reset(q);
        }
        let header = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .bg(rgb(pal().panel))
            .child(div().text_xl().child("Aria"))
            .child(div().text_xs().text_color(rgb(pal().dim)).child("osu! storyboard 桌面动态壁纸"))
            .child({
                let running = self.child.is_some();
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_0p5()
                    .rounded_full()
                    .bg(if running { rgb(pal().ok_bg) } else { rgb(pal().ctl) })
                    .text_xs()
                    .text_color(if running { rgb(pal().ok_fg) } else { rgb(pal().dim) })
                    .child(if running { "● 运行中" } else { "○ 未运行" })
            })
            .child(div().flex_1())
            .child(div().text_xs().text_color(rgb(pal().dim)).child(self.status_line.clone()))
            .child(self.small_btn("关于", cx, |this, cx| {
                this.about = true;
                cx.notify();
            }));

        // 左上角 tab 切换:播放列表 / 曲库 / 设置(与旧 UI 一致)
        let tabs = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .bg(rgb(pal().panel))
            .child(self.tab(format!("播放列表 · {}", self.queue.len()), View::Queue, cx))
            .child(self.tab("曲库".to_string(), View::Library, cx))
            .child(self.tab("设置".to_string(), View::Settings, cx));

        let content = match self.view {
            View::Queue => self.queue_panel(cx).into_any_element(),
            View::Library => self.library_panel(cx).into_any_element(),
            View::Settings => self.settings_panel(cx).into_any_element(),
        };

        let body = div()
            .flex_1()
            .flex()
            .flex_row()
            .min_h(px(0.0))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .min_w(px(0.0))
                    .child(tabs)
                    .child(div().flex_1().min_h(px(0.0)).child(content)),
            )
            .child(self.side_panel(cx));

        let bar = div().flex_none().child(self.play_bar(cx));
        let toast = self.toast.clone();
        let mut root = div()
            .id("app-root")
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _e, _w, cx| {
                if this.edit_armed {
                    this.edit_armed = false;
                } else if this.edit.is_some() {
                    this.commit_num_edit();
                    cx.notify();
                }
            }))
            .size_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(rgb(pal().bg))
            .text_color(rgb(pal().text))
            .child(header)
            .child(body)
            .child(bar);
        if let Some((msg, _)) = toast {
            root = root.child(
                div().px_3().py_1().bg(rgb(pal().toast_bg)).text_sm().text_color(rgb(pal().toast_fg)).child(msg),
            );
        }
        if self.about {
            root = root.child(self.about_modal(cx));
        }
        if self.mods_dialog.is_some() {
            root = root.child(self.mods_modal(cx));
        }
        if self.overrides.is_some() {
            root = root.child(self.override_modal(cx));
        }
        if self.clear_confirm {
            root = root.child(self.clear_modal(cx));
        }
        if self.close_ask {
            root = root.child(self.close_ask_modal(cx));
        }
        if self.quick_opts {
            root = root.child(self.quick_opts_modal(cx));
        }
        if self.ctx_multi || self.ctx.is_some() {
            root = root.child(gpui::deferred(self.ctx_menu(cx)).with_priority(30));
        }
        if self.star_popup {
            root = root.child(self.star_popup_modal(cx));
        }
        root
    }
}

impl App {
    fn tab(&self, label: impl Into<String>, view: View, cx: &mut Context<Self>) -> impl IntoElement {
        let label = label.into();
        let active = self.view == view;
        let id = format!("tab{:?}", view);
        div()
            .id(gpui::ElementId::Name(id.into()))
            .px_4()
            .py_2()
            .rounded_md()
            .bg(if active { rgb(pal().accent) } else { rgb(pal().ctl) })
            .text_sm()
            .child(label.clone())
            .on_click(cx.listener(move |this, _e, _w, cx| {
                this.view = view;
                cx.notify();
            }))
    }

    /// 下拉选择:按钮 + 点开的浮层(anchored,自动避让窗口边缘)。
    fn dropdown(
        &self,
        kind: DropKind,
        current: String,
        options: Vec<(String, String)>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.dropdown_w(kind, current, options, None, cx)
    }

    fn dropdown_w(
        &self,
        kind: DropKind,
        current: String,
        options: Vec<(String, String)>,
        width: Option<f32>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let open = self.open_menu == Some(kind);
        let current_text = options
            .iter()
            .find(|(v, _)| *v == current)
            .map(|(_, l)| l.clone())
            .unwrap_or_else(|| current.clone());
        // 菜单高度 = 内容高度(选项固定 34px),最多 420px 后内部滚动;
        // 不用虚拟列表,避免状态残留导致高度与内容不一致。
        let menu = if open {
            let mut items: Vec<gpui::AnyElement> = Vec::new();
            for (value, text) in options.clone() {
                let v = value.clone();
                let selected = value == current;
                items.push(
                    div()
                        .id(gpui::ElementId::Name(format!("opt{kind:?}{value}").into()))
                        .w_full()
                        .h(px(34.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .px_3()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|st| st.bg(rgb(pal().ctl_hover)))
                        .bg(if selected { rgb(pal().accent) } else { rgb(pal().panel) })
                        .text_color(if selected { rgb(0xffffff) } else { rgb(pal().text) })
                        .child(text)
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.pick(kind, v.clone());
                            this.open_menu = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
            // 播放条里的下拉向上展开,避免被窗口底部截断
            let open_up = matches!(
                kind,
                DropKind::QueueDiff | DropKind::Speed | DropKind::PlayMode
            );
            Some(
                div()
                    .absolute()
                    .left(px(0.0))
                    .map(|d| if open_up { d.bottom(px(36.0)) } else { d.top(px(26.0)) })
                    .child(
                        div()
                            .relative()
                            .w(px(240.0))
                            .flex()
                            .flex_col()
                            .rounded_md()
                            .overflow_hidden()
                            .border_1()
                            .border_color(rgb(pal().line))
                            .bg(rgb(pal().panel))
                            .child(
                                div()
                                    .id(gpui::ElementId::Name(format!("ddmenu{kind:?}").into()))
                                    .w_full()
                                    .max_h(px(420.0))
                                    .overflow_y_scroll()
                                    .track_scroll(&self.menu_scroll)
                                    .on_scroll_wheel(cx.listener(|_t, _e, _w, cx| cx.notify()))
                                    .flex()
                                    .flex_col()
                                    .children(items),
                            )
                            .child(self.tracked_scrollbar(&self.menu_scroll)),
                    ),
            )
        } else {
            None
        };
        div()
            .relative()
            .flex()
            .flex_col()
            .child(
                div()
                    .id(gpui::ElementId::Name(format!("dd{kind:?}").into()))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .flex_none()
                    .map(|d| match width { Some(w) => d.w(px(w)), None => d })
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(pal().ctl))
                    .text_sm()
                    .cursor_pointer()
                    .hover(|st| st.bg(rgb(pal().ctl_hover)))
                    .overflow_hidden()
                    .child(format!("{} ▾", current_text))
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.open_menu = if this.open_menu == Some(kind) {
                            None
                        } else {
                            Some(kind)
                        };
                        cx.notify();
                    })),
            )
            .children(menu.map(|m| gpui::deferred(m).with_priority(10)))
    }

    /// 下拉选中后应用。
    fn pick(&mut self, kind: DropKind, value: String) {
        match kind {
            DropKind::Collection => {
                self.settings.collection = if value.is_empty() { None } else { Some(value) };
                self.apply_filter();
            }
            DropKind::Source => {
                self.settings.source = value;
                self.spawn_library_load();
            }
            DropKind::Sort => {
                self.sort = match value.as_str() {
                    "title" => SortKey::Title,
                    "star" => SortKey::Star,
                    "artist" => SortKey::Artist,
                    "creator" => SortKey::Creator,
                    "size" => SortKey::Size,
                    "collection" => SortKey::CollectionOrder,
                    _ => SortKey::Added,
                };
                self.apply_filter();
            }
            DropKind::PlayMode => {
                self.settings.play_mode = match value.as_str() {
                    "single" => crate::playlist::PlayMode::SingleLoop,
                    "random" => crate::playlist::PlayMode::Random,
                    _ => crate::playlist::PlayMode::ListOrder,
                };
                self.send(json!({"cmd": "set_loop",
                    "on": self.settings.play_mode == crate::playlist::PlayMode::SingleLoop}));
            }
            DropKind::Theme => {
                self.settings.theme_mode = value;
                apply_palette(self.settings.theme_mode != "light", &self.settings.accent);
            }
            DropKind::Accent => {
                self.settings.accent = value;
                apply_palette(self.settings.theme_mode != "light", &self.settings.accent);
            }
            DropKind::CloseAction => {
                self.settings.close_action = if value.is_empty() { None } else { Some(value) };
            }
            DropKind::Skin => {
                self.settings.skin = if value.is_empty() { None } else { Some(value) };
                self.send(json!({"cmd": "set_skin", "path": self.resolve_skin()}));
            }
            DropKind::Monitor => {
                self.settings.monitor = if value.is_empty() { None } else { Some(value) };
                if !self.queue.is_empty() {
                    self.play_index(self.pos.min(self.queue.len().saturating_sub(1)));
                }
            }
            DropKind::AudioDevice => {
                self.settings.audio_device = if value.is_empty() { None } else { Some(value) };
                self.send(json!({"cmd": "set_audio_device", "id": self.settings.audio_device}));
            }
            DropKind::Upscale => {
                self.settings.upscale = value;
                self.send(json!({"cmd": "set_upscale", "mode": self.settings.upscale, "quality": self.settings.upscale_quality}));
            }
            DropKind::UpscaleQuality => {
                self.settings.upscale_quality = value;
                self.send(json!({"cmd": "set_upscale", "mode": self.settings.upscale, "quality": self.settings.upscale_quality}));
            }
            DropKind::Fps => {
                self.settings.fps = value.parse().unwrap_or(0);
                self.send(json!({"cmd": "set_fps", "fps": self.settings.fps}));
            }
            DropKind::QuickMods => {
                self.settings.quick_mods = value.parse().unwrap_or(0);
            }
            DropKind::Diff => {
                self.settings.target_star = match value.as_str() {
                    "hardest" => None,
                    "easiest" => Some(-1.0),
                    other => other.parse::<f64>().ok().filter(|v| *v > 0.0),
                };
            }
            DropKind::RenderMode => {
                self.settings.render_mode = value;
            }
            DropKind::Speed => {
                self.settings.speed = value.parse().unwrap_or(1.0);
                self.send(json!({"cmd": "set_speed", "x": self.settings.speed}));
            }
            DropKind::QueueDiff => {
                let i = self.pos;
                if let Some(t) = self.queue.get_mut(i) {
                    t.sha2 = if value.is_empty() { None } else { Some(value) };
                }
                self.save_queue();
                if !self.queue.is_empty() {
                    self.play_index(i);
                }
            }
        }
        self.persist();
    }

    /// 竖向滚动条(按 offset/max_offset 换算,relative 定位不依赖像素高度)。
    fn scrollbar(&self, handle: &gpui::UniformListScrollHandle) -> impl IntoElement {
        let (frac, ratio) = {
            let st = handle.0.borrow();
            let off = f32::from(st.base_handle.offset().y);
            let max = f32::from(st.base_handle.max_offset().height);
            let view = f32::from(st.base_handle.bounds().size.height);
            let ratio = if view + max > 0.0 { (view / (view + max)).clamp(0.05, 1.0) } else { 1.0 };
            let frac = if max > 0.0 { (off / max).clamp(0.0, 1.0) } else { 0.0 };
            (frac, ratio)
        };
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .right(px(2.0))
            .w(px(4.0))
            .rounded_full()
            .bg(rgb(pal().line2))
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .h(gpui::relative(ratio))
                    .top(gpui::relative(frac * (1.0 - ratio)))
                    .rounded_full()
                    .bg(rgb(pal().thumb)),
            )
    }

    fn small_btn(
        &self,
        label: impl Into<String>,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        self.small_btn_w(label, None, cx, f)
    }

    /// 固定宽度的工具按钮(工具栏对齐用)。
    fn small_btn_w(
        &self,
        label: impl Into<String>,
        width: Option<f32>,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let label = label.into();
        // 稳定 id:GPUI 的 click 需要按下/抬起命中同一元素,id 每帧变化
        // 会导致点击永远不触发(心跳每秒重绘一次)。
        let id = format!("btn::{label}");
        let mut btn = div()
            .id(gpui::ElementId::Name(id.into()))
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(pal().ctl))
            .text_sm()
            .cursor_pointer()
            .hover(|st| st.bg(rgb(pal().ctl_hover)))
            .child(label.clone());
        if let Some(w) = width {
            btn = btn
                .w(px(w))
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .overflow_hidden();
        }
        btn
            .on_click(cx.listener(move |this, _e, _w, cx| {
                this.commit_num_edit();
                f(this, cx);
                cx.notify();
            }))
    }

    fn text_field(&self, field: Field, value: &str, width: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.edit.as_ref().is_some_and(|e| e.field == field);
        let shown = if active {
            self.edit.as_ref().map(|e| e.buf.clone()).unwrap_or_default()
        } else {
            value.to_string()
        };
        let handle = self.focus.get(&field).cloned();
        let mut el = div()
            .id(gpui::ElementId::Name(format!("tf{field:?}").into()))
            .w(px(width))
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(pal().line2))
            .border_1()
            .border_color(if active { rgb(pal().accent) } else { rgb(pal().line) })
            .text_sm()
            .child(if active {
                format!("{}▏", shown)
            } else if shown.is_empty() {
                if field == Field::Search { "搜索".to_string() } else { "…".to_string() }
            } else {
                shown
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _e, window, cx| {
                    this.begin_edit(field, window);
                    this.edit_armed = true;
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(move |this, ev, _w, cx| {
                this.on_key(field, ev, cx);
            }));
        if let Some(h) = handle {
            el = el.track_focus(&h);
        }
        el
    }

    fn library_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = div()
            .flex()
            .flex_col()
            .px_3()
            .py_2()
            .gap_1()
            .bg(rgb(pal().panel))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(self.text_field(Field::Search, &self.search.clone(), 240.0, cx))
                    .child(self.dropdown_w(
                        DropKind::Sort,
                        match self.sort {
                            SortKey::Added => "added",
                            SortKey::Title => "title",
                            SortKey::Star => "star",
                            SortKey::Artist => "artist",
                            SortKey::Creator => "creator",
                            SortKey::Size => "size",
                            SortKey::CollectionOrder => "collection",
                        }
                        .to_string(),
                        vec![
                            ("added".into(), "按导入时间".into()),
                            ("artist".into(), "按作者".into()),
                            ("title".into(), "按标题".into()),
                            ("creator".into(), "按谱师".into()),
                            ("star".into(), "按难度".into()),
                            ("size".into(), "按大小".into()),
                            ("collection".into(), "按收藏顺序".into()),
                        ],
                        None,
                        cx,
                    ))
                    .child(self.small_btn(if self.desc { "降序 ↓" } else { "升序 ↑" }, cx, |this, _| {
                        this.desc = !this.desc;
                        this.apply_filter();
                    }))
                    .child({
                        let label = if self.star_max > 0.0 {
                            format!("★{:.1} – {:.1}", self.star_min, self.star_max)
                        } else {
                            format!("★{:.1} – ∞", self.star_min)
                        };
                        div()
                            .id("star-filter")
                            .w(px(120.0))
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_center()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|st| st.bg(rgb(pal().ctl_hover)))
                            .bg(rgb(pal().ctl))
                            .text_sm()
                            .child(label)
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.star_popup = !this.star_popup;
                                if this.star_popup {
                                    this.star_draft = (this.star_min, this.star_max);
                                }
                                cx.notify();
                            }))
                    })
                    .child(self.small_btn("刷新", cx, |this, _| this.spawn_library_load()))
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("sb-toggle")
                            .w(px(104.0))
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_center()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|st| st.bg(rgb(pal().ctl_hover)))
                            .bg(if self.sb_only { rgb(pal().accent) } else { rgb(pal().ctl) })
                            .text_color(if self.sb_only { rgb(0xffffff) } else { rgb(pal().text) })
                            .text_sm()
                            .child("仅 SB/视频")
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.sb_only = !this.sb_only;
                                this.apply_filter();
                                cx.notify();
                            })),
                    )
                    .child(self.dropdown_w(DropKind::Collection, self.settings.collection.clone().unwrap_or_default(), {
                            let mut opts = vec![("".to_string(), "全部谱面".to_string())];
                            opts.extend(self.collections.iter().map(|c| (c.clone(), c.clone())));
                            opts
                        }, Some(152.0), cx))

                    .child(self.small_btn("全部加入", cx, |this, _| this.add_filtered())),
            );

        // 虚拟列表:行 + 展开的难度块各自占一项
        let mut items: Vec<(usize, bool)> = Vec::new();
        for (i, r) in self.rows.iter().enumerate() {
            items.push((i, false));
            if self.expanded.as_deref() == Some(r.set_id.as_str()) {
                items.push((i, true));
            }
        }
        if self.lib_list.item_count() != items.len() {
            self.lib_list.reset(items.len());
        }
        let entity = cx.entity();
        let list = gpui::list(self.lib_list.clone(), move |ix, _w, cx| {
            let app = entity.read(cx);
            let Some((ri, is_chips)) = items.get(ix).copied() else {
                return div().into_any_element();
            };
            let Some(row) = app.rows.get(ri) else {
                return div().into_any_element();
            };
            if is_chips {
                Self::lib_chips_el(app, &entity, &row.set_id)
            } else {
                Self::lib_row_el(app, &entity, row)
            }
        })
        .w_full()
        .h_full()
        .min_h(px(0.0));
        let list = div()
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .on_scroll_wheel(cx.listener(|_t, _e, _w, cx| cx.notify()))
            .child(list)
            .child(self.lib_scrollbar(cx));
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(toolbar)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .text_sm()
                    .text_color(rgb(pal().dim))
                    .child(format!(
                        "{} · 显示 {} / 共 {}",
                        if self.settings.source == "stable" { "osu!stable" } else { "osu!lazer" },
                        self.rows.len(),
                        self.all_rows.len()
                    ))
                    .child(
                        self.settings
                            .lazer_dir
                            .clone()
                            .or_else(|| self.settings.stable_dir.clone())
                            .or_else(|| {
                                crate::lazer::data_root().map(|p| p.to_string_lossy().into_owned())
                            })
                            .unwrap_or_default(),
                    ),
            )
            .child(list)
    }

    /// 普通滚动容器的跟随型滚动条(轮滚时刷新)。
    fn tracked_scrollbar(&self, h: &gpui::ScrollHandle) -> impl IntoElement {
        let off: f32 = h.offset().y.into();
        let max: f32 = h.max_offset().height.into();
        let view: f32 = h.bounds().size.height.into();
        let ratio = if view + max > 0.0 {
            (view / (view + max)).clamp(0.05, 1.0)
        } else {
            1.0
        };
        let frac = if max > 0.0 { (off / max).clamp(0.0, 1.0) } else { 0.0 };
        if max <= 0.0 {
            return div().id(gpui::ElementId::Name("sb-hidden".into()));
        }
        div()
            .id("sb-track")
            .absolute()
            .right(px(2.0))
            .top_0()
            .bottom_0()
            .w(px(8.0))
            .rounded_full()
            .bg(rgb(pal().line2))
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .h(gpui::relative(ratio))
                    .top(gpui::relative(frac * (1.0 - ratio)))
                    .rounded_full()
                    .bg(rgb(pal().thumb)),
            )
    }

    /// 通用 ListState 滚动条(可拖拽)。
    fn list_scrollbar(
        &self,
        state: &gpui::ListState,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let max: f32 = state.max_offset_for_scrollbar().height.into();
        let view: f32 = state.viewport_bounds().size.height.into();
        let off: f32 = (-state.scroll_px_offset_for_scrollbar().y).into();
        let ratio = if view + max > 0.0 {
            (view / (view + max)).clamp(0.05, 1.0)
        } else {
            1.0
        };
        let frac = if max > 0.0 { (off / max).clamp(0.0, 1.0) } else { 0.0 };
        if max <= 0.0 {
            return div().id(gpui::ElementId::Name(format!("bar{id}hidden").into()));
        }
        let st = state.clone();
        div()
            .id(gpui::ElementId::Name(format!("bar{id}").into()))
            .absolute()
            .right(px(2.0))
            .top_0()
            .bottom_0()
            .w(px(8.0))
            .rounded_full()
            .bg(rgb(pal().line2))
            .on_drag_move::<ListBarDrag>(cx.listener(move |_t, e: &gpui::DragMoveEvent<ListBarDrag>, _w, cx| {
                let track_h: f32 = e.bounds.size.height.into();
                if track_h <= 0.0 {
                    return;
                }
                let thumb_h = track_h * ratio;
                let y: f32 = e.event.position.y.into();
                let rel = (y - f32::from(e.bounds.origin.y) - thumb_h / 2.0)
                    .clamp(0.0, (track_h - thumb_h).max(0.0));
                let f = if track_h - thumb_h > 0.0 { rel / (track_h - thumb_h) } else { 0.0 };
                let max: f32 = st.max_offset_for_scrollbar().height.into();
                let cur: f32 = (-st.scroll_px_offset_for_scrollbar().y).into();
                st.scroll_by(px(f * max - cur));
                cx.notify();
            }))
            .child(
                div()
                    .id(gpui::ElementId::Name(format!("th{id}").into()))
                    .absolute()
                    .left_0()
                    .right_0()
                    .h(gpui::relative(ratio))
                    .top(gpui::relative(frac * (1.0 - ratio)))
                    .rounded_full()
                    .bg(rgb(pal().thumb))
                    .on_drag(ListBarDrag, |_, _, _, cx| cx.new(|_| DragGhost)),
            )
    }

    /// 曲库虚拟列表滚动条(可拖拽)。
    fn lib_scrollbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let max: f32 = self.lib_list.max_offset_for_scrollbar().height.into();
        let view: f32 = self.lib_list.viewport_bounds().size.height.into();
        let off: f32 = (-self.lib_list.scroll_px_offset_for_scrollbar().y).into();
        let ratio = if view + max > 0.0 {
            (view / (view + max)).clamp(0.05, 1.0)
        } else {
            1.0
        };
        let frac = if max > 0.0 { (off / max).clamp(0.0, 1.0) } else { 0.0 };
        div()
            .id("libbar")
            .absolute()
            .right(px(2.0))
            .top_0()
            .bottom_0()
            .w(px(8.0))
            .rounded_full()
            .bg(rgb(pal().line2))
            .on_drag_move::<LibBarDrag>(cx.listener(move |this, e: &gpui::DragMoveEvent<LibBarDrag>, _w, cx| {
                let track_h: f32 = e.bounds.size.height.into();
                if track_h <= 0.0 {
                    return;
                }
                let thumb_h = track_h * ratio;
                let top: f32 = e.event.position.y.into();
                let rel = (top - f32::from(e.bounds.origin.y) - thumb_h / 2.0)
                    .clamp(0.0, (track_h - thumb_h).max(0.0));
                let f = if track_h - thumb_h > 0.0 { rel / (track_h - thumb_h) } else { 0.0 };
                let max: f32 = this.lib_list.max_offset_for_scrollbar().height.into();
                let cur: f32 = (-this.lib_list.scroll_px_offset_for_scrollbar().y).into();
                this.lib_list.scroll_by(px(f * max - cur));
                cx.notify();
            }))
            .child(
                div()
                    .id("libthumb")
                    .absolute()
                    .left_0()
                    .right_0()
                    .h(gpui::relative(ratio))
                    .top(gpui::relative(frac * (1.0 - ratio)))
                    .rounded_full()
                    .bg(rgb(pal().thumb))
                    .on_drag(LibBarDrag, |_, _, _, cx| cx.new(|_| DragGhost)),
            )
    }

    fn lib_row_el(app: &App, entity: &gpui::Entity<App>, r: &Row) -> gpui::AnyElement {
        let set_id = r.set_id.clone();
        let previewing = app.preview.as_ref().is_some_and(|(id, _)| id == &set_id);
        let expanded = app.expanded.as_deref() == Some(set_id.as_str());
        let cover = app.thumb_of(&set_id);
        let e1 = entity.clone();
        let e2 = entity.clone();
        let e3 = entity.clone();
        let e4 = entity.clone();
        let sid1 = set_id.clone();
        let sid2 = set_id.clone();
        let sid3 = set_id.clone();
        let sid4 = set_id.clone();
        div()
            .id(gpui::ElementId::Name(set_id.clone().into()))
            .relative()
            .w_full()
            .h(px(84.0))
            .overflow_hidden()
            .bg(if previewing { rgb(pal().tint_preview) } else { rgb(pal().bg) })
            .hover(|st| st.bg(rgb(pal().row_hover)))
            .border_b_1()
            .border_color(rgb(pal().line2))
            .on_mouse_down(gpui::MouseButton::Left, move |ev: &gpui::MouseDownEvent, _w, cx| {
                let _ = e1.update(cx, |this, cx| {
                    if ev.click_count >= 2 {
                        this.preview = None;
                        if this.settings.dblclick_add {
                            this.add_to_queue(&sid1);
                        } else {
                            this.play(&sid1, None, this.settings.quick_mods);
                        }
                    } else {
                        this.preview_set(&sid1, None);
                    }
                    cx.notify();
                });
            })
            .on_mouse_down(gpui::MouseButton::Right, move |_e: &gpui::MouseDownEvent, _w, cx| {
                let _ = e2.update(cx, |this, cx| {
                    this.mods_dialog = Some(ModsDialog {
                        set_id: sid2.clone(),
                        sha2: None,
                        mods: this.settings.quick_mods,
                        queue_index: None,
                        bulk: None,
                    });
                    cx.notify();
                });
            })
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .pl(px(14.0))
            .pr(px(8.0))
            .py_2()
            .child(
                div()
                    .id(gpui::ElementId::Name(format!("cv{}", r.set_id).into()))
                    .flex_none()
                    .w(px(56.0))
                    .h(px(56.0))
                    .overflow_hidden()
                    .rounded_md()
                    .on_mouse_down(gpui::MouseButton::Left, move |_e: &gpui::MouseDownEvent, _w, cx| {
                        let _ = e3.update(cx, |this, cx| {
                            if this.settings.quick_add {
                                this.add_to_queue(&sid3);
                            } else {
                                this.preview_set(&sid3, None);
                            }
                            cx.notify();
                        });
                    })
                    .child(match cover {
                        Some(img) => gpui::img(gpui::ImageSource::Image(img))
                            .w_full()
                            .h_full()
                            .object_fit(gpui::ObjectFit::Cover)
                            .rounded_md()
                            .into_any_element(),
                        None => div()
                            .w_full()
                            .h_full()
                            .rounded_md()
                            .bg(rgb(pal().line2))
                            .into_any_element(),
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .flex_1()
                    .min_w(px(0.0))
                    .pr(px(40.0))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(status_badge(r.status))
                            .child(
                                div()
                                    .text_base()
                                    .overflow_hidden()
                                    .min_w(px(0.0))
                                    .child(r.title.clone()),
                            )
                            .children(if r.diffs > 1 {
                                Some(star_badge(r.star_lo))
                            } else {
                                None
                            })
                            .children(if r.diffs > 1 && r.star_lo < r.star {
                                Some(div().text_xs().text_color(rgb(pal().dim)).child("–"))
                            } else {
                                None
                            })
                            .child(star_badge(r.star)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .overflow_hidden()
                            .text_color(rgb(pal().dim))
                            .child({
                                let filtered = app.star_min > 0.0 || app.star_max > 0.0;
                                let n = if filtered { r.diffs_in_range } else { r.diffs };
                                format!("{} · {} · {} 难度", r.artist, r.creator, n)
                            }),
                    ),
            )
            .child(
                div()
                    .id(gpui::ElementId::Name(format!("chev{}", r.set_id).into()))
                    .absolute()
                    .right(px(2.0))
                    .top_0()
                    .bottom_0()
                    .w(px(34.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .cursor_pointer()
                    .text_color(rgb(pal().dim))
                    .child(if expanded { "▾" } else { "▸" })
                    .on_click(move |_e: &gpui::ClickEvent, _w, cx| {
                        let _ = e4.update(cx, |this, cx| {
                            this.expanded = if this.expanded.as_deref() == Some(sid4.as_str()) {
                                None
                            } else {
                                Some(sid4.clone())
                            };
                            cx.notify();
                        });
                    }),
            )
            .into_any_element()
    }

    fn lib_chips_el(app: &App, entity: &gpui::Entity<App>, set_id: &str) -> gpui::AnyElement {
        let Some(set) = app
            .lib
            .as_ref()
            .and_then(|l| app.set_idx.get(set_id).and_then(|i| l.sets.get(*i)))
        else {
            return div().into_any_element();
        };
        let sid_base = set_id.to_string();
        let mut maps: Vec<&crate::lazer::LazerBeatmap> = set.beatmaps.iter().collect();
        maps.sort_by(|a, b| a.star_rating.total_cmp(&b.star_rating));
        div()
            .id(gpui::ElementId::Name(format!("chips{sid_base}").into()))
            .w_full()
            .flex()
            .flex_row()
            .flex_wrap()
            .justify_start()
            .gap_2()
            .pl(px(94.0))
            .pr(px(40.0))
            .py_2()
            .bg(rgb(pal().input))
            .children(maps.into_iter().map(|b| {
                let e1 = entity.clone();
                let e2 = entity.clone();
                let sid = sid_base.clone();
                let sid2 = sid_base.clone();
                let sha = b.sha2.clone();
                let sha2 = b.sha2.clone();
                div()
                    .id(gpui::ElementId::Name(format!("chip{}{}", sid_base, b.sha2).into()))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_full()
                    .bg(rgb(pal().ctl))
                    .cursor_pointer()
                    .hover(|st| st.bg(rgb(pal().ctl_hover)))
                    .child(star_badge(b.star_rating))
                    .child(div().text_sm().child(b.name.clone()))
                    .on_click(move |_e: &gpui::ClickEvent, _w, cx| {
                        let _ = e1.update(cx, |this, cx| {
                            this.expanded = None;
                            this.play(&sid, Some(&sha), this.settings.quick_mods);
                            cx.notify();
                        });
                    })
                    .on_mouse_down(gpui::MouseButton::Right, move |_e: &gpui::MouseDownEvent, _w, cx| {
                        let _ = e2.update(cx, |this, cx| {
                            this.mods_dialog = Some(ModsDialog {
                                set_id: sid2.clone(),
                                sha2: Some(sha2.clone()),
                                mods: this.settings.quick_mods,
                                queue_index: None,
                                bulk: None,
                            });
                            cx.notify();
                        });
                    })
            }))
            .into_any_element()
    }

    /// 当前(或上次播放)谱面集 id。
    fn current_set_id(&self) -> Option<String> {
        if let Some((id, _)) = &self.preview {
            return Some(id.clone());
        }
        self.queue.get(self.pos).map(|q| q.set_id.clone()).or_else(|| self.settings.lazer_set.clone())
    }

    /// 信息卡大封面(256px,按 set_id 缓存)。
    fn hero_image(&mut self, set_id: &str) -> Option<Arc<gpui::Image>> {
        if let Some(v) = self.hero.get(set_id) {
            return v.clone();
        }
        let img = self.cover_paths.get(set_id).and_then(|p| thumb_at(p, 256));
        // 曲库未就绪时不缓存空结果,否则信息卡永远「无封面」
        if img.is_some() {
            if self.hero.len() >= 16 {
                if let Some(old) = self.hero_order.pop_front() {
                    self.hero.remove(&old);
                }
            }
            self.hero.insert(set_id.to_string(), img.clone());
            self.hero_order.push_back(set_id.to_string());
        }
        img
    }

    fn info_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let set_id = self.current_set_id();
        let (title, artist, diff_name, diff_star, stats, tags, overridden) = if let Some(id) = &set_id {
            if let (Some(lib), Some(set)) = (&self.lib, self.lib.as_ref().and_then(|l| l.sets.iter().find(|s| s.id == *id))) {
                let _ = lib;
                // 优先用实际播放/预览的难度,其次最难
                let want = self
                    .preview
                    .as_ref()
                    .and_then(|(pid, sha)| (pid == id).then(|| sha.clone()).flatten())
                    .or_else(|| {
                        self.queue
                            .get(self.pos)
                            .filter(|t| &t.set_id == id)
                            .and_then(|t| t.sha2.clone())
                    })
                    .or_else(|| {
                        self.queue
                            .get(self.pos)
                            .filter(|t| &t.set_id == id)
                            .and_then(|_| self.policy_pick(id))
                    });
                let b = want
                    .and_then(|sha| set.beatmaps.iter().find(|b| b.sha2 == sha))
                    .or_else(|| {
                        set.beatmaps
                            .iter()
                            .max_by(|a, b| a.star_rating.total_cmp(&b.star_rating))
                    });
                (
                    pick_text(&set.title, &set.title_unicode, self.settings.display_unicode),
                    pick_text(&set.artist, &set.artist_unicode, self.settings.display_unicode),
                    b.map(|b| b.name.clone()).unwrap_or_default(),
                    b.map(|b| b.star_rating).unwrap_or(0.0),
                    vec![
                        ("谱师".to_string(), set.creator.clone()),
                        ("难度数".to_string(), set.beatmaps.len().to_string()),
                        ("时长".to_string(), format!("{:.0}s", b.map(|x| x.length_ms / 1000.0).unwrap_or(0.0))),
                        ("BPM".to_string(), format!("{:.0}", b.map(|x| x.bpm).unwrap_or(0.0))),
                        ("AR / OD".to_string(), format!("{:.1} / {:.1}", b.map(|x| x.ar).unwrap_or(0.0), b.map(|x| x.od).unwrap_or(0.0))),
                        ("CS / HP".to_string(), format!("{:.1} / {:.1}", b.map(|x| x.cs).unwrap_or(0.0), b.map(|x| x.hp).unwrap_or(0.0))),
                        ("谱面集 ID".to_string(), if set.online_id > 0 { set.online_id.to_string() } else { "—".to_string() }),
                        ("Beatmap ID".to_string(), b.map(|x| if x.online_id > 0 { x.online_id.to_string() } else { "—".to_string() }).unwrap_or_else(|| "—".to_string())),
                    ],
                    set.tags.clone(),
                    self.settings.set_overrides.contains_key(id),
                )
            } else {
                fallback_from_queue(self, &set_id)
            }
        } else {
            fallback_from_queue(self, &set_id)
        };
        let cover = set_id.as_deref().and_then(|id| self.hero_image(id));

        div()
            .id("info-card")
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .overflow_y_scroll()
            .bg(rgb(pal().panel))
            .border_b_1()
            .border_color(rgb(pal().line))
            .child(
                div()
                    .w_full()
                    .h(px(150.0))
                    .flex_none()
                    .overflow_hidden()
                    .rounded_md()
                    .bg(rgb(pal().line2))
                    .child(match cover {
                        Some(img) => gpui::img(gpui::ImageSource::Image(img))
                            .w_full()
                            .h_full()
                            .object_fit(gpui::ObjectFit::Cover)
                            .into_any_element(),
                        None => div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_xs()
                            .text_color(rgb(pal().dim))
                            .child("无封面")
                            .into_any_element(),
                    }),
            )
            .child({
                let c = if title.is_empty() { "未播放".to_string() } else { title.clone() };
                div()
                    .id("info-title")
                    .text_sm()
                    .cursor_pointer()
                    .child(c.clone())
                    .on_click(cx.listener(move |_t, _e, _w, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(c.clone()));
                    }))
            })
            .child({
                let c = artist.clone();
                div()
                    .id("info-artist")
                    .text_xs()
                    .overflow_hidden()
                    .cursor_pointer()
                    .text_color(rgb(pal().dim))
                    .child(c.clone())
                    .on_click(cx.listener(move |_t, _e, _w, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(c.clone()));
                    }))
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child({
                        let c = diff_name.clone();
                        div()
                            .id("info-diff")
                            .text_xs()
                            .overflow_hidden()
                            .cursor_pointer()
                            .text_color(rgb(pal().dim))
                            .child(c.clone())
                            .on_click(cx.listener(move |_t, _e, _w, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(c.clone()));
                            }))
                    })
                    .child(star_badge(diff_star))
                    .child(div().flex_1())
                    .child(self.small_btn("个性设置…", cx, |this, _| this.open_overrides())),
            )
            .children(stats.into_iter().map(|(k, v)| {
                let copy = format!("{}: {}", k, v);
                div()
                    .id(gpui::ElementId::Name(format!("info{k}").into()))
                    .flex()
                    .flex_row()
                    .justify_between()
                    .py_0p5()
                    .text_xs()
                    .cursor_pointer()
                    .child(div().text_color(rgb(pal().dim)).child(k))
                    .child(div().child(v))
                    .on_click(cx.listener(move |_t, _e, _w, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()));
                    }))
            }))
            .children(if tags.is_empty() {
                None
            } else {
                Some(
                    div()
                        .id("tags-box")
                        .max_h(px(110.0))
                        .overflow_y_scroll()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_1()
                        .children(
                            tags.split_whitespace()
                                .map(|tag| {
                                    let t = tag.to_string();
                                    let label = t.clone();
                                    div()
                                        .id(gpui::ElementId::Name(format!("tag{t}").into()))
                                        .px_1()
                                        .rounded_md()
                                        .bg(rgb(pal().ctl))
                                        .text_xs()
                                        .text_color(rgb(pal().dim))
                                        .cursor_pointer()
                                        .child(label)
                                        .on_click(cx.listener(move |_t, _e, _w, cx| {
                                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(t.clone()));
                                        }))
                                }),
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .pt_1()
                    .children(if overridden {
                        Some(div().text_xs().text_color(rgb(pal().warn)).child("已启用个性设置"))
                    } else {
                        None
                    }),
            )
    }

    /// 右侧信息栏:大封面 + 歌曲信息 + 谱面信息(AR/OD/BPM/时长/标签)。
    fn side_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(330.0))
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(pal().line))
            .child(self.info_card(cx))
    }

    fn queue_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let header = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .bg(rgb(pal().panel))
            .text_sm()
            .child({
                let total: f64 = self
                    .queue
                    .iter()
                    .map(|t| {
                        if t.length_ms > 0.0 {
                            t.length_ms
                        } else {
                            self.set_length_ms(&t.set_id, t.sha2.as_deref())
                        }
                    })
                    .sum();
                let secs = (total / 1000.0).round() as u64;
                format!(
                    "{} 首 · 约 {}:{:02}",
                    self.queue.len(),
                    secs / 60,
                    secs % 60
                )
            })
            .child(self.small_btn_w(if self.select_mode { "完成" } else { "多选" }, Some(88.0), cx, |this, _| {
                this.select_mode = !this.select_mode;
                this.selected.clear();
            }))
            .children(if self.select_mode {
                Some(self.small_btn("全选", cx, |this, _| {
                    if this.selected.len() == this.queue.len() {
                        this.selected.clear();
                    } else {
                        this.selected = (0..this.queue.len()).collect();
                    }
                }))
            } else {
                None
            })
            .child(self.small_btn_w("清空", Some(88.0), cx, |this, _| {
                this.clear_confirm = true;
            }));

        let entity = cx.entity();
        let count = self.queue.len();
        if self.queue_list.item_count() != count {
            self.queue_list.reset(count);
        }
        // 拖动预览:按「起点 Y → 目标行」换算插入位置,不受重排影响(无反馈抽动)
        let mut order: Vec<usize> = (0..count).collect();
        if let (Some(s0), Some(o)) = (self.drag_src, self.drag_over) {
            if s0 < count && o < count && s0 != o {
                let moved = order.remove(s0);
                order.insert(o, moved);
            }
        }
        let list = gpui::list(self.queue_list.clone(), move |ix, _w, cx| {
            let i = order.get(ix).copied().unwrap_or(ix);
            {
            let app = entity.read(cx);
            {
                {
                    let Some(t) = app.queue.get(i) else {
                        return div().into_any_element();
                    };
                    let set = app
                        .lib
                        .as_ref()
                        .and_then(|l| app.set_idx.get(&t.set_id).and_then(|k| l.sets.get(*k)));
                    let resolved = t
                        .sha2
                        .clone()
                        .or_else(|| app.policy_pick(&t.set_id));
                    let bm = set.and_then(|st| {
                        resolved
                            .as_ref()
                            .and_then(|sha| st.beatmaps.iter().find(|b| &b.sha2 == sha))
                    });
                    // 未锁定难度时用该谱面集最高星级,避免显示成 ★—
                    let star = bm.map(|b| b.star_rating).unwrap_or_else(|| {
                        set.map(|st| {
                            st.beatmaps
                                .iter()
                                .map(|b| b.star_rating)
                                .fold(0.0, f64::max)
                        })
                        .unwrap_or(0.0)
                    });
                    let diff_name = bm.map(|b| b.name.clone()).unwrap_or_default();
                    let dur_ms = bm
                        .map(|b| b.length_ms)
                        .filter(|v| *v > 0.0)
                        .unwrap_or(t.length_ms);
                    let cover = app.thumb_of(&t.set_id);
                    let selected = app.selected.contains(&i);
                    let playing = i == app.pos;
                    let previewing = app.preview.as_ref().is_some_and(|(id, _)| id == &t.set_id);
                    let e1 = entity.clone();
                    let e2 = entity.clone();
                    let e3 = entity.clone();
                    let e4 = entity.clone();
                    let e5 = entity.clone();
                    let e6 = entity.clone();
                    let mods = t.mods;
                    let sid_prev = t.set_id.clone();
                    let sha_prev = t.sha2.clone();
                    div()
                        .id(gpui::ElementId::Name(format!("q{i}").into()))
                        .relative()
                        .w_full()
                        .h(px(84.0))
                        .overflow_hidden()
                        .opacity(if app.dragging == Some(i) { 0.35 } else { 1.0 })
                        .bg(if playing {
                            rgb(pal().tint_play)
                        } else if previewing {
                            rgb(pal().tint_preview)
                        } else if selected {
                            rgb(pal().tint_preview)
                        } else {
                            rgb(pal().bg)
                        })
                        .hover(|st| st.bg(rgb(pal().row_hover)))
                        .border_b_1()
                        .border_color(rgb(pal().line2))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .on_mouse_down(gpui::MouseButton::Left, move |ev: &gpui::MouseDownEvent, _w, cx| {
                            let _ = e1.update(cx, |this, cx| {
                                if this.select_mode {
                                    if ev.modifiers.shift {
                                        // Shift 连选:从上一个锚点选到当前
                                        if let Some(a) = this.sel_anchor {
                                            let (lo, hi) = if a <= i { (a, i) } else { (i, a) };
                                            for k in lo..=hi {
                                                this.selected.insert(k);
                                            }
                                        } else {
                                            this.selected.insert(i);
                                            this.sel_anchor = Some(i);
                                        }
                                    } else {
                                        if !this.selected.remove(&i) {
                                            this.selected.insert(i);
                                        }
                                        this.sel_anchor = Some(i);
                                    }
                                } else if ev.click_count >= 2 {
                                    this.preview = None;
                                    this.play_index(i);
                                } else {
                                    this.preview_set(&sid_prev, sha_prev.clone());
                                }
                                cx.notify();
                            });
                        })
                        .on_mouse_down(gpui::MouseButton::Right, move |e: &gpui::MouseDownEvent, _w, cx| {
                            let x: f32 = e.position.x.into();
                            let y: f32 = e.position.y.into();
                            let _ = e2.update(cx, |this, cx| {
                                this.ctx_pos = (x, y);
                                if this.select_mode {
                                    // 多选:右键未选中的行先加入选中(资源管理器语义)
                                    if !this.selected.contains(&i) {
                                        this.selected.insert(i);
                                    }
                                    this.ctx_multi = true;
                                    this.ctx = None;
                                } else {
                                    this.ctx_multi = false;
                                    this.ctx = Some(i);
                                }
                                cx.notify();
                            });
                        })
                        .on_drop::<usize>(move |src: &usize, _w, cx| {
                            let src = *src;
                            let _ = e5.update(cx, |this, cx| {
                                // 落位用拖动计算出的目标(预览重排后行下标会变,不能用 i)
                                let target = this.drag_over.unwrap_or(i);
                                if this.settings.play_mode != crate::playlist::PlayMode::Random {
                                    this.move_to(src, target);
                                }
                                this.dragging = None;
                                this.drag_src = None;
                                this.drag_over = None;
                                cx.notify();
                            });
                        })
                        .child(
                            div()
                                .w(px(22.0))
                                .flex_none()
                                .text_sm()
                                .text_color(rgb(pal().accent))
                                .child(if selected { "☑" } else { "" }),
                        )
                        .child(
                            div()
                                .id(gpui::ElementId::Name(format!("drag{i}").into()))
                                .w(px(16.0))
                                .flex_none()
                                .text_sm()
                                .text_color(rgb(pal().dim))
                                .cursor_pointer()
                                .child(if app.settings.play_mode == crate::playlist::PlayMode::Random { " " } else { "⠿" })
                                .on_drag(i, {
                                    let entity = entity.clone();
                                    let title = t.title.clone();
                                    let artist = t.artist.clone();
                                    let cover = app.thumb_of(&t.set_id);
                                    move |idx, offset, _window, cx| {
                                        let idx = *idx;
                                        let oy: f32 = offset.y.into();
                                        let _ = entity.update(cx, |this, cx| {
                                            this.dragging = Some(idx);
                                            this.drag_src = Some(idx);
                                            this.drag_over = Some(idx);
                                            this.drag_start_y = oy;
                                            cx.notify();
                                        });
                                        cx.new(|_| DragPreview {
                                            title: title.clone(),
                                            artist: artist.clone(),
                                            cover: cover.clone(),
                                        })
                                    }
                                }),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(52.0))
                                .h(px(52.0))
                                .overflow_hidden()
                                .rounded_md()
                                .child(match cover {
                                    Some(img) => gpui::img(gpui::ImageSource::Image(img))
                                        .w_full()
                                        .h_full()
                                        .object_fit(gpui::ObjectFit::Cover)
                                        .rounded_md()
                                        .into_any_element(),
                                    None => div()
                                        .w_full()
                                        .h_full()
                                        .rounded_md()
                                        .bg(rgb(pal().line2))
                                        .into_any_element(),
                                }),
                        )
                        .child(
                            div()
                                .w(px(34.0))
                                .flex_none()
                                .text_sm()
                                .text_color(if playing { rgb(pal().accent) } else { rgb(pal().dim) })
                                .child(if playing { "▶".to_string() } else { format!("{}", i + 1) }),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .justify_center()
                                .flex_1()
                                .min_w(px(0.0))
                                .overflow_hidden()
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_base()
                                                .overflow_hidden()
                                                .min_w(px(0.0))
                                                .child(if t.title.is_empty() { "…".to_string() } else { t.title.clone() }),
                                        )
                                        .child(star_badge(star))
                                        .children(if mods != 0 {
                                            Some(
                                                div()
                                                    .px_1()
                                                    .rounded_md()
                                                    .bg(rgb(pal().mod_bg))
                                                    .text_xs()
                                                    .text_color(rgb(pal().mod_fg))
                                                    .child(mods_label(mods)),
                                            )
                                        } else {
                                            None
                                        }),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .overflow_hidden()
                                        .text_color(rgb(pal().dim))
                                        .child({
                                            let mut sub = if t.artist.is_empty() {
                                                "曲库加载中".to_string()
                                            } else {
                                                t.artist.clone()
                                            };
                                            if !diff_name.is_empty() {
                                                sub.push_str(" · ");
                                                sub.push_str(&diff_name);
                                            }
                                            sub
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .w(px(56.0))
                                .flex_none()
                                .text_xs()
                                .text_color(rgb(pal().dim))
                                .child(fmt_ms(dur_ms)),
                        )
                        .child(
                            div()
                                .id(gpui::ElementId::Name(format!("del{i}").into()))
                                .w(px(36.0))
                                .h(px(44.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_base()
                                .text_color(rgb(pal().dim))
                                .cursor_pointer()
                                .child("✕")
                                .on_click(move |_e: &gpui::ClickEvent, _w, cx| {
                                    let _ = e3.update(cx, |this, cx| {
                                        this.remove_at(i);
                                        cx.notify();
                                    });
                                }),
                        )
                        .into_any_element()
                }
            }
            }
        })
        .px_2()
        .w_full()
        .h_full()
        .min_h(px(0.0));
        let list_box = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .on_scroll_wheel(cx.listener(|_t, _e, _w, cx| cx.notify()))
            .on_drag_move::<usize>(cx.listener(|this, e: &gpui::DragMoveEvent<usize>, _w, cx| {
                let src = *e.drag(cx);
                let y: f32 = e.event.position.y.into();
                let n = this.queue.len() as isize;
                if n <= 0 {
                    return;
                }
                let steps = ((y - this.drag_start_y) / 84.0).round() as isize;
                let target = (src as isize + steps).clamp(0, n - 1) as usize;
                if this.drag_over != Some(target) {
                    this.drag_over = Some(target);
                    this.drag_src = Some(src);
                    cx.notify();
                }
            }))
            .child(list)
            .child(self.list_scrollbar(&self.queue_list, "queue", cx))
            .children(if self.queue.is_empty() {
                Some(
                    div()
                        .id("queue-empty")
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            this.view = View::Library;
                            cx.notify();
                        }))
                        .child(div().text_sm().text_color(rgb(pal().dim)).child("播放列表是空的"))
                        .child(self.small_btn("去曲库添加", cx, |this, _| this.view = View::Library)),
                )
            } else {
                None
            });

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(list_box)
    }

    fn settings_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // 设置内容变化(行数/高度变化)时让单条目列表重新测量,否则排版不更新
        if self.settings_rev_seen.get() != self.settings_rev {
            self.settings_rev_seen.set(self.settings_rev);
            self.settings_list.reset(1);
        }
        let s = &self.settings;
        let mut collections = vec![("".to_string(), "全部".to_string())];
        collections.extend(self.collections.iter().map(|c| (c.clone(), c.clone())));
        let mut monitors = vec![("".to_string(), "主屏".to_string())];
        monitors.extend(self.monitors.iter().map(|(l, d)| (d.clone(), l.clone())));
        let mut audio = vec![("".to_string(), "系统默认".to_string())];
        audio.extend(self.audio_devs.iter().map(|(id, name)| (id.clone(), name.clone())));
        let mut skins = vec![("".to_string(), "内置 Argon".to_string())];
        skins.extend(self.skins.iter().filter_map(|(label, v)| v.clone().map(|v| (v, label.clone()))));

        let mut items: Vec<gpui::AnyElement> = Vec::new();
        items.push((section("外观")).into_any_element());
        items.push(({
                let btn = |label: &'static str, value: &'static str, cur: &str, cx: &mut Context<Self>| {
                    let on = cur == value;
                    div()
                        .id(gpui::ElementId::Name(format!("th{value}").into()))
                        .px_4()
                        .py_2()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|st| st.bg(rgb(pal().ctl_hover)))
                        .bg(if on { rgb(pal().accent) } else { rgb(pal().ctl) })
                        .text_color(if on { rgb(0xffffff) } else { rgb(pal().text) })
                        .text_sm()
                        .child(label)
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.settings.theme_mode = value.to_string();
                            apply_palette(value != "light", &this.settings.accent);
                            this.persist();
                            cx.notify();
                        }))
                };
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .px_2()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(pal().line2))
                    .text_sm()
                    .child("主题")
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(btn("深色", "dark", &s.theme_mode, cx))
                            .child(btn("浅色", "light", &s.theme_mode, cx)),
                    )
            }).into_any_element());
        items.push(({
                let swatch = |name: &'static str, color: u32, cur: &str, cx: &mut Context<Self>| {
                    let on = cur == name;
                    div()
                        .id(gpui::ElementId::Name(format!("ac{name}").into()))
                        .w(px(28.0))
                        .h(px(28.0))
                        .rounded_full()
                        .cursor_pointer()
                        .bg(rgb(color))
                        .border_2()
                        .border_color(if on { rgb(pal().text) } else { rgb(pal().line) })
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.settings.accent = name.to_string();
                            apply_palette(this.settings.theme_mode != "light", &this.settings.accent);
                            this.persist();
                            cx.notify();
                        }))
                };
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .px_2()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(pal().line2))
                    .text_sm()
                    .child("主题色")
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(swatch("blue", 0x30507a, &s.accent, cx))
                            .child(swatch("purple", 0x5b3a86, &s.accent, cx))
                            .child(swatch("green", 0x2f6f52, &s.accent, cx))
                            .child(swatch("pink", 0x8a3a63, &s.accent, cx))
                            .child(swatch("orange", 0x8a5a2a, &s.accent, cx))

                    )
            }).into_any_element());
        items.push((section("画面")).into_any_element());
        items.push((self.dropdown_row(
                "渲染模式",
                DropKind::RenderMode,
                s.render_mode.clone(),
                vec![
                    ("always".into(), "始终渲染".into()),
                    ("autopause".into(), "全屏时不渲染".into()),
                    ("off".into(), "播放器模式".into()),
                ],
                cx,
            )).into_any_element());
        items.push((self.dropdown_row("显示器", DropKind::Monitor, s.monitor.clone().unwrap_or_default(), monitors, cx)).into_any_element());
        items.push((self.dropdown_row(
                "超分",
                DropKind::Upscale,
                s.upscale.clone(),
                vec![
                    ("off".into(), "关闭".into()),
                    ("fsr".into(), "FSR".into()),
                    ("anime4k-a".into(), "Anime4K A".into()),
                    ("anime4k-aa".into(), "Anime4K A+A".into()),
                    ("anime4k-b".into(), "Anime4K B".into()),
                    ("anime4k-bb".into(), "Anime4K B+B".into()),
                    ("anime4k-c".into(), "Anime4K C".into()),
                    ("anime4k-ca".into(), "Anime4K C+A".into()),
                ],
                cx,
            )).into_any_element());
        if let Some(x) = (if s.upscale.starts_with("anime4k") {
                Some(self.dropdown_row(
                "超分质量",
                DropKind::UpscaleQuality,
                s.upscale_quality.clone(),
                vec![
                    ("s".into(), "S".into()),
                    ("m".into(), "M".into()),
                    ("l".into(), "L".into()),
                    ("vl".into(), "VL".into()),
                    ("ul".into(), "UL".into()),
                ],
                cx,
                ))
            } else {
                None
            }) { items.push(x.into_any_element()); }
        items.push((self.toggle_row("storyboard/视频", s.storyboard, "storyboard", cx)).into_any_element());
        items.push((self.toggle_row("HD (Hidden)", s.hidden, "hidden", cx)).into_any_element());
        items.push((self.toggle_row("HUD", s.hud, "hud", cx)).into_any_element());
        items.push((self.toggle_row("PP 计数", s.pp, "pp", cx)).into_any_element());
        items.push((self.toggle_row("隐藏游玩画面", s.gameplay_hidden, "gameplay_hidden", cx)).into_any_element());
        items.push((self.toggle_row("休息段变亮", s.break_lighten, "break_lighten", cx)).into_any_element());
        items.push((self.toggle_row("减少打击动画", s.reduce_anim, "reduce_anim", cx)).into_any_element());
        items.push((self.step_row("背景亮度", format!("{:.0}%", s.bg_opacity * 100.0), "bg_opacity", cx)).into_any_element());
        items.push((self.toggle_row("滑条渐入", s.snaking_in, "snaking_in", cx)).into_any_element());
        items.push((self.toggle_row("滑条渐出", s.snaking_out, "snaking_out", cx)).into_any_element());
        items.push((section("音频")).into_any_element());
        items.push((self.toggle_row("打击音效", s.hitsound, "hitsound", cx)).into_any_element());
        items.push((self.toggle_row("谱面自带音效", s.beatmap_hitsounds, "beatmap_hitsounds", cx)).into_any_element());
        items.push((self.toggle_row("BGM 淡入淡出", s.fade_audio, "fade_audio", cx)).into_any_element());
        items.push((self.step_row("音效偏移", format!("{:.0} ms", s.audio_offset_ms), "offset", cx)).into_any_element());
        items.push((self.step_row("BGM 音量", format!("{:.0}%", s.volume * 100.0), "bgm", cx)).into_any_element());
        items.push((self.step_row("打击音效音量", format!("{:.0}%", s.hits_volume * 100.0), "hits", cx)).into_any_element());
        items.push((self.step_row("播放速度", format!("{:.2}x", s.speed), "speed", cx)).into_any_element());
        items.push((self.step_row("总音量", format!("{:.0}%", s.master_volume * 100.0), "volume", cx)).into_any_element());
        items.push((self.dropdown_row("音频输出", DropKind::AudioDevice, s.audio_device.clone().unwrap_or_default(), audio, cx)).into_any_element());
        items.push((section("播放")).into_any_element());
        items.push(({
                let laz_ok = crate::lazer::realm_path().is_some();
                let stb_ok = crate::stable::stable_root().is_some();
                let btn = |name: &'static str,
                           active: bool,
                           available: bool,
                           cx: &mut Context<Self>| {
                    let mut b = div()
                        .id(gpui::ElementId::Name(format!("src{name}").into()))
                        .px_4()
                        .py_2()
                        .rounded_md()
                        .text_sm()
                        .child(name);
                    if !available {
                        // 未识别到该曲库:置灰不可点
                        b = b
                            .bg(rgb(pal().line2))
                            .text_color(rgb(0x55555f))
                            .child(div().text_xs().text_color(rgb(0x44444c)).child("未检测到"));
                    } else {
                        b = b
                            .cursor_pointer()
                            .hover(|st| st.bg(rgb(pal().ctl_hover)))
                            .bg(if active { rgb(pal().accent) } else { rgb(pal().ctl) })
                            .text_color(if active { rgb(0xffffff) } else { rgb(pal().text) })
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                if this.settings.source != name {
                                    this.pick(DropKind::Source, name.to_string());
                                }
                                cx.notify();
                            }));
                    }
                    b
                };
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .px_2()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(pal().line2))
                    .text_sm()
                    .child("曲库源")
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(btn("lazer", s.source == "lazer", laz_ok, cx))
                            .child(btn("stable", s.source == "stable", stb_ok, cx)),
                    )
            }).into_any_element());
        items.push((self.dropdown_row(
                "帧率",
                DropKind::Fps,
                s.fps.to_string(),
                vec![
                    ("0".into(), "跟随刷新率".into()),
                    ("30".into(), "30 fps".into()),
                    ("60".into(), "60 fps".into()),
                    ("120".into(), "120 fps".into()),
                    ("240".into(), "240 fps".into()),
                    ("360".into(), "360 fps".into()),
                ],
                cx,
            )).into_any_element());
        items.push((self.dropdown_row(
                "难度",
                DropKind::Diff,
                match s.target_star {
                    None => "hardest".to_string(),
                    Some(t) if t < 0.0 => "easiest".to_string(),
                    Some(t) => format!("{t:.1}"),
                },
                vec![("hardest".into(), "最难".into()), ("easiest".into(), "最容易".into())],
                cx,
            )).into_any_element());
        items.push((self.step_row(
                "自定义星级",
                match s.target_star {
                    Some(t) if t > 0.0 => format!("★{t:.1}"),
                    _ => "未启用".into(),
                },
                "target_star",
                cx,
            )).into_any_element());
        items.push((div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .px_2()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(pal().line2))
                    .text_sm()
                    .child("默认快速加入选项")
                    .child(
                        div()
                            .id("quick-opts")
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(rgb(pal().ctl))
                            .cursor_pointer()
                            .hover(|st| st.bg(rgb(pal().ctl_hover)))
                            .child(format!(
                                "mods:{} · 难度:{}",
                                if s.quick_mods == 0 {
                                    "无".to_string()
                                } else {
                                    mods_label(s.quick_mods)
                                },
                                match s.quick_policy.as_str() {
                                    "easiest" => "最容易".to_string(),
                                    "custom" => format!(
                                        "最接近 ★{:.1}",
                                        s.quick_star.unwrap_or(5.0)
                                    ),
                                    _ => "最难".to_string(),
                                }
                            ))
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.quick_opts = true;
                                cx.notify();
                            })),
                    )).into_any_element());
        items.push((section("皮肤")).into_any_element());
        items.push((self.dropdown_row("皮肤", DropKind::Skin, s.skin.clone().unwrap_or_default(), skins, cx)).into_any_element());
        items.push((self.toggle_row("强制皮肤颜色", s.force_skin_colours, "force_skin_colours", cx)).into_any_element());
        items.push((self.toggle_row("光标", s.cursor, "cursor", cx)).into_any_element());
        items.push((self.toggle_row("光标轨迹", s.cursor_trail, "cursor_trail", cx)).into_any_element());
        items.push((self.toggle_row("光标波纹", s.cursor_ripples, "cursor_ripples", cx)).into_any_element());
        items.push((self.step_row("光标大小", format!("{:.2}", s.cursor_size), "cursor_size", cx)).into_any_element());
        items.push((section("系统")).into_any_element());
        items.push((self.toggle_row("启动后自动播放", s.play_on_start, "play_on_start", cx)).into_any_element());
        items.push((self.toggle_row("开机自启", s.autostart, "autostart", cx)).into_any_element());
        items.push((self.toggle_row("记录日志", s.log_enabled, "log", cx)).into_any_element());
        items.push((self.toggle_row("调试窗口(下次启动)", s.debug_window, "debug_window", cx)).into_any_element());
        items.push((self.dropdown_row(
                "关闭窗口",
                DropKind::CloseAction,
                s.close_action.clone().unwrap_or_default(),
                vec![
                    ("".into(), "询问".into()),
                    ("tray".into(), "最小化到托盘".into()),
                    ("exit".into(), "退出".into()),
                ],
                cx,
            )).into_any_element());
        items.push((section("路径")).into_any_element());
        items.push((self.path_row("osu!lazer 目录", Field::LazerDir, s.lazer_dir.clone(), cx)).into_any_element());
        items.push((self.path_row("osu!stable 目录", Field::StableDir, s.stable_dir.clone(), cx)).into_any_element());
        items.push((self.path_row("ffmpeg", Field::Ffmpeg, s.ffmpeg.clone(), cx)).into_any_element());
        items.push((self.path_row("ffprobe", Field::Ffprobe, s.ffprobe.clone(), cx)).into_any_element());
        items.push((div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .py_3()
                    .child(self.small_btn("导出日志", cx, |this, _| this.export_log()))
                    .child(self.small_btn("还原桌面壁纸", cx, |this, _| this.restore_wallpaper()))
                    .child(self.small_btn("重启壁纸进程", cx, |this, _| this.restart_child()))
                    .child(self.small_btn("当前曲目个性设置", cx, |this, _| this.open_overrides()))
                    .child(self.small_btn(
                        format!("清除全部个性设置 ({})", self.settings.set_overrides.len()),
                        cx,
                        |this, _| {
                            this.settings.set_overrides.clear();
                            this.persist();
                            this.toast("已清除全部个性设置");
                        },
                    ))
                    .child(self.small_btn("退出 Aria", cx, |this, _| {
                        #[cfg(target_os = "linux")]
                        let _ = crate::kde::restore_wallpaper();
                        this.persist();
                        std::process::exit(0);
                    }))).into_any_element());
        let count = items.len();
        if self.settings_list.item_count() != count || self.settings_rev_seen.get() != self.settings_rev {
            self.settings_rev_seen.set(self.settings_rev);
            self.settings_list.reset(count);
        }
        let items = std::rc::Rc::new(std::cell::RefCell::new(items.into_iter().map(Some).collect::<Vec<_>>()));
        let mine = items.clone();
        let list = gpui::list(self.settings_list.clone(), move |ix, _w, _cx| {
            mine.borrow_mut()
                .get_mut(ix)
                .and_then(|s| s.take())
                .unwrap_or_else(|| div().into_any_element())
        })
        .w_full()
        .h_full();
        div()
            .relative()
            .size_full()
            .child(list)
            .child(self.list_scrollbar(&self.settings_list, "settings", cx))
    }

    /// 单击行:切换右侧信息卡预览(不播放、不加入队列)。
    fn preview_set(&mut self, set_id: &str, sha2: Option<String>) {
        let same = self
            .preview
            .as_ref()
            .is_some_and(|(id, s)| id == set_id && s == &sha2);
        self.preview = if same { None } else { Some((set_id.to_string(), sha2)) };
    }

    fn open_overrides(&mut self) {
        let set_id = self
            .current_set_id()
            .or_else(|| self.queue.first().map(|t| t.set_id.clone()));
        let Some(set_id) = set_id else {
            self.toast("先在曲库点选一首曲目");
            return;
        };
        let ov = self.settings.set_overrides.get(&set_id).cloned().unwrap_or_default();
        self.overrides = Some(OverrideEdit { set_id, ov });
    }

    fn save_overrides(&mut self) {
        let Some(ed) = self.overrides.take() else { return };
        if ed.ov.enabled {
            self.settings.set_overrides.insert(ed.set_id.clone(), ed.ov);
        } else {
            self.settings.set_overrides.remove(&ed.set_id);
        }
        self.persist();
        // 正在播这首就重播,让新覆盖立即生效
        let playing = self.queue.get(self.pos).map(|t| t.set_id == ed.set_id).unwrap_or(false)
            || self.settings.lazer_set.as_deref() == Some(ed.set_id.as_str());
        if playing && !self.queue.is_empty() {
            let i = self.pos.min(self.queue.len() - 1);
            self.play_index(i);
        }
        self.toast(if self.settings.set_overrides.contains_key(&ed.set_id) {
            "已启用本谱面集个性设置"
        } else {
            "个性设置已保存"
        });
    }

    fn reset_overrides(&mut self) {
        let Some(ed) = self.overrides.take() else { return };
        self.settings.set_overrides.remove(&ed.set_id);
        self.persist();
        let playing = self.queue.get(self.pos).map(|t| t.set_id == ed.set_id).unwrap_or(false)
            || self.settings.lazer_set.as_deref() == Some(ed.set_id.as_str());
        if playing && !self.queue.is_empty() {
            let i = self.pos.min(self.queue.len() - 1);
            self.play_index(i);
        }
        self.toast("已恢复全局设置");
    }

    /// 原生文件/目录选择(XDG portal,后台线程,结果在 poll 里落账)。
    fn browse(&mut self, field: Field) {
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new();
            let picked = match field {
                Field::Ffmpeg | Field::Ffprobe => dialog.pick_file(),
                _ => dialog.pick_folder(),
            };
            if let Some(p) = picked {
                PICKED.lock().unwrap().push((field, p));
            }
        });
    }

    fn in_path(name: &str) -> bool {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
            .unwrap_or(false)
    }

    fn path_ok(field: Field, v: &Option<String>) -> bool {
        match field {
            Field::LazerDir | Field::StableDir => {
                v.as_ref().map(|p| Path::new(p).is_dir()).unwrap_or(true)
            }
            Field::Ffmpeg => v.as_deref().map(|p| Path::new(p).is_file()).unwrap_or_else(|| Self::in_path("ffmpeg")),
            Field::Ffprobe => v.as_deref().map(|p| Path::new(p).is_file()).unwrap_or_else(|| Self::in_path("ffprobe")),
            _ => true,
        }
    }

    fn export_log(&mut self) {
        let src = crate::logging::default_path();
        let Some(src) = src else { return };
        let dst = data_dir().join(format!(
            "aria-log-{}.txt",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ));
        match std::fs::copy(&src, &dst) {
            Ok(_) => self.toast(format!("日志已导出:{}", dst.display())),
            Err(e) => self.toast(format!("导出失败:{e}")),
        }
    }

    fn restore_wallpaper(&mut self) {
        #[cfg(target_os = "linux")]
        match crate::kde::restore_wallpaper() {
            Ok(()) => self.toast("已还原桌面壁纸"),
            Err(e) => self.toast(format!("还原失败:{e}")),
        }
    }

    fn restart_child(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.child.kill();
            let _ = c.child.wait();
        }
        self.child = spawn_wallpaper(self.status.clone());
        self.toast("壁纸进程已重启");
    }

    fn dropdown_row(
        &self,
        label: &'static str,
        kind: DropKind,
        current: String,
        options: Vec<(String, String)>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .justify_between()
            .items_center()
            .px_2()
            .py_2()
            .border_b_1()
            .border_color(rgb(pal().line2))
            .text_sm()
            .child(label)
            .child(self.dropdown(kind, current, options, cx))
    }

    fn cycle_row(&self, label: &'static str, value: String, kind: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .justify_between()
            .items_center()
            .px_2()
            .py_2()
            .border_b_1()
            .border_color(rgb(pal().line2))
            .text_sm()
            .child(label)
            .child(
                div()
                    .id(gpui::ElementId::Name(format!("cy{kind}").into()))
                    .px_2()
                    .rounded_md()
                    .bg(rgb(pal().ctl))
                    .child(value)
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.cycle(kind);
                        cx.notify();
                    })),
            )
    }

    fn step_row(&self, label: &'static str, value: String, kind: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .justify_between()
            .items_center()
            .px_2()
            .py_2()
            .border_b_1()
            .border_color(rgb(pal().line2))
            .text_sm()
            .child(label)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .id(gpui::ElementId::Name(format!("mn{kind}").into()))
                            .px_2()
                            .rounded_md()
                            .bg(rgb(pal().ctl))
                            .child("−")
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.bump(kind, -1.0);
                                cx.notify();
                            })),
                    )
                    .child(match Self::num_field(kind) {
                        Some(field) => self
                            .text_field(field, &value, 84.0, cx)
                            .into_any_element(),
                        None => div()
                            .w(px(84.0))
                            .text_xs()
                            .text_color(rgb(pal().dim))
                            .child(value.clone())
                            .into_any_element(),
                    })
                    .child(
                        div()
                            .id(gpui::ElementId::Name(format!("pl{kind}").into()))
                            .px_2()
                            .rounded_md()
                            .bg(rgb(pal().ctl))
                            .child("+")
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.bump(kind, 1.0);
                                cx.notify();
                            })),
                    ),
            )
    }

    fn toggle_row(&self, label: &'static str, on: bool, kind: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .justify_between()
            .items_center()
            .px_2()
            .py_2()
            .border_b_1()
            .border_color(rgb(pal().line2))
            .text_sm()
            .child(label)
            .child(
                div()
                    .id(gpui::ElementId::Name(format!("tg{kind}").into()))
                    .px_2()
                    .rounded_md()
                    .bg(if on { rgb(pal().accent) } else { rgb(pal().ctl) })
                    .child(if on { "开" } else { "关" })
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.toggle(kind);
                        cx.notify();
                    })),
            )
    }

    fn path_row(&self, label: &'static str, field: Field, value: Option<String>, cx: &mut Context<Self>) -> impl IntoElement {
        let ok = Self::path_ok(field, &value);
        // 未手动指定时显示自动检测到的路径
        let effective = value.clone().or_else(|| match field {
            Field::LazerDir => crate::lazer::data_root().map(|p| p.to_string_lossy().into_owned()),
            Field::StableDir => crate::stable::stable_root().map(|p| p.to_string_lossy().into_owned()),
            Field::Ffmpeg => Some("系统 PATH 中的 ffmpeg".to_string()),
            Field::Ffprobe => Some("系统 PATH 中的 ffprobe".to_string()),
            _ => None,
        });
        let is_default = value.is_none();
        let shown = effective.clone().unwrap_or_else(|| "未找到(点击浏览选择)".to_string());
        div()
            .flex()
            .flex_row()
            .justify_between()
            .items_center()
            .gap_2()
            .px_2()
            .py_2()
            .border_b_1()
            .border_color(rgb(pal().line2))
            .text_sm()
            .child(div().flex().flex_row().gap_1().items_center().child(label).child(
                div()
                    .text_xs()
                    .text_color(if ok { rgb(0x60c080) } else { rgb(0xe06060) })
                    .child(if ok { "✓" } else { "✗" }),
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .child(
                        div()
                            .text_xs()
                            .text_color(if ok { rgb(pal().text) } else { rgb(0xe06060) })
                            .child(shown),
                    )
                    .children(if is_default {
                        Some(div().text_xs().text_color(rgb(pal().dim)).child("自动检测(默认)"))
                    } else {
                        None
                    }),
            )
            .child(self.small_btn("浏览…", cx, move |this, _| this.browse(field)))
            .child(self.small_btn_w("恢复默认", Some(96.0), cx, move |this, _| {
                match field {
                    Field::LazerDir => this.settings.lazer_dir = None,
                    Field::StableDir => this.settings.stable_dir = None,
                    Field::Ffmpeg => this.settings.ffmpeg = None,
                    Field::Ffprobe => this.settings.ffprobe = None,
                    _ => {}
                }
                match field {
                    Field::LazerDir | Field::StableDir => this.spawn_library_load(),
                    _ => {
                        this.send(json!({"cmd": "set_ffmpeg_bins",
                            "ffmpeg": this.settings.ffmpeg,
                            "ffprobe": this.settings.ffprobe}));
                    }
                }
                this.persist();
            }))
    }

    /// 通用滑块:圆角轨道 + 高亮进度 + 圆形滑钮;拖动时按元素 bounds 换算比例。
    fn slider(
        &self,
        id: &'static str,
        frac: f32,
        width: Option<f32>,
        kind: SliderKind,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let f = frac.clamp(0.0, 1.0);
        // 进度条比音量条更高、更好点(轨道 8px,命中区 24px)
        let seek = kind == SliderKind::Seek;
        let hit_h = if seek { 24.0 } else { 18.0 };
        let bar_h = if seek { 8.0 } else { 4.0 };
        let bar_top = if seek { 8.0 } else { 7.0 };
        let thumb = if seek { 14.0 } else { 12.0 };
        let mut track = div().id(id).relative().h(px(hit_h)).flex().items_center();
        if let Some(w) = width {
            track = track.w(px(w));
        } else {
            track = track.flex_1();
        }
        let track = track
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(bar_top))
                    .h(px(bar_h))
                    .rounded_full()
                    .bg(rgb(pal().line)),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(bar_top))
                    .h(px(bar_h))
                    .w(gpui::relative(f))
                    .rounded_full()
                    .bg(rgb(pal().accent)),
            )
            .child(
                div()
                    .absolute()
                    .top(px((hit_h - thumb) / 2.0))
                    .left(gpui::relative(f))
                    .ml(px(-thumb / 2.0))
                    .size(px(thumb))
                    .rounded_full()
                    .bg(rgb(0xffffff))
                    .border_2()
                    .border_color(rgb(pal().accent)),
            );
        // 两种滑块用不同拖动载荷类型,避免同时响应
        match kind {
            SliderKind::Seek => track
                .on_drag(SeekDrag, |_, _, _, cx| cx.new(|_| DragGhost))
                .on_drag_move::<SeekDrag>(cx.listener(|this, e: &gpui::DragMoveEvent<SeekDrag>, _w, cx| {
                    let w: f32 = e.bounds.size.width.into();
                    if w <= 0.0 {
                        return;
                    }
                    let x: f32 = e.event.position.x.into();
                    let r = ((x - f32::from(e.bounds.origin.x)) / w).clamp(0.0, 1.0);
                    let dur = {
                        let st = this.status.lock().unwrap();
                        if st.duration > 0.0 {
                            st.duration
                        } else {
                            this.queue.get(this.pos).map(|q| q.length_ms / 1000.0).unwrap_or(0.0)
                        }
                    };
                    if dur > 0.0 {
                        let target = dur * r as f64;
                        // 本地先跳,别等下一次状态上报(否则点了没反应)
                        this.prog_ms = target;
                        this.prog_at = std::time::Instant::now();
                        this.seek_guard = std::time::Instant::now();
                        this.send(json!({"cmd": "seek", "ms": (target * 1000.0) as f32}));
                    }
                    cx.notify();
                })),
            SliderKind::StarMin => track
                .on_drag(StarMinDrag, |_, _, _, cx| cx.new(|_| DragGhost))
                .on_drag_move::<StarMinDrag>(cx.listener(|this, e: &gpui::DragMoveEvent<StarMinDrag>, _w, cx| {
                    let w: f32 = e.bounds.size.width.into();
                    if w <= 0.0 {
                        return;
                    }
                    let x: f32 = e.event.position.x.into();
                    let r = ((x - f32::from(e.bounds.origin.x)) / w).clamp(0.0, 1.0);
                    this.star_draft.0 = (r as f64 * 12.0 * 2.0).round() / 2.0;
                    if this.star_draft.1 > 0.0 && this.star_draft.0 > this.star_draft.1 {
                        this.star_draft.1 = this.star_draft.0;
                    }
                    cx.notify();
                })),
            SliderKind::StarMax => track
                .on_drag(StarMaxDrag, |_, _, _, cx| cx.new(|_| DragGhost))
                .on_drag_move::<StarMaxDrag>(cx.listener(|this, e: &gpui::DragMoveEvent<StarMaxDrag>, _w, cx| {
                    let w: f32 = e.bounds.size.width.into();
                    if w <= 0.0 {
                        return;
                    }
                    let x: f32 = e.event.position.x.into();
                    let r = ((x - f32::from(e.bounds.origin.x)) / w).clamp(0.0, 1.0);
                    let v = (r as f64 * 12.0 * 2.0).round() / 2.0;
                    this.star_draft.1 = if v >= 12.0 { 0.0 } else { v.max(this.star_draft.0) };
                    cx.notify();
                })),
            SliderKind::Volume => track
                .on_drag(VolumeDrag, |_, _, _, cx| cx.new(|_| DragGhost))
                .on_drag_move::<VolumeDrag>(cx.listener(|this, e: &gpui::DragMoveEvent<VolumeDrag>, _w, cx| {
                    let w: f32 = e.bounds.size.width.into();
                    if w <= 0.0 {
                        return;
                    }
                    let x: f32 = e.event.position.x.into();
                    let r = ((x - f32::from(e.bounds.origin.x)) / w).clamp(0.0, 1.0);
                    this.settings.master_volume = r;
                    this.send(json!({"cmd": "set_master", "v": r}));
                    this.persist();
                    cx.notify();
                })),
        }
    }

    fn play_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let (t, dur) = {
            let st = self.status.lock().unwrap();
            let dur = if st.duration > 0.0 {
                st.duration
            } else {
                self.queue.get(self.pos).map(|q| q.length_ms / 1000.0).unwrap_or(0.0)
            };
            drop(st);
            // 子进程每秒才报一次状态,这里用本地时钟插值,避免进度条一格一格跳
            let mut t = self.prog_ms;
            if self.playing {
                let elapsed = self.prog_at.elapsed().as_secs_f64();
                t += elapsed * f64::from(self.settings.speed).max(0.25);
                if dur > 0.0 {
                    t = t.min(dur);
                }
            }
            (t, dur)
        };
        let frac = if dur > 0.0 { (t / dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let now = self
            .queue
            .get(self.pos)
            .map(|q| format!("{} - {}", q.title, q.artist))
            .unwrap_or_else(|| "未播放".to_string());
        let star = self
            .queue
            .get(self.pos)
            .and_then(|t| {
                let sha = t.sha2.as_ref()?;
                self.lib
                    .as_ref()?
                    .sets
                    .iter()
                    .find(|s| s.id == t.set_id)?
                    .beatmaps
                    .iter()
                    .find(|b| &b.sha2 == sha)
                    .map(|b| b.star_rating)
            })
            .unwrap_or(0.0);

        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .py_2()
            .bg(rgb(pal().panel))
            .border_t_1()
            .border_color(rgb(pal().line))
            // 第一行:进度条(时间 + 滑块,整行拉伸)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(40.0))
                            .text_xs()
                            .text_color(rgb(pal().dim))
                            .child(format!("{}:{:02}", (t as u64) / 60, (t as u64) % 60)),
                    )
                    .child(self.slider("seek", frac, None, SliderKind::Seek, cx))
                    .child(
                        div()
                            .w(px(40.0))
                            .text_xs()
                            .text_color(rgb(pal().dim))
                            .child(format!("{}:{:02}", (dur as u64) / 60, (dur as u64) % 60)),
                    ),
            )
            // 第二行:播控 + 曲目 + 音量
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(self.small_btn("⏮", cx, |this, _| this.step(-1)))
                    .child(
                        div()
                            .id("toggle")
                            .px_3()
                            .py_1()
                            .rounded_full()
                            .bg(rgb(pal().accent))
                            .text_color(rgb(0xffffff))
                            .cursor_pointer()
                            .child(if self.playing { "⏸" } else { "▶" })
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.toggle_pause();
                                cx.notify();
                            })),
                    )
                    .child(self.small_btn("⏭", cx, |this, _| this.step(1)))

                    .child(self.small_btn("停止", cx, |this, _| this.unload()))
                    .child({
                        let (mut opts, cur) = match self.queue.get(self.pos) {
                            Some(t) => {
                                let mut opts: Vec<(String, String)> = Vec::new();
                                if let Some(set) = self
                                    .lib
                                    .as_ref()
                                    .and_then(|l| self.set_idx.get(&t.set_id).and_then(|i| l.sets.get(*i)))
                                {
                                    let mut bs: Vec<&crate::lazer::LazerBeatmap> =
                                        set.beatmaps.iter().collect();
                                    bs.sort_by(|a, b| a.star_rating.total_cmp(&b.star_rating));
                                    for b in bs {
                                        opts.push((
                                            b.sha2.clone(),
                                            format!("{} ★{:.2}", b.name, b.star_rating),
                                        ));
                                    }
                                }
                                let cur = t
                                    .sha2
                                    .clone()
                                    .or_else(|| self.policy_pick(&t.set_id))
                                    .unwrap_or_default();
                                (opts, cur)
                            }
                            None => (Vec::new(), String::new()),
                        };
                        opts.truncate(60);
                        self.dropdown(DropKind::QueueDiff, cur, opts, cx)
                    })
                    .child(self.dropdown_w(
                        DropKind::Speed,
                        // 用与选项一致的字符串,才能高亮当前档
                        match self.settings.speed {
                            v if (v - 0.5).abs() < 0.001 => "0.5".to_string(),
                            v if (v - 0.75).abs() < 0.001 => "0.75".to_string(),
                            v if (v - 1.0).abs() < 0.001 => "1".to_string(),
                            v if (v - 1.25).abs() < 0.001 => "1.25".to_string(),
                            v if (v - 1.5).abs() < 0.001 => "1.5".to_string(),
                            v if (v - 2.0).abs() < 0.001 => "2".to_string(),
                            v => format!("{}", v),
                        },
                        vec![
                            ("0.5".into(), "0.50x".into()),
                            ("0.75".into(), "0.75x".into()),
                            ("1".into(), "1.00x".into()),
                            ("1.25".into(), "1.25x".into()),
                            ("1.5".into(), "1.50x".into()),
                            ("2".into(), "2.00x".into()),
                        ],
                        Some(92.0),
                        cx,
                    ))
                    .child({
                        let cur = self.queue.get(self.pos).map(|t| t.set_id.clone());
                        let inner = match cur.and_then(|id| self.hero_image(&id)) {
                            Some(img) => gpui::img(gpui::ImageSource::Image(img))
                                .w_full()
                                .h_full()
                                .object_fit(gpui::ObjectFit::Cover)
                                .rounded_md()
                                .into_any_element(),
                            None => div()
                                .size_full()
                                .rounded_md()
                                .bg(rgb(pal().line2))
                                .into_any_element(),
                        };
                        div()
                            .w(px(34.0))
                            .h(px(34.0))
                            .flex_none()
                            .overflow_hidden()
                            .child(inner)
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .overflow_hidden()
                                            .min_w(px(0.0))
                                            .text_color(rgb(pal().text))
                                            .child(now),
                                    ),
                            ),
                    )
                    .child(div().text_xs().text_color(rgb(pal().dim)).child("🔈"))
                    .child(self.slider(
                        "volume",
                        self.settings.master_volume,
                        Some(110.0),
                        SliderKind::Volume,
                        cx,
                    ))
                    .child(
                        div()
                            .w(px(38.0))
                            .text_xs()
                            .text_color(rgb(pal().dim))
                            .child(format!("{:.0}%", self.settings.master_volume * 100.0)),
                    )
                    .child(self.dropdown_w(
                        DropKind::PlayMode,
                        match self.settings.play_mode {
                            crate::playlist::PlayMode::SingleLoop => "single".to_string(),
                            crate::playlist::PlayMode::ListOrder => "list".to_string(),
                            crate::playlist::PlayMode::Random => "random".to_string(),
                        },
                        vec![
                            ("list".into(), "顺序".into()),
                            ("single".into(), "单曲循环".into()),
                            ("random".into(), "随机".into()),
                        ],
                        Some(104.0),
                        cx,
                    ))

            )
    }

    fn mods_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(dlg) = &self.mods_dialog else { return div() };
        let title = self
            .all_rows
            .iter()
            .find(|r| r.set_id == dlg.set_id)
            .map(|r| r.title.clone())
            .unwrap_or_else(|| dlg.set_id.clone());
        let bits = dlg.mods;
        let chip = |label: &'static str, bit: u32, cx: &mut Context<Self>| {
            let on = bits & bit != 0;
            div()
                .id(gpui::ElementId::Name(format!("mp{label}").into()))
                .px_2()
                .py_1()
                .rounded_md()
                .text_xs()
                .cursor_pointer()
                .bg(if on { rgb(pal().accent) } else { rgb(pal().ctl) })
                .text_color(if on { rgb(0xffffff) } else { rgb(pal().text) })
                .child(label)
                .on_click(cx.listener(move |this, _e, _w, cx| {
                    if let Some(d) = this.mods_dialog.as_mut() {
                        // EZ↔HR、DT↔HT↔NC 互斥
                        d.mods ^= bit;
                    }
                    cx.notify();
                }))
        };
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000aa))
            .child(
                div()
                    .w(px(460.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(pal().panel))
                    .border_1()
                    .border_color(rgb(pal().line))
                    .child(div().text_lg().child(if dlg.queue_index.is_some() { "编辑条目(mods / 难度)" } else { "加入播放列表" }))
                    .child(div().text_sm().text_color(rgb(pal().dim)).child(title))
                    .children({
                        let beatmaps: Vec<(String, String, f64)> = self
                            .lib
                            .as_ref()
                            .and_then(|l| self.set_idx.get(&dlg.set_id).and_then(|i| l.sets.get(*i)))
                            .map(|set| {
                                let mut bs: Vec<&crate::lazer::LazerBeatmap> =
                                    set.beatmaps.iter().collect();
                                bs.sort_by(|a, b| a.star_rating.total_cmp(&b.star_rating));
                                bs.into_iter()
                                    .map(|b| (b.sha2.clone(), b.name.clone(), b.star_rating))
                                    .collect()
                            })
                            .unwrap_or_default();
                        if beatmaps.is_empty() {
                            None
                        } else {
                            let cur = dlg.sha2.clone();
                            Some(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(div().text_xs().text_color(rgb(pal().dim)).child("难度"))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .flex_wrap()
                                            .gap_1()
                                            .children(beatmaps.into_iter().map(|(sha, name, star)| {
                                                let on = cur.as_deref() == Some(sha.as_str());
                                                let s2 = sha.clone();
                                                div()
                                                    .id(gpui::ElementId::Name(
                                                        format!("mdiff{sha}").into(),
                                                    ))
                                                    .flex()
                                                    .flex_row()
                                                    .items_center()
                                                    .gap_1()
                                                    .px_2()
                                                    .py_1()
                                                    .rounded_full()
                                                    .cursor_pointer()
                                                    .hover(|st| st.bg(rgb(pal().ctl_hover)))
                                                    .bg(if on { rgb(pal().accent) } else { rgb(pal().ctl) })
                                                    .text_color(if on { rgb(0xffffff) } else { rgb(pal().text) })
                                                    .text_xs()
                                                    .child(format!("★{:.2}", star))
                                                    .child(name)
                                                    .on_click(cx.listener(move |this, _e, _w, cx| {
                                                        if let Some(d) = this.mods_dialog.as_mut() {
                                                            d.sha2 = Some(s2.clone());
                                                        }
                                                        cx.notify();
                                                    }))
                                            })),
                                    ),
                            )
                        }
                    })
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .children([
                                ("HD", 8u32),
                                ("HR", 16),
                                ("EZ", 2),
                                ("DT", 64),
                                ("HT", 256),
                                ("NC", 512),
                            ]
                            .into_iter()
                            .map(|(l, b)| chip(l, b, cx))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .child(self.small_btn("取消", cx, |this, _| this.mods_dialog = None))
                            .child(self.small_btn("确定", cx, |this, _| {
                                if let Some(d) = this.mods_dialog.take() {
                                    match d.queue_index {
                                        Some(i) => {
                                            if let Some(t) = this.queue.get_mut(i) {
                                                t.mods = d.mods;
                                                if let Some(sha) = &d.sha2 {
                                                    t.sha2 = Some(sha.clone());
                                                }
                                            }
                                            let playing = this.pos == i;
                                            this.save_queue();
                                            if playing {
                                                this.play_index(i);
                                            }
                                        }
                                        None => match d.bulk {
                                            Some(sel) => {
                                                for i in sel {
                                                    if let Some(t) = this.queue.get_mut(i) {
                                                        t.mods = d.mods;
                                                    }
                                                }
                                                this.save_queue();
                                            }
                                            None => {
                                                this.add_to_queue_with(&d.set_id, d.sha2, d.mods)
                                            }
                                        },
                                    }
                                }
                            })),
                    ),
            )
    }

    /// 星级区间弹窗(手动滑动 + 实时结果数)。
    fn star_popup_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (dmin, dmax) = self.star_draft;
        let max_frac = if dmax > 0.0 { (dmax / 12.0) as f32 } else { 1.0 };
        let min_frac = (dmin / 12.0) as f32;
        // 草稿范围下会剩多少谱面集(仅显示,不改列表)
        let matches = self
            .all_rows
            .iter()
            .filter(|r| r.star >= dmin && (dmax <= 0.0 || r.star <= dmax))
            .count();
        let row = |label: &'static str, is_min: bool, cx: &mut Context<Self>| {
            let kind = if is_min { SliderKind::StarMin } else { SliderKind::StarMax };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().w(px(54.0)).text_sm().child(label))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(self.slider(
                            if is_min { "star-min" } else { "star-max" },
                            if is_min { min_frac } else { max_frac },
                            None,
                            kind,
                            cx,
                        )),
                )
                .child(
                    div()
                        .w(px(56.0))
                        .text_sm()
                        .text_color(rgb(pal().dim))
                        .child(if is_min {
                            format!("{:.1}", dmin)
                        } else if dmax > 0.0 {
                            format!("{:.1}", dmax)
                        } else {
                            "∞".to_string()
                        }),
                )
        };
        div()
            .id("star-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000aa))
            .child(
                div()
                    .w(px(520.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(pal().panel))
                    .border_1()
                    .border_color(rgb(pal().line))
                    .child(div().text_lg().child("星级过滤"))
                    .child(row("最低星级", true, cx))
                    .child(row("最高星级", false, cx))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(pal().dim))
                            .child(format!(
                                "将匹配 {} 个谱面集(共 {} 个)",
                                matches,
                                self.all_rows.len()
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(self.small_btn("重置", cx, |this, _| {
                                this.star_draft = (0.0, 0.0);
                            }))
                            .child(self.small_btn("取消", cx, |this, _| {
                                this.star_popup = false;
                            }))
                            .child(self.small_btn("确定", cx, |this, _| {
                                this.star_min = this.star_draft.0;
                                this.star_max = this.star_draft.1;
                                this.star_popup = false;
                                this.apply_filter();
                            })),
                    ),
            )
    }

    /// 默认快速加入选项:mods 多选 + 难度策略。
    fn quick_opts_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mods = self.settings.quick_mods;
        let policy = self.settings.quick_policy.clone();
        let star = self.settings.quick_star.unwrap_or(5.0);
        let chip = |label: &'static str, bit: u32, cx: &mut Context<Self>| {
            let on = mods & bit != 0;
            div()
                .id(gpui::ElementId::Name(format!("qo{label}").into()))
                .px_3()
                .py_2()
                .rounded_md()
                .cursor_pointer()
                .hover(|st| st.bg(rgb(pal().ctl_hover)))
                .bg(if on { rgb(pal().accent) } else { rgb(pal().ctl) })
                .text_color(if on { rgb(0xffffff) } else { rgb(pal().text) })
                .text_sm()
                .child(label)
                .on_click(cx.listener(move |this, _e, _w, cx| {
                    this.settings.quick_mods ^= bit;
                    this.persist();
                    cx.notify();
                }))
        };
        let pbtn = |label: &'static str, value: &'static str, cx: &mut Context<Self>| {
            let on = policy == value;
            div()
                .id(gpui::ElementId::Name(format!("qp{value}").into()))
                .px_3()
                .py_2()
                .rounded_md()
                .cursor_pointer()
                .hover(|st| st.bg(rgb(pal().ctl_hover)))
                .bg(if on { rgb(pal().accent) } else { rgb(pal().ctl) })
                .text_color(if on { rgb(0xffffff) } else { rgb(pal().text) })
                .text_sm()
                .child(label)
                .on_click(cx.listener(move |this, _e, _w, cx| {
                    this.settings.quick_policy = value.to_string();
                    this.persist();
                    cx.notify();
                }))
        };
        div()
            .id("quick-opts-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000aa))
            .child(
                div()
                    .w(px(560.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(pal().panel))
                    .border_1()
                    .border_color(rgb(pal().line))
                    .child(div().text_lg().child("默认快速加入选项"))
                    .child(div().text_xs().text_color(rgb(pal().dim)).child("单击封面 / 全部加入时使用"))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xs().text_color(rgb(pal().dim)).child("mods(可多选)"))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .child(chip("HD", 8, cx))
                                    .child(chip("HR", 16, cx))
                                    .child(chip("EZ", 2, cx))
                                    .child(chip("DT", 64, cx))
                                    .child(chip("HT", 256, cx))
                                    .child(chip("NC", 512, cx)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xs().text_color(rgb(pal().dim)).child("默认难度"))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .child(pbtn("最难", "hardest", cx))
                                    .child(pbtn("最容易", "easiest", cx))
                                    .child(pbtn("最接近指定星级", "custom", cx))
                                    .child(
                                        div()
                                            .w(px(70.0))
                                            .text_sm()
                                            .text_color(if policy == "custom" {
                                                rgb(pal().text)
                                            } else {
                                                rgb(pal().dim)
                                            })
                                            .child(format!("★{:.1}", star)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(pal().dim))
                            .child("提示:星级在「自定义星级」设置项里调整"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .child(self.small_btn("关闭", cx, |this, _| this.quick_opts = false)),
                    ),
            )
    }

    /// 关闭窗口确认(close_action = 询问 时弹出);可勾选记住选择。
    fn close_ask_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let remember = self.close_remember;
        div()
            .id("close-ask")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000aa))
            .child(
                div()
                    .w(px(460.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(pal().panel))
                    .border_1()
                    .border_color(rgb(pal().line))
                    .child(div().text_lg().child("关闭 Aria?"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(pal().dim))
                            .child("最小化到托盘可继续在后台播放;退出会还原桌面壁纸。"),
                    )
                    .child(
                        div()
                            .id("close-remember")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .cursor_pointer()
                            .child(
                                div()
                                    .w(px(18.0))
                                    .h(px(18.0))
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(rgb(pal().line))
                                    .bg(if remember { rgb(pal().accent) } else { rgb(pal().ctl) })
                                    .child(if remember { "✓" } else { "" }),
                            )
                            .child(div().text_sm().child("记住我的选择(下次不再询问)"))
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.close_remember = !this.close_remember;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .child(self.small_btn("取消", cx, |this, _| this.close_ask = false))
                            .child(self.small_btn("最小化到托盘", cx, |this, cx| {
                                if this.close_remember {
                                    this.settings.close_action = Some("tray".to_string());
                                    sync_close_mode(&this.settings);
                                    this.persist();
                                }
                                this.close_ask = false;
                                this.minimize_window(cx);
                            }))
                            .child(self.small_btn("退出", cx, |this, _| {
                                if this.close_remember {
                                    this.settings.close_action = Some("exit".to_string());
                                    sync_close_mode(&this.settings);
                                }
                                #[cfg(target_os = "linux")]
                                let _ = crate::kde::restore_wallpaper();
                                this.persist();
                                std::process::exit(0);
                            })),
                    ),
            )
    }

    /// 关闭窗口确认(close_action = 询问 时弹出)。

    fn clear_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000aa))
            .child(
                div()
                    .w(px(380.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(pal().panel))
                    .border_1()
                    .border_color(rgb(pal().line))
                    .child(div().text_lg().child("清空播放列表?"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(pal().dim))
                            .child(format!("将移除全部 {} 个条目,不可撤销。", self.queue.len())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .child(self.small_btn("取消", cx, |this, _| this.clear_confirm = false))
                            .child(self.small_btn("清空", cx, |this, _| {
                                this.queue.clear();
                                this.selected.clear();
                                this.pos = 0;
                                this.save_queue();
                                this.clear_confirm = false;
                            })),
                    ),
            )
    }

    /// 队列右键菜单(单选/多选)。
    fn ctx_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (x, y) = self.ctx_pos;
        let n_sel = self.selected.len();
        let mut items: Vec<gpui::AnyElement> = Vec::new();
        if self.ctx_multi {
            items.push(self.ctx_item(format!("修改 mods…({} 首)", n_sel), "bulk_mods", cx).into_any_element());
            items.push(self.ctx_item("设置难度:最难".to_string(), "bulk_hardest", cx).into_any_element());
            items.push(self.ctx_item("设置难度:最容易".to_string(), "bulk_easiest", cx).into_any_element());
            items.push(self.ctx_item(format!("移除({} 首)", n_sel), "bulk_remove", cx).into_any_element());
        } else {
            items.push(self.ctx_item("修改…(mods / 难度)".to_string(), "mods", cx).into_any_element());
            items.push(self.ctx_item("个性设置…".to_string(), "override", cx).into_any_element());
            items.push(self.ctx_item("移除".to_string(), "remove", cx).into_any_element());
        }
        div()
            .id("ctx-layer")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _e, _w, cx| {
                this.ctx = None;
                this.ctx_multi = false;
                cx.notify();
            }))
            .child(
                div()
                    .id("ctx-menu")
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(210.0))
                    .flex()
                    .flex_col()
                    .rounded_md()
                    .bg(rgb(pal().panel))
                    .border_1()
                    .border_color(rgb(pal().line))
                    .py_1()
                    .children(items),
            )
    }

    fn ctx_item(&self, label: String, action: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let single = self.ctx.unwrap_or(0);
        let selected: Vec<usize> = self.selected.iter().copied().collect();
        div()
            .id(gpui::ElementId::Name(format!("ctx{action}").into()))
            .px_3()
            .py_2()
            .text_sm()
            .cursor_pointer()
            .hover(|st| st.bg(rgb(pal().ctl_hover)))
            .child(label)
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |this, _e, _w, cx| {
                cx.stop_propagation();
                this.ctx = None;
                this.ctx_multi = false;
                match action {
                    "mods" => {
                        if let Some(t) = this.queue.get(single).cloned() {
                            this.mods_dialog = Some(ModsDialog {
                                set_id: t.set_id,
                                sha2: t.sha2,
                                mods: t.mods,
                                queue_index: Some(single),
                                bulk: None,
                            });
                        }
                    }
                    "override" => {
                        if let Some(t) = this.queue.get(single).cloned() {
                            let ov = this
                                .settings
                                .set_overrides
                                .get(&t.set_id)
                                .cloned()
                                .unwrap_or_default();
                            this.preview = Some((t.set_id.clone(), t.sha2.clone()));
                            this.overrides = Some(OverrideEdit { set_id: t.set_id, ov });
                        }
                    }
                    "remove" => this.remove_at(single),
                    "bulk_mods" => {
                        if let Some(&first) = selected.first() {
                            if let Some(t) = this.queue.get(first).cloned() {
                                this.mods_dialog = Some(ModsDialog {
                                    set_id: t.set_id,
                                    sha2: t.sha2,
                                    mods: t.mods,
                                    queue_index: None,
                                    bulk: Some(selected.clone()),
                                });
                            }
                        }
                    }
                    "bulk_hardest" | "bulk_easiest" => {
                        let hardest = action == "bulk_hardest";
                        for i in &selected {
                            let set_id = match this.queue.get(*i) {
                                Some(t) => t.set_id.clone(),
                                None => continue,
                            };
                            if let Some(sha) = this.policy_pick_for(&set_id, hardest) {
                                if let Some(t) = this.queue.get_mut(*i) {
                                    t.sha2 = Some(sha);
                                }
                            }
                        }
                        this.save_queue();
                        if selected.contains(&this.pos) {
                            this.play_index(this.pos);
                        }
                    }
                    "bulk_remove" => {
                        let mut idx = selected.clone();
                        idx.sort_unstable_by(|a, b| b.cmp(a));
                        for i in idx {
                            this.remove_at(i);
                        }
                    }
                    _ => {}
                }
                cx.notify();
            }))
    }

    fn policy_pick_for(&self, set_id: &str, hardest: bool) -> Option<String> {
        let policy = if hardest {
            crate::playlist::PickPolicy::Hardest
        } else {
            crate::playlist::PickPolicy::Easiest
        };
        let set = self
            .lib
            .as_ref()
            .and_then(|l| self.set_idx.get(set_id).and_then(|i| l.sets.get(*i)))?;
        if set.beatmaps.is_empty() {
            return None;
        }
        Some(crate::playlist::pick_diff(set, policy).sha2.clone())
    }

    fn override_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(ed) = &self.overrides else { return div() };
        let row = |label: &'static str, value: String, kind: &'static str, cx: &mut Context<Self>| {
            div()
                .flex()
                .flex_row()
                .justify_between()
                .items_center()
                .py_1()
                .text_sm()
                .child(label)
                .child(
                    div()
                        .id(gpui::ElementId::Name(format!("ov{kind}").into()))
                        .px_2()
                        .rounded_md()
                        .bg(rgb(pal().ctl))
                        .child(value)
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            if let Some(ed) = this.overrides.as_mut() {
                                match kind {
                                    "enabled" => ed.ov.enabled = !ed.ov.enabled,
                                    "storyboard" => ed.ov.storyboard = tri_next(ed.ov.storyboard),
                                    "hitsound" => ed.ov.hitsound = tri_next(ed.ov.hitsound),
                                    "beatmap_hitsounds" => {
                                        ed.ov.beatmap_hitsounds = tri_next(ed.ov.beatmap_hitsounds)
                                    }
                                    "gameplay_hidden" => {
                                        ed.ov.gameplay_hidden = tri_next(ed.ov.gameplay_hidden)
                                    }
                                    "offset" => {
                                        let v = ed.ov.audio_offset_ms.unwrap_or(0.0) + 5.0;
                                        ed.ov.audio_offset_ms = Some(v.clamp(-200.0, 200.0));
                                    }
                                    "bg" => {
                                        let v = ed.ov.bg_opacity.unwrap_or(0.7) + 0.05;
                                        ed.ov.bg_opacity = Some(v.clamp(0.0, 1.0));
                                    }
                                    _ => {}
                                }
                            }
                            cx.notify();
                        })),
                )
        };
        let tri = |v: Option<bool>| match v {
            None => "跟随全局".to_string(),
            Some(true) => "开".to_string(),
            Some(false) => "关".to_string(),
        };
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000aa))
            .child(
                div()
                    .w(px(460.0))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(pal().panel))
                    .border_1()
                    .border_color(rgb(pal().line))
                    .child(div().text_lg().child("当前曲目个性设置"))
                    .child(div().text_xs().text_color(rgb(pal().dim)).child(ed.set_id.clone()))
                    .child(row("启用", if ed.ov.enabled { "开".into() } else { "关".into() }, "enabled", cx))
                    .child(row("storyboard/视频", tri(ed.ov.storyboard), "storyboard", cx))
                    .child(row("打击音效", tri(ed.ov.hitsound), "hitsound", cx))
                    .child(row("谱面自带音效", tri(ed.ov.beatmap_hitsounds), "beatmap_hitsounds", cx))
                    .child(row("隐藏游玩画面", tri(ed.ov.gameplay_hidden), "gameplay_hidden", cx))
                    .child(row("音效偏移 +5ms", format!("{:.0} ms", ed.ov.audio_offset_ms.unwrap_or(0.0)), "offset", cx))
                    .child(row("背景亮度 +5%", format!("{:.0}%", ed.ov.bg_opacity.unwrap_or(0.7) * 100.0), "bg", cx))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .pt_2()
                            .child(self.small_btn("保存", cx, |this, _| this.save_overrides()))
                            .child(self.small_btn("取消", cx, |this, _| this.overrides = None)),
                    ),
            )
    }

    fn about_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let oss: &[(&str, &str, &str)] = &[
            ("osu-replay-render", "https://github.com/Ohdmire/osu-replay-render", "osu! 场景渲染: 游玩画面 + storyboard + 背景处理"),
            ("osu-parse", "https://github.com/Ohdmire/osu-parse", ".osu 谱面解析(hit objects / timing)"),
            ("osu-storyboard-render", "https://github.com/Ohdmire/osu-storyboard-render", "storyboard 指令处理与素材回调"),
            ("osu-replay-judge", "https://github.com/Ohdmire/osu-replay-judge", "提供判定支持"),
            ("realm-db-reader", "https://github.com/Ohdmire/realm-db-reader", "osu!lazer client.realm 只读解析"),
            ("osu-db", "https://github.com/negamartin/osu-db", "osu!stable 曲库 / 收藏夹 / 星级读取"),
            ("GPUI", "https://github.com/zed-industries/zed", "桌面 UI 框架(Zed)"),
            ("wgpu", "https://github.com/gfx-rs/wgpu", "GPU 渲染"),
            ("kira", "https://github.com/tesselode/kira", "音频播放"),
            ("Symphonia", "https://github.com/pdeljanov/Symphonia", "音频解码"),
            ("SoundTouch", "https://codeberg.org/soundtouch/soundtouch", "BGM 变速"),
        ];
        let thanks: &[(&str, &str, &str)] = &[
            ("xlfish233", "https://osu.ppy.sh/users/34424018", "Token Provider"),
            ("Citrusis", "https://osu.ppy.sh/users/30298378", "Logo 设计"),
            ("七海灯子", "", "许愿一个 cinema mod"),
        ];
        let item = |name: &'static str, url: &'static str, desc: &'static str, cx: &mut Context<Self>| {
            div()
                .id(gpui::ElementId::Name(format!("ab{name}").into()))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .hover(|st| st.bg(rgb(pal().ctl_hover)))
                .child(div().w(px(190.0)).flex_none().text_sm().text_color(rgb(0x6aa0e0)).child(name))
                .child(div().text_xs().text_color(rgb(pal().dim)).child(desc))
                .on_click(cx.listener(move |_t, _e, _w, cx| {
                    if !url.is_empty() {
                        let _ = std::process::Command::new("xdg-open")
                            .arg(url)
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn();
                    }
                    cx.notify();
                }))
        };
        div()
            .id("about-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000aa))
            .child(
                div()
                    .id("about-card")
                    .w(px(620.0))
                    .max_h(px(660.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(pal().panel))
                    .border_1()
                    .border_color(rgb(pal().line))
                    .child(div().text_xl().child("Aria"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(pal().dim))
                            .child(format!("版本 {}", env!("CARGO_PKG_VERSION"))),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(pal().dim))
                            .child("osu! storyboard 桌面动态壁纸 · by ATRI1024"),
                    )
                    .child(
                        div()
                            .id("about-repo")
                            .text_xs()
                            .text_color(rgb(0x6aa0e0))
                            .cursor_pointer()
                            .child("https://github.com/Ohdmire/Aria")
                            .on_click(cx.listener(|_t, _e, _w, cx| {
                                let _ = std::process::Command::new("xdg-open")
                                    .arg("https://github.com/Ohdmire/Aria")
                                    .stdout(std::process::Stdio::null())
                                    .stderr(std::process::Stdio::null())
                                    .spawn();
                                cx.notify();
                            })),
                    )
                    .child(scroll_section("使用的开源库", oss.iter().map(|(n, u, d)| item(n, u, d, cx).into_any_element()).collect(), "oss"))
                    .child(scroll_section("感谢名单", thanks.iter().map(|(n, u, d)| item(n, u, d, cx).into_any_element()).collect(), "thanks"))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .pt_1()
                            .border_t_1()
                            .border_color(rgb(pal().line))
                            .child(div().text_xs().text_color(rgb(pal().dim)).child(
                                "osu! 是 Ppy Pty Ltd 的游戏;本项目与 ppy 无关 · MIT License",
                            ))
                            .child(
                                div()
                                    .id("sponsor-link")
                                    .text_xs()
                                    .text_color(rgb(0x6aa0e0))
                                    .cursor_pointer()
                                    .child("赞助作者(爱发电)  https://afdian.com/a/ATRI1024")
                                    .on_click(cx.listener(|_t, _e, _w, cx| {
                                        let _ = std::process::Command::new("xdg-open")
                                            .arg("https://afdian.com/a/ATRI1024")
                                            .stdout(std::process::Stdio::null())
                                            .stderr(std::process::Stdio::null())
                                            .spawn();
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .child(self.small_btn("关闭", cx, |this, _| {
                                this.about = false;
                            })),
                    ),
            )
    }
}

/// 关于页的滚动分组(标题 + 条目列表)。
fn scroll_section(title: &'static str, items: Vec<gpui::AnyElement>, id: &'static str) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().text_color(rgb(0x6a6a75)).child(title))
        .child(
            div()
                .id(gpui::ElementId::Name(format!("about{id}").into()))
                .max_h(px(210.0))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .children(items),
        )
}

fn section(title: &'static str) -> impl IntoElement {
    div().pt_3().pb_1().text_xs().text_color(rgb(0x6a6a75)).child(title)
}

/// 单实例:第一个实例监听 socket,后续实例通知它显示窗口后自行退出。
#[cfg(target_os = "linux")]
fn single_instance() -> bool {
    use std::io::Write;
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let path = dir.join("aria-ui.sock");
    let spawn_accept = |listener: std::os::unix::net::UnixListener| {
        std::thread::spawn(move || {
            for mut conn in listener.incoming().flatten() {
                let mut buf = [0u8; 16];
                let n = std::io::Read::read(&mut conn, &mut buf).unwrap_or(0);
                let msg = String::from_utf8_lossy(&buf[..n]).to_string();
                if msg.contains("replace") {
                    REPLACE_REQUESTED.store(true, std::sync::atomic::Ordering::Relaxed);
                } else {
                    SHOW_REQUESTED.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });
    };
    match std::os::unix::net::UnixListener::bind(&path) {
        Ok(listener) => {
            spawn_accept(listener);
            true
        }
        Err(_) => {
            // 已有实例?能连上 = 活着,通知它显示窗口后退出。
            if let Ok(mut s) = std::os::unix::net::UnixStream::connect(&path) {
                // 已有实例:请求它退出(开发时 cargo run 直接热替换),
                // 等 socket 释放后自己接管。
                let _ = s.write_all(b"replace\n");
                drop(s);
                for _ in 0..60 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    if let Ok(listener) = std::os::unix::net::UnixListener::bind(&path) {
                        spawn_accept(listener);
                        return true;
                    }
                }
                return false;
            }
            // 连不上 = 旧实例崩溃留下的死 socket,清掉重绑。
            let _ = std::fs::remove_file(&path);
            match std::os::unix::net::UnixListener::bind(&path) {
                Ok(listener) => {
                    spawn_accept(listener);
                    true
                }
                Err(_) => false,
            }
        }
    }
}

pub fn run() {
    #[cfg(target_os = "linux")]
    if !single_instance() {
        return;
    }
    Application::new().run(|cx: &mut gpui::App| {
        let options = WindowOptions {
            // 用最大化:窗口尺寸随屏幕,不会再出现窗口比屏幕大/跑出屏幕
            window_bounds: Some(gpui::WindowBounds::Maximized(gpui::Bounds {
                origin: gpui::point(px(40.), px(40.)),
                size: gpui::size(px(1100.), px(640.)),
            })),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Aria".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        crate::tray::spawn();
        // KDE 壁纸插件:装包 + 切到 Aria(退出/关窗时还原)。
        #[cfg(target_os = "linux")]
        {
            if let Err(e) = crate::kde::install_plugin() {
                log::warn!("[kde] 安装壁纸插件失败: {e}");
            }
            if !crate::kde::plugin_installed() {
                log::warn!("[kde] 壁纸插件未安装,壁纸不会显示(libariaplugin.so 缺失?)");
            } else if let Err(e) = crate::kde::enable_wallpaper() {
                log::warn!("[kde] 切换壁纸失败: {e}");
            }
        }
        // 被 kill(SIGTERM/SIGINT)时也要还原壁纸,否则桌面停在最后一帧。
        #[cfg(target_os = "linux")]
        {
            let signals = signal_hook::iterator::Signals::new([
                signal_hook::consts::SIGTERM,
                signal_hook::consts::SIGINT,
            ])
            .ok();
            if let Some(mut signals) = signals {
                std::thread::spawn(move || {
                    for _ in signals.forever() {
                        let _ = crate::kde::restore_wallpaper();
                        std::process::exit(0);
                    }
                });
            }
        }
        cx.on_window_closed(|cx| {
            #[cfg(target_os = "linux")]
            let _ = crate::kde::restore_wallpaper();
            cx.quit();
        })
        .detach();
        let mut view_slot: Option<gpui::Entity<App>> = None;
        if let Ok(handle) = cx.open_window(options, |window, cx| {
            window.on_window_should_close(cx, |window, _cx| {
                if CLOSE_TO_TRAY.load(std::sync::atomic::Ordering::Relaxed) {
                    window.minimize_window();
                    false
                } else if CLOSE_TO_EXIT.load(std::sync::atomic::Ordering::Relaxed) {
                    true
                } else {
                    // 询问:交给主线程弹确认框
                    CLOSE_ASK.store(true, std::sync::atomic::Ordering::Relaxed);
                    false
                }
            });
            let view = cx.new(App::new);
            view_slot = Some(view.clone());
            view
        }) {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
            *WINDOW.lock().unwrap() = Some(handle);
        }
        // 心跳:实体建好后再 spawn(构造期 spawn 的 WeakEntity 升不上来,
        // 任务会静默退出 —— 曲库后台结果不应用、进度不刷新就是这么来的)。
        if let Some(view) = view_slot {
            cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                if view
                    .update(cx, |this, cx| {
                        if this.poll(cx) {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
                }
            })
            .detach();
        }
    });
}
