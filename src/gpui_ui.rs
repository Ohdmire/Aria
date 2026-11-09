//! Aria 的 GPUI 前端(替代原 Tauri/WebView 设置窗口)。
//!
//! 结构:
//! - 曲库页:搜索 / 星级下限 / 排序 / 收藏夹 / SB 筛选 / 刷新 / 源切换,
//!   列表带封面,点击播放,右键加入播放列表;
//! - 设置页:全部会话参数(开关/步进/枚举/路径文本),改动即持久化并下发;
//! - 右侧播放列表:点击切歌、多选、右键菜单(难度/上移下移/移除);
//! - 底部播放条:上一首/暂停/下一首、±10s、进度、时长、音量;
//! - 托盘:见 `tray` 模块(独立线程 + 平台消息循环)。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::StatefulInteractiveElement;
// 自绘控件(滚动条)的过渡动画
use gpui::AnimationExt;
use serde_json::json;
use gpui::{
    div, px, rgb, rgba, Application, Context, FocusHandle, KeyDownEvent, MouseButton, Render,
    Window, WindowOptions,
};

/// 应用数据目录(Windows `%APPDATA%\com.ohdmire.aria`,Linux XDG,
/// 与旧 Tauri app_data_dir、[`crate::logging`] 同口径)。
#[cfg(windows)]
pub fn data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("com.ohdmire.aria")
}

/// 应用数据目录(Linux XDG,与旧 Tauri app_data_dir 同口径)。
#[cfg(not(windows))]
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

/// 主窗口的 Win32 句柄:给原生文件对话框当 owner。没有 owner 时 Windows 上
/// 对话框可能弹在主窗口后面,看起来像"点了没反应"。
#[cfg(windows)]
static MAIN_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// 把一个裸 HWND 包成 `HasWindowHandle`/`HasDisplayHandle`,交给 rfd 当 owner。
#[cfg(windows)]
struct RawParent(std::num::NonZeroIsize);

#[cfg(windows)]
impl raw_window_handle::HasWindowHandle for RawParent {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let h = raw_window_handle::Win32WindowHandle::new(self.0);
        Ok(unsafe {
            raw_window_handle::WindowHandle::borrow_raw(raw_window_handle::RawWindowHandle::Win32(h))
        })
    }
}

#[cfg(windows)]
impl raw_window_handle::HasDisplayHandle for RawParent {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(unsafe {
            raw_window_handle::DisplayHandle::borrow_raw(raw_window_handle::RawDisplayHandle::Windows(
                raw_window_handle::WindowsDisplayHandle::new(),
            ))
        })
    }
}

/// 关闭主窗口的行为(由 settings.close_action 同步)。
static CLOSE_TO_MIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CLOSE_TO_TRAY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CLOSE_TO_EXIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);


static PICKED: Mutex<Vec<(Field, PathBuf)>> = Mutex::new(Vec::new());
static SHOW_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static REPLACE_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 策略:询问 / 最小化 / 退出。
fn sync_close_mode(s: &crate::settings::Settings) {
    CLOSE_TO_MIN.store(s.close_action.as_deref() == Some("min"), std::sync::atomic::Ordering::Relaxed);
    CLOSE_TO_TRAY.store(s.close_action.as_deref() == Some("tray"), std::sync::atomic::Ordering::Relaxed);
    CLOSE_TO_EXIT.store(s.close_action.as_deref() == Some("exit"), std::sync::atomic::Ordering::Relaxed);
}

/// 关闭按钮。询问直接改界面状态,不丢给轮询。
/// 窗口最小化/隐藏,或刚从那种状态回来时,不弹询问。
fn on_should_close(window: &mut gpui::Window, cx: &mut gpui::App, view: &gpui::Entity<App>) -> bool {
    if CLOSE_TO_TRAY.load(std::sync::atomic::Ordering::Relaxed) {
        #[cfg(windows)]
        {
            crate::win::block_ask();
            crate::win::hide_window(MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed));
        }
        #[cfg(not(windows))]
        window.minimize_window();
        let _ = view.update(cx, |this, cx| {
            this.close_ask = false;
            this.release_ui_caches();
            cx.notify();
        });
        return false;
    }
    if CLOSE_TO_MIN.load(std::sync::atomic::Ordering::Relaxed) {
        #[cfg(windows)]
        crate::win::block_ask();
        let _ = view.update(cx, |this, cx| {
            this.close_ask = false;
            cx.notify();
        });
        window.minimize_window();
        return false;
    }
    if CLOSE_TO_EXIT.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    // 只有关闭按钮按下(或 Alt+F4)才弹。显示窗口时补来的 WM_CLOSE 没有这个状态。
    #[cfg(windows)]
    let open_ask = crate::win::take_close_intent()
        && crate::win::main_window_shown(MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed));
    #[cfg(not(windows))]
    let open_ask = true;
    if !open_ask {
        #[cfg(windows)]
        crate::win::block_ask();
        let _ = view.update(cx, |this, cx| {
            this.close_ask = false;
            cx.notify();
        });
        return false;
    }
    #[cfg(windows)]
    crate::win::set_ask_open(true);
    let _ = view.update(cx, |this, cx| {
        this.close_ask = true;
        this.close_remember = false;
        cx.notify();
    });
    false
}

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const GOLD: u32 = 0xe0c060;

/// 播放列表行高:虚拟列表测量、拖动换算、滚动定位统一用这个值。
const QUEUE_ROW_H: f32 = 84.0;

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
    /// 危险操作(退出/清空/恢复默认)的按钮底色与悬停色。
    danger: u32,
    danger_hover: u32,
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
    danger: 0xa83232,
    danger_hover: 0xc04040,
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
            danger: 0xa83232,
            danger_hover: 0xc04040,
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
            danger: 0xa83232,
            danger_hover: 0xc04040,
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
    /// 全局快捷键的三条组合键文本(Windows `RegisterHotKey`;其它平台只保存不注册)。
    HotkeyPrev,
    HotkeyToggle,
    HotkeyNext,
    NumVolume,
    NumBgm,
    NumHits,
    NumSpeed,
    NumStar,
    NumOffset,
    NumBg,
    NumCursor,
    NumStarMin,
    /// 批量导入对话框里的自定义星级(只改对话框草稿,不动全局设置)。
    NumBatchStar,
    /// 默认快速加入选项里的"最接近指定星级"数值(与全局自定义星级共用)。
    NumQuickStar,
    /// 曲目个性设置里的数值项(草稿,保存后才写入)。
    OvOffset,
    OvBg,
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
    /// 显示用标题/艺术家(按「以 Unicode 显示」开关由下面四个字段挑选,过滤时重算)
    title: String,
    artist: String,
    /// 原名与 unicode 名都留着:搜索两个都匹配,切换显示也不用重读曲库
    title_raw: String,
    title_u: String,
    artist_raw: String,
    artist_u: String,
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
    /// 两套名字都存(romaji / unicode),显示时按「以 Unicode 显示」现挑
    title: String,
    title_u: String,
    artist: String,
    artist_u: String,
    length_ms: f64,
    mods: u32,
}

#[derive(Default)]
struct PlayStatus {
    t: f64,
    playing: bool,
    duration: f64,
    ended: bool,
    /// 子进程每次 Load 完成(loaded 事件)自增,用来判断新曲是否真的就绪。
    load_seq: u64,
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
    // 曲目个性设置里的三态项(跟随全局/开/关)
    OvStoryboard,
    OvHitsound,
    OvBeatmapHitsounds,
    OvGameplayHidden,
}

struct LibBarDrag;
struct ListBarDrag;
struct StarMinDrag;
struct StarMaxDrag;
struct SeekDrag;
struct VolumeDrag;
/// 设置页滑杆的拖动载荷;kind 用于区分是哪一条滑杆(所有滑杆共用
/// 一个载荷类型,监听器里用 kind 过滤,避免同时改多个值)。
struct SettingDrag(&'static str);
/// 通用 ScrollHandle 滚动条(按 id 区分同一帧里的多条)。
struct ScrollBarDrag(&'static str);

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
        // 与行同高:拖动把手撑满整行后,ghost 的锚点就是行左上角,
        // 起点时刚好盖住原行,移动时与行网格保持对齐(不再纵向偏移)。
        div()
            .w(px(360.0))
            .h(px(QUEUE_ROW_H))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .rounded_lg()
            .bg(gpui::rgba(0x2a2f3aff))
            .border_1()
            .border_color(rgb(pal().accent))
            .opacity(0.9)
            .child(
                div()
                    .w(px(48.0))
                    .h(px(48.0))
                    .flex_none()
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

/// xorshift64*:洗牌用,不需要密码学质量。
fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Fisher-Yates 洗出 `0..n` 的排列;`avoid` 避免首项是它(轮末重洗时
/// 不要紧接着重播上一轮最后一首)。
fn shuffled_indices(rng: &mut u64, n: usize, avoid: Option<usize>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        let j = (xorshift(rng) as usize) % (i + 1);
        order.swap(i, j);
    }
    if n > 1 {
        if let Some(a) = avoid {
            if order[0] == a {
                order.swap(0, 1);
            }
        }
    }
    order
}

/// 拖动落点换算:指针窗口 Y → 队列行号(0 起,夹到 [0, count-1])。
/// `list_top` 是播放列表可视区顶部(窗口坐标),`scrolled` 是内容已向下
/// 滚动的像素。行高固定,所以先减去列表顶、加回滚动量,再按行高取整。
fn drag_slot(pointer_y: f32, list_top: f32, scrolled: f32, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let slot = ((pointer_y - list_top + scrolled) / QUEUE_ROW_H).floor();
    slot.clamp(0.0, (count - 1) as f32) as usize
}

/// mods 位切换:同组互斥(HR↔EZ;DT/HT/NC 三选一),再点同一位取消。
/// 位值与 osu! legacy 一致:HD=8, HR=16, EZ=2, DT=64, HT=256, NC=512。
fn toggle_mod(bits: u32, bit: u32) -> u32 {
    if bits & bit != 0 {
        return bits & !bit;
    }
    let exclusive: u32 = match bit {
        16 => 2,         // HR 关 EZ
        2 => 16,         // EZ 关 HR
        64 => 256 | 512, // DT 关 HT/NC
        256 => 64 | 512, // HT 关 DT/NC
        512 => 64 | 256, // NC 关 DT/HT
        _ => 0,
    };
    (bits & !exclusive) | bit
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

/// 「全部加入」批量导入时统一使用的难度策略(每个谱面集各自挑一档)。
#[derive(Clone, Copy, PartialEq)]
enum BatchDiff {
    Hardest,
    Easiest,
    Custom(f64),
}

struct ModsDialog {
    set_id: String,
    sha2: Option<String>,
    mods: u32,
    /// Some(i) = 编辑已有队列条目;None = 新加入。
    queue_index: Option<usize>,
    /// 多选批量改 mods 的目标下标。
    bulk: Option<Vec<usize>>,
    /// Some(n) = 「全部加入」的批量确认框(当前过滤命中 n 个谱面集)。
    batch: Option<usize>,
    /// 批量导入的难度策略(非批量对话框忽略)。
    batch_diff: BatchDiff,
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
    /// 随机模式:队列下标的一次性洗牌排列(整轮播完前不重复)。
    rand_order: Vec<usize>,
    /// 当前曲目在 `rand_order` 中的位置。
    rand_pos: usize,
    /// xorshift 随机状态(壁纸不需要密码学质量)。
    rand_state: u64,
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
    /// 清空全部个性设置的确认框。
    ov_clear_confirm: bool,
    /// 恢复默认设置的确认框。
    reset_confirm: bool,
    /// 曲库中展开难度的谱面集。
    expanded: Option<String>,
    /// 正在拖动的队列下标(源)。
    dragging: Option<usize>,
    /// 拖动让位预览的目标下标(落点)。
    drag_over: Option<usize>,
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
    /// 刚下发 Load、还没收到新曲目状态:此间忽略旧曲目的进度回报。
    awaiting_start: bool,
    /// play() 下发 Load 时记下的 load_seq(子进程 loaded 事件后变化)。
    load_seq_seen: u64,
    close_remember: bool,

    menu_list: gpui::ListState,
    menu_scroll: gpui::ScrollHandle,
    queue_list: gpui::ListState,
    settings_list: gpui::ListState,
    settings_scroll: gpui::ScrollHandle,
    /// 关于页「感谢名单」的滚动位置(限高滚动区)。
    about_thanks_scroll: gpui::ScrollHandle,
    /// 曲库还没加载完时的待播请求。
    pending_play: Option<(String, Option<String>, u32)>,
    path_cache: HashMap<String, PathEntry>,
    status_line: String,
    monitors: Vec<(String, String)>,
    audio_devs: Vec<(String, String)>,
    skins: Vec<(String, Option<String>)>,
    /// 左上角 Aria logo(与旧 Web UI 的 ui/icon.png 为同一张 128×128 图)。
    logo: Arc<gpui::Image>,
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
    /// 展开的谱面集在 `rows` 里的下标(虚拟列表用它把 ix 映射成"行/难度块"),
    /// 只在 rows 或 expanded 变化时更新,不逐帧算。
    expanded_row: Option<usize>,
    /// 当前鼠标悬停在哪条自绘滚动条上(同一时刻只可能有一条),驱动宽度/配色动画。
    sb_hover: std::cell::Cell<Option<&'static str>>,
    /// 拖动中指针滑出滚动条时先"粘住"高亮,松手后再清。
    sb_sticky: std::cell::Cell<Option<&'static str>>,
    /// 自动检测到的路径(注册表 / storage.ini / PATH),只在变化时重新采集。
    probe_lazer_root: Option<std::path::PathBuf>,
    probe_stable_root: Option<std::path::PathBuf>,
    probe_realm: Option<std::path::PathBuf>,
    probe_ffmpeg: Option<std::path::PathBuf>,
    probe_ffprobe: Option<std::path::PathBuf>,
}

/// 播放条上的传输图标:纯路径绘制,不依赖字体字形,也不是 emoji。
#[derive(Clone, Copy, PartialEq, Eq)]
enum TransportIcon {
    Prev,
    Play,
    Pause,
    Next,
}

/// 在给定方框里画出图标(16×16 栅格,按实际 bounds 缩放)。
fn transport_icon_path(
    kind: TransportIcon,
    b: gpui::Bounds<gpui::Pixels>,
) -> Option<gpui::Path<gpui::Pixels>> {
    let s = f32::from(b.size.width) / 16.0;
    let p = |x: f32, y: f32| {
        gpui::point(b.origin.x + gpui::px(x * s), b.origin.y + gpui::px(y * s))
    };
    let mut pb = gpui::PathBuilder::fill();
    match kind {
        TransportIcon::Play => {
            pb.add_polygon(&[p(4.5, 2.0), p(13.0, 8.0), p(4.5, 14.0)], true);
        }
        TransportIcon::Pause => {
            pb.add_polygon(&[p(4.0, 2.0), p(6.5, 2.0), p(6.5, 14.0), p(4.0, 14.0)], true);
            pb.add_polygon(&[p(9.5, 2.0), p(12.0, 2.0), p(12.0, 14.0), p(9.5, 14.0)], true);
        }
        TransportIcon::Prev => {
            // 竖条 + 左向三角
            pb.add_polygon(&[p(2.5, 2.0), p(4.5, 2.0), p(4.5, 14.0), p(2.5, 14.0)], true);
            pb.add_polygon(&[p(13.5, 2.0), p(5.5, 8.0), p(13.5, 14.0)], true);
        }
        TransportIcon::Next => {
            // 右向三角 + 竖条
            pb.add_polygon(&[p(2.5, 2.0), p(10.5, 8.0), p(2.5, 14.0)], true);
            pb.add_polygon(&[p(11.5, 2.0), p(13.5, 2.0), p(13.5, 14.0), p(11.5, 14.0)], true);
        }
    }
    pb.build().ok()
}

/// 把一次按键事件翻译成快捷键文本(`Ctrl+Alt+Right` / `F8` / `MediaNext`)。
///
/// 只按修饰键(Ctrl / Alt / Shift / Win)时返回 `None` —— 捕获模式要等主键。
/// 键名统一走 [`crate::hotkeys`] 解析器认识的写法,主键缺失时用字符本身兜底。
fn hotkey_chord_text(ks: &gpui::Keystroke) -> Option<String> {
    let key = ks.key.as_str();
    // 修饰键本身不构成组合
    if matches!(
        key,
        "control" | "ctrl" | "alt" | "shift" | "platform" | "function" | "fn" | "win" | "super" | "meta" | "cmd"
    ) {
        return None;
    }
    let named = match key {
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        "space" => "Space",
        "enter" | "return" => "Enter",
        "tab" => "Tab",
        "escape" | "esc" => "Escape",
        "backspace" => "Backspace",
        "delete" | "del" => "Delete",
        "insert" | "ins" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "pause" => "Pause",
        "printscreen" => "PrintScreen",
        "capslock" => "CapsLock",
        "numlock" => "NumLock",
        "scrolllock" => "ScrollLock",
        "mediaplaypause" | "playpause" => "MediaPlayPause",
        "medianext" | "nexttrack" => "MediaNext",
        "mediaprev" | "prevtrack" => "MediaPrev",
        "mediastop" | "stop" => "MediaStop",
        "minus" => "Minus",
        "equal" => "Plus",
        "comma" => "Comma",
        "period" => "Period",
        "slash" => "Slash",
        "backslash" => "Backslash",
        "semicolon" => "Semicolon",
        "quote" => "Quote",
        "backtick" | "backquote" => "Backquote",
        "leftbracket" => "BracketLeft",
        "rightbracket" => "BracketRight",
        // 字母 / 数字 / f1–f24 / numpad:键名本身就够解析器识别
        _ => {
            if let Some(c) = ks.key_char.as_deref().filter(|c| c.chars().count() == 1) {
                // 符号键用字符本身(解析器认 - = , . / \ ; ' ` [ ])
                return finish_chord(ks, &c.to_uppercase());
            }
            return finish_chord(ks, &key.to_uppercase());
        }
    };
    finish_chord(ks, named)
}

/// 拼上修饰键前缀。
fn finish_chord(ks: &gpui::Keystroke, base: &str) -> Option<String> {
    let mut text = String::new();
    if ks.modifiers.control {
        text.push_str("Ctrl+");
    }
    if ks.modifiers.alt {
        text.push_str("Alt+");
    }
    if ks.modifiers.shift {
        text.push_str("Shift+");
    }
    if ks.modifiers.platform {
        text.push_str("Win+");
    }
    text.push_str(base);
    Some(text)
}

/// 滚动偏移(gpui 里是**负值**:顶部 0、底部 -max)→ 滑块位置比例 0..1。
///
/// 单独拎出来是因为这个符号很容易搞反:直接拿 `offset` 当正值算,比例恒为 0,
/// 滑块就永远贴在顶端("滚动条不滚动"的元凶)。
fn scroll_frac(offset_y: f32, max: f32) -> f32 {
    if max <= 0.0 {
        0.0
    } else {
        ((-offset_y) / max).clamp(0.0, 1.0)
    }
}

/// 两个 `0xRRGGBB` 之间线性插值(自绘控件的过渡配色用)。
fn lerp_rgb(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |shift: u32| {
        let x = ((a >> shift) & 0xff) as f32;
        let y = ((b >> shift) & 0xff) as f32;
        ((x + (y - x) * t).round() as u32 & 0xff) << shift
    };
    ch(16) | ch(8) | ch(0)
}

impl App {
    fn new(cx: &mut Context<Self>) -> Self {
        let dir = data_dir();
        let s = crate::settings::load_from(&dir);
        apply_palette(s.theme_mode != "light", &s.accent);
        // 随机播放种子:时间纳秒 ^ 进程 id,避免每次启动洗牌序列相同
        let rand_seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            ^ (std::process::id() as u64).rotate_left(32);
        let sb_only = s.sb_only;
        let mut focus = HashMap::new();
        for f in [
            Field::Search,
            Field::LazerDir,
            Field::StableDir,
            Field::Ffmpeg,
            Field::Ffprobe,
            Field::Skin,
            // 快捷键的三行也要有焦点句柄,否则点进去收不到按键(录不进去)
            Field::HotkeyPrev,
            Field::HotkeyToggle,
            Field::HotkeyNext,
            Field::NumVolume,
            Field::NumBgm,
            Field::NumHits,
            Field::NumSpeed,
            Field::NumStar,
            Field::NumOffset,
            Field::NumBg,
            Field::NumCursor,
            Field::NumStarMin,
            Field::NumBatchStar,
            Field::NumQuickStar,
            Field::OvOffset,
            Field::OvBg,
        ] {
            focus.insert(f, cx.focus_handle());
        }

        let mut app = Self {
            view: View::Queue,
            star_min: s.star_min,
            star_max: s.star_max.unwrap_or(0.0),
            settings: s,
            lib: None,
            all_rows: Vec::new(),
            rows: Vec::new(),
            cover_paths: HashMap::new(),
            collections: Vec::new(),
            search: String::new(),
            sort: SortKey::Added,
            desc: true,
            sb_only,
            queue: Vec::new(),
            pos: 0,
            rand_order: Vec::new(),
            rand_pos: 0,
            rand_state: rand_seed | 1,
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
            ov_clear_confirm: false,
            reset_confirm: false,
            expanded: None,
            dragging: None,
            drag_over: None,
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
            awaiting_start: false,
            load_seq_seen: 0,
            close_remember: false,

            menu_list: gpui::ListState::new(0, gpui::ListAlignment::Top, px(100.0)),
            menu_scroll: gpui::ScrollHandle::new(),
            queue_list: gpui::ListState::new(0, gpui::ListAlignment::Top, px(300.0)),
            settings_list: gpui::ListState::new(1, gpui::ListAlignment::Top, px(200.0)),
            settings_scroll: gpui::ScrollHandle::new(),
            about_thanks_scroll: gpui::ScrollHandle::new(),
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
            logo: Arc::new(gpui::Image::from_bytes(
                gpui::ImageFormat::Png,
                include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/128x128.png")).to_vec(),
            )),
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
            expanded_row: None,
            sb_hover: std::cell::Cell::new(None),
            sb_sticky: std::cell::Cell::new(None),
            probe_lazer_root: None,
            probe_stable_root: None,
            probe_realm: None,
            probe_ffmpeg: None,
            probe_ffprobe: None,
        };
        app.refresh_probes();
        sync_close_mode(&app.settings);
        // 全局快捷键:按已保存的组合注册(默认关闭 = 空表;幂等)
        crate::hotkeys::start(&app.settings);
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
                title_u: t.title_u.clone(),
                artist: t.artist.clone(),
                artist_u: t.artist_u.clone(),
                length_ms: t.length_ms,
                mods: t.mods,
            })
            .collect();
        self.pos = self.settings.playlist_pos.min(self.queue.len().saturating_sub(1));
    }

    /// 后台解析曲库(不阻塞 UI):结果在 poll 里应用。
    fn spawn_library_load(&mut self) {
        // 曲库源/目录可能刚变过:顺手重新采集自动检测路径(不逐帧做)
        self.refresh_probes();
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
                            title_raw: set.title.clone(),
                            title_u: set.title_unicode.clone(),
                            artist_raw: set.artist.clone(),
                            artist_u: set.artist_unicode.clone(),
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
                // 搜索:原名和 unicode 名**都**参与匹配(不是只搜当前显示的那个)
                if !q.is_empty()
                    && !r.title_raw.to_lowercase().contains(&q)
                    && !r.title_u.to_lowercase().contains(&q)
                    && !r.artist_raw.to_lowercase().contains(&q)
                    && !r.artist_u.to_lowercase().contains(&q)
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
        // 显示名按「以 Unicode 显示」开关现挑:切换开关只重算这一步,不用重读曲库
        let unicode = self.settings.display_unicode;
        for r in rows.iter_mut() {
            r.title = pick_text(&r.title_raw, &r.title_u, unicode);
            r.artist = pick_text(&r.artist_raw, &r.artist_u, unicode);
        }
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
        // 行集合变了:同步"展开的谱面集在第几行"(虚拟列表用它做算术映射)
        self.expanded_row = self
            .expanded
            .as_ref()
            .and_then(|id| self.rows.iter().position(|r| &r.set_id == id));
    }

    // ---- 播放 ----

    fn spawn_if_needed(&mut self) {
        let dead = self.child.as_mut().map(|c| !c.alive()).unwrap_or(true);
        if dead {
            self.child = spawn_wallpaper(self.status.clone());
            // 新进程的默认渲染模式是 AutoPause,必须把保存的设置补上
            if self.child.is_some() {
                self.send_child_settings();
            }
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
        // 开始播放 = 退出预览:右侧信息卡要跟着新歌走,不能停在之前预览的那首
        self.preview = None;
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
            // 新曲从 0 开始:先把进度锚点归零,避免切歌瞬间还显示上一首的
            // 位置(子进程每秒才报一次状态)。
            self.prog_ms = 0.0;
            self.prog_at = std::time::Instant::now();
            self.seek_guard = std::time::Instant::now();
            // 新曲状态到达前,忽略旧曲每秒回报的 t_ms,否则进度条会
            // 先跳回上一首的位置再归零。用子进程的 loaded 事件作为
            // "真的就绪"信号:Load 处理中它会先报 0,不能据此放行。
            self.awaiting_start = true;
            if let Ok(mut st) = self.status.lock() {
                self.load_seq_seen = st.load_seq;
                st.t = 0.0;
            }
            self.current_title(set_id, Some(&chosen));
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

    /// 记住"实际在放的那首"(谱面集 + 难度),播放条据此显示,不受曲库预览影响。
    fn current_title(&mut self, set_id: &str, sha2: Option<&str>) {
        if self.all_rows.iter().any(|r| r.set_id == set_id) {
            self.settings.lazer_set = Some(set_id.to_string());
            if let Some(s) = sha2.filter(|s| !s.is_empty()) {
                // 顺手记下难度名:曲库还没加载完的那几秒,播放条不至于把 sha2 当名字
                let name = self
                    .lib
                    .as_ref()
                    .and_then(|l| self.set_idx.get(set_id).and_then(|i| l.sets.get(*i)))
                    .and_then(|set| set.beatmaps.iter().find(|b| b.sha2 == s))
                    .map(|b| b.name.clone());
                self.settings.lazer_sha2 = Some(s.to_string());
                if name.is_some() {
                    self.settings.lazer_diff = name;
                }
            }
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
    /// 按指定策略从谱面集里挑一档难度(canonical: 全局策略见 `policy_pick`)。
    fn pick_with_policy(&self, set_id: &str, policy: crate::playlist::PickPolicy) -> Option<String> {
        let set = self
            .lib
            .as_ref()
            .and_then(|l| self.set_idx.get(set_id).and_then(|i| l.sets.get(*i)))?;
        if set.beatmaps.is_empty() {
            return None;
        }
        Some(crate::playlist::pick_diff(set, policy).sha2.clone())
    }

    /// 批量/多选弹窗的初始难度策略:沿用全局难度设置。
    fn default_batch_diff(&self) -> BatchDiff {
        match self.settings.target_star {
            None => BatchDiff::Hardest,
            Some(t) if t < 0.0 => BatchDiff::Easiest,
            Some(t) => BatchDiff::Custom(t.clamp(0.5, 12.0)),
        }
    }

    fn policy_pick(&self, set_id: &str) -> Option<String> {
        let policy = match self.settings.target_star {
            None => crate::playlist::PickPolicy::Hardest,
            Some(t) if t < 0.0 => crate::playlist::PickPolicy::Easiest,
            Some(t) => crate::playlist::PickPolicy::Custom(t),
        };
        self.pick_with_policy(set_id, policy)
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
        if self.settings.play_mode == crate::playlist::PlayMode::Random {
            self.step_random(delta);
            return;
        }
        let n = self.queue.len() as isize;
        let next = (self.pos as isize + delta).rem_euclid(n) as usize;
        self.play_index(next);
    }

    /// 用当前 RNG 状态洗出 `0..n` 的排列;`avoid` 避免首项是它 ——
    /// 一轮播完重洗时不要紧接着重播上一轮最后一首。
    fn shuffled_indices(&mut self, n: usize, avoid: Option<usize>) -> Vec<usize> {
        shuffled_indices(&mut self.rand_state, n, avoid)
    }

    /// 随机模式的前进/后退:沿一次性洗好的排列走,整轮播完前不重复;
    /// 走完一轮重洗(首项避开刚播完的那首)。队列增删或手动点播后会按
    /// 当前曲目重新定位游标,不会因为换了歌就从头开始。
    fn step_random(&mut self, delta: isize) {
        let n = self.queue.len();
        if n == 0 {
            return;
        }
        if self.pos >= n {
            self.pos = n - 1;
        }
        // 队列长度变了 = 旧排列失效,重洗
        if self.rand_order.len() != n {
            self.rand_order = self.shuffled_indices(n, None);
            self.rand_pos = 0;
        }
        // 游标跟着当前曲目(可能刚被手动点播/删除)
        self.rand_pos = self
            .rand_order
            .iter()
            .position(|&x| x == self.pos)
            .unwrap_or(0);
        if delta >= 0 {
            self.rand_pos += 1;
            if self.rand_pos >= n {
                self.rand_order = self.shuffled_indices(n, Some(self.pos));
                self.rand_pos = 0;
            }
        } else {
            self.rand_pos = if self.rand_pos == 0 { n - 1 } else { self.rand_pos - 1 };
        }
        let idx = self.rand_order[self.rand_pos];
        self.play_index(idx);
    }

    /// 按比例(0..=1)跳转:先把本地进度锚点挪到目标(点击/拖动立即生效,
    /// 不等子进程下一次状态上报),再把 seek 发给壁纸进程。返回是否真的
    /// 发出了跳转(时长未知时为 false)。
    fn seek_to_frac(&mut self, frac: f32) -> bool {
        let dur = {
            let st = self.status.lock().unwrap();
            if st.duration > 0.0 {
                st.duration
            } else {
                self.queue.get(self.pos).map(|q| q.length_ms / 1000.0).unwrap_or(0.0)
            }
        };
        if dur <= 0.0 {
            return false;
        }
        let target = dur * f64::from(frac.clamp(0.0, 1.0));
        self.prog_ms = target;
        self.prog_at = std::time::Instant::now();
        self.seek_guard = std::time::Instant::now();
        self.awaiting_start = false;
        self.send(json!({"cmd": "seek", "ms": (target * 1000.0) as f32}));
        true
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

    /// 恢复默认设置:保留播放列表 / 个性设置 / 上次播放记录等用户数据,
    /// 其余回到默认值,并把能实时生效的播放参数重新下发给壁纸进程。
    fn reset_settings(&mut self) {
        let old = std::mem::take(&mut self.settings);
        let mut fresh = crate::settings::Settings::default();
        fresh.playlist = old.playlist;
        fresh.playlist_pos = old.playlist_pos;
        fresh.set_overrides = old.set_overrides;
        fresh.lazer_set = old.lazer_set;
        fresh.lazer_sha2 = old.lazer_sha2;
        fresh.lazer_diff = old.lazer_diff;
        self.settings = fresh;

        apply_palette(self.settings.theme_mode != "light", &self.settings.accent);
        sync_close_mode(&self.settings);
        // 运行时派生状态(星级过滤 / SB 筛选 / 下拉菜单)
        self.star_min = self.settings.star_min;
        self.star_max = self.settings.star_max.unwrap_or(0.0);
        self.star_draft = (0.0, 0.0);
        self.sb_only = self.settings.sb_only;
        self.open_menu = None;
        self.close_remember = false;
        self.reset_confirm = false;
        // 自启 / 日志的外部状态也回到默认
        let _ = crate::settings::set_autostart(false);
        crate::logging::set_enabled(false);
        self.persist();
        // 曲库源 / 路径 / Unicode 显示已恢复默认,重新解析曲库
        self.spawn_library_load();
        self.apply_filter();
        // 能热更新的参数立刻生效
        self.send_live_settings();
        self.toast("已恢复默认设置");
    }

    /// 子进程刚(重)启动时,把保存的会话参数一次性下发。
    ///
    /// 子进程里的 `RenderMode` 默认是 `AutoPause`:不下发的话,设置页选「始终渲染」
    /// 也不会生效 —— 全屏遮挡时画面照样被拆掉(和 Tauri 版 `ctl::spawn_child`
    /// 起播前的那一批 `Set*` 命令完全对应)。
    fn send_child_settings(&mut self) {
        let (
            volume,
            hits_volume,
            hud,
            pp,
            hit_anim,
            brk_lighten,
            cursor_on,
            cursor_sz,
            snaking_in,
            snaking_out,
            cursor_trail,
            cursor_ripples,
            render_mode,
            debug_win,
            bg_opacity,
            master,
            offset,
            ghidden,
            fade,
            ffmpeg,
            ffprobe,
            log_on,
            fps,
        ) = {
            let s = &self.settings;
            (
                s.volume,
                if s.hitsound { s.hits_volume } else { 0.0 },
                s.hud,
                s.pp,
                !s.reduce_anim,
                s.break_lighten,
                s.cursor,
                s.cursor_size,
                s.snaking_in,
                s.snaking_out,
                s.cursor_trail,
                s.cursor_ripples,
                s.render_mode.clone(),
                s.debug_window,
                s.bg_opacity,
                s.master_volume,
                s.audio_offset_ms,
                // 单曲覆盖在 Load 里下发,这里只补全局值
                s.gameplay_hidden,
                s.fade_audio,
                s.ffmpeg.clone(),
                s.ffprobe.clone(),
                s.log_enabled,
                s.fps,
            )
        };
        let cmds = [
            json!({"cmd": "set_volume", "v": volume}),
            json!({"cmd": "set_hits_volume", "v": hits_volume}),
            json!({"cmd": "set_hud", "on": hud}),
            json!({"cmd": "set_pp", "on": pp}),
            json!({"cmd": "set_hit_animations", "on": hit_anim}),
            json!({"cmd": "set_break_lighten", "on": brk_lighten}),
            json!({"cmd": "set_cursor", "on": cursor_on}),
            json!({"cmd": "set_cursor_size", "x": cursor_sz}),
            json!({"cmd": "set_snaking_in", "on": snaking_in}),
            json!({"cmd": "set_snaking_out", "on": snaking_out}),
            json!({"cmd": "set_cursor_trail", "on": cursor_trail}),
            json!({"cmd": "set_cursor_ripples", "on": cursor_ripples}),
            json!({"cmd": "set_render_mode", "mode": render_mode}),
            json!({"cmd": "set_debug_window", "on": debug_win}),
            json!({"cmd": "set_bg_opacity", "v": bg_opacity}),
            json!({"cmd": "set_master", "v": master}),
            json!({"cmd": "set_offset", "ms": offset}),
            json!({"cmd": "set_gameplay_hidden", "on": ghidden}),
            json!({"cmd": "set_fade", "on": fade}),
            json!({"cmd": "set_ffmpeg_bins", "ffmpeg": ffmpeg, "ffprobe": ffprobe}),
            json!({"cmd": "set_log", "on": log_on}),
            // 帧率必须在起播前下发:子进程默认 0(跟随刷新率)
            json!({"cmd": "set_fps", "fps": fps}),
        ];
        for c in cmds {
            self.send(c);
        }
    }

    /// 把设置里可热更新的项重新下发(恢复默认后用)。
    fn send_live_settings(&mut self) {
        let s = &self.settings;
        let cmds = vec![
            json!({"cmd": "set_master", "v": s.master_volume}),
            json!({"cmd": "set_volume", "v": s.volume}),
            json!({"cmd": "set_hits_volume", "v": if s.hitsound { s.hits_volume } else { 0.0 }}),
            json!({"cmd": "set_speed", "x": s.speed}),
            json!({"cmd": "set_offset", "ms": s.audio_offset_ms}),
            json!({"cmd": "set_bg_opacity", "v": s.bg_opacity}),
            json!({"cmd": "set_snaking_in", "on": s.snaking_in}),
            json!({"cmd": "set_snaking_out", "on": s.snaking_out}),
            json!({"cmd": "set_hidden", "on": s.hidden}),
            json!({"cmd": "set_hud", "on": s.hud}),
            json!({"cmd": "set_pp", "on": s.pp}),
            json!({"cmd": "set_break_lighten", "on": s.break_lighten}),
            json!({"cmd": "set_hit_animations", "on": !s.reduce_anim}),
            json!({"cmd": "set_gameplay_hidden", "on": s.gameplay_hidden}),
            json!({"cmd": "set_fade", "on": s.fade_audio}),
            json!({"cmd": "set_cursor", "on": s.cursor}),
            json!({"cmd": "set_cursor_trail", "on": s.cursor_trail}),
            json!({"cmd": "set_cursor_ripples", "on": s.cursor_ripples}),
            json!({"cmd": "set_cursor_size", "x": s.cursor_size}),
            json!({"cmd": "set_video", "on": s.storyboard}),
            json!({"cmd": "set_beatmap_hitsounds", "on": s.beatmap_hitsounds}),
            json!({"cmd": "set_fail", "on": s.fail}),
            json!({"cmd": "set_upscale", "mode": s.upscale, "quality": s.upscale_quality}),
            json!({"cmd": "set_fps", "fps": s.fps}),
            json!({"cmd": "set_render_mode", "mode": s.render_mode}),
            json!({"cmd": "set_loop", "on": matches!(s.play_mode, crate::playlist::PlayMode::SingleLoop)}),
            json!({"cmd": "set_skin", "path": serde_json::Value::Null}),
            json!({"cmd": "set_audio_device", "id": serde_json::Value::Null}),
            json!({"cmd": "set_ffmpeg_bins", "ffmpeg": serde_json::Value::Null, "ffprobe": serde_json::Value::Null}),
            json!({"cmd": "set_log", "on": false}),
        ];
        for c in cmds {
            self.send(c);
        }
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
                title_u: t.title_u.clone(),
                artist: t.artist.clone(),
                artist_u: t.artist_u.clone(),
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
        let Some(r) = self.all_rows.iter().find(|r| r.set_id == set_id) else { return };
        let len = self.set_length_ms(set_id, sha2.as_deref());
        // 已在列表里 = 用这次的难度/mods 原位更新(位置不变),
        // 与旧版 Tauri 的"重复导入 = 替换现有条目参数"一致。
        if let Some(t) = self.queue.iter_mut().find(|t| t.set_id == set_id) {
            t.sha2 = sha2;
            t.mods = mods;
            t.title = r.title_raw.clone();
            t.title_u = r.title_u.clone();
            t.artist = r.artist_raw.clone();
            t.artist_u = r.artist_u.clone();
            t.length_ms = len;
            self.save_queue();
            self.toast("已更新现有条目");
            return;
        }
        self.queue.push(QueueTrack {
            set_id: r.set_id.clone(),
            sha2,
            title: r.title_raw.clone(),
            title_u: r.title_u.clone(),
            artist: r.artist_raw.clone(),
            artist_u: r.artist_u.clone(),
            length_ms: len,
            mods,
        });
        self.save_queue();
        self.toast("已加入播放列表");
    }

    fn add_to_queue(&mut self, set_id: &str) {
        let Some(r) = self.all_rows.iter().find(|r| r.set_id == set_id) else { return };
        // 快速加入按全局难度设置(难度不再单独设置)
        let policy = match self.settings.target_star {
            None => crate::playlist::PickPolicy::Hardest,
            Some(t) if t < 0.0 => crate::playlist::PickPolicy::Easiest,
            Some(t) => crate::playlist::PickPolicy::Custom(t),
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
        if let Some(t) = self.queue.iter_mut().find(|t| t.set_id == set_id) {
            t.sha2 = sha2;
            t.mods = mods;
            t.title = r.title_raw.clone();
            t.title_u = r.title_u.clone();
            t.artist = r.artist_raw.clone();
            t.artist_u = r.artist_u.clone();
            t.length_ms = len;
            self.save_queue();
            self.toast("已更新现有条目");
            return;
        }
        self.queue.push(QueueTrack {
            set_id: r.set_id.clone(),
            sha2,
            title: r.title_raw.clone(),
            title_u: r.title_u.clone(),
            artist: r.artist_raw.clone(),
            artist_u: r.artist_u.clone(),
            length_ms: len,
            mods,
        });
        self.save_queue();
        self.toast("已加入播放列表");
    }

    /// 把当前筛选结果全部加入播放列表:新条目追加,已存在条目原位更新,
    /// 使用确认框里选的 mods 与难度策略(每个谱面集按该策略各挑一档)。
    fn add_filtered_with_mods(&mut self, mods: u32, diff: BatchDiff) {
        let policy = match diff {
            BatchDiff::Hardest => crate::playlist::PickPolicy::Hardest,
            BatchDiff::Easiest => crate::playlist::PickPolicy::Easiest,
            BatchDiff::Custom(t) => crate::playlist::PickPolicy::Custom(t),
        };
        let ids: Vec<String> = self.rows.iter().map(|r| r.set_id.clone()).collect();
        let mut added = 0usize;
        let mut updated = 0usize;
        for id in ids {
            let Some(r) = self.all_rows.iter().find(|r| r.set_id == id) else { continue };
            let sha2 = self.pick_with_policy(&id, policy);
            let len = self.set_length_ms(&id, sha2.as_deref());
            // 已存在的条目原位更新(位置不变),不跳过 —— 后加入的导入
            // 可以刷新已有条目的难度/mods,统计里区分新增与更新。
            if let Some(t) = self.queue.iter_mut().find(|t| t.set_id == id) {
                t.sha2 = sha2;
                t.mods = mods;
                t.title = r.title_raw.clone();
                t.title_u = r.title_u.clone();
                t.artist = r.artist_raw.clone();
                t.artist_u = r.artist_u.clone();
                t.length_ms = len;
                updated += 1;
                continue;
            }
            self.queue.push(QueueTrack {
                set_id: r.set_id.clone(),
                sha2,
                title: r.title_raw.clone(),
                title_u: r.title_u.clone(),
                artist: r.artist_raw.clone(),
                artist_u: r.artist_u.clone(),
                length_ms: len,
                mods,
            });
            added += 1;
        }
        self.save_queue();
        self.toast(format!("已加入 {added} 条,更新 {updated} 条"));
    }

    fn unload(&mut self) {
        self.send(json!({"cmd": "unload"}));
        self.playing = false;
        self.awaiting_start = false;
        self.prog_ms = 0.0;
        self.prog_at = std::time::Instant::now();
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
                // 只是显示名切换:重新算一遍列表即可,不必重读整个曲库
                self.apply_filter();
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
            "hotkeys_enabled" => {
                self.settings.hotkeys_enabled = !self.settings.hotkeys_enabled;
                self.apply_hotkeys();
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
                // 与设置页下拉同序(Always → AutoPause → FsPause → FsSleep → Off)
                let order = ["always", "autopause", "fs_pause", "fs_sleep", "off"];
                let i = order.iter().position(|v| *v == self.settings.render_mode).unwrap_or(0);
                self.settings.render_mode = order[(i + 1) % order.len()].to_string();
                self.send(json!({"cmd": "set_render_mode", "mode": self.settings.render_mode}));
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
                    None => Some("min".into()),
                    Some("min") => Some("tray".into()),
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
            "batch_star" => ("★", 1.0),
            "quick_star" => ("★", 1.0),
            "ov_offset" => (" ms", 1.0),
            "ov_bg" => ("%", 0.01),
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
            "batch_star" => Field::NumBatchStar,
            "quick_star" => Field::NumQuickStar,
            "ov_offset" => Field::OvOffset,
            "ov_bg" => Field::OvBg,
            _ => return None,
        })
    }

    /// `ListState::reset` 会把滚动位置清零。删行/展开行导致条数变化时,
    /// 用这个"重置但保住当前位置",避免列表跳回最顶上。
    fn reset_list_keep_scroll(state: &gpui::ListState, len: usize) {
        let keep = state.logical_scroll_top();
        state.reset(len);
        if len > 0 {
            state.scroll_to(gpui::ListOffset {
                item_ix: keep.item_ix.min(len - 1),
                offset_in_item: keep.offset_in_item,
            });
        }
    }

    /// 全局快捷键的三行(捕获模式:按键而不是打字)
    fn is_hotkey(field: Field) -> bool {
        matches!(
            field,
            Field::HotkeyPrev | Field::HotkeyToggle | Field::HotkeyNext
        )
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
            Field::NumBatchStar => "batch_star",
            Field::NumQuickStar => "quick_star",
            Field::OvOffset => "ov_offset",
            Field::OvBg => "ov_bg",
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
            "batch_star" => match self.mods_dialog.as_ref().map(|d| d.batch_diff) {
                Some(BatchDiff::Custom(t)) => t,
                _ => 5.0,
            },
            "quick_star" => self.settings.custom_star_value(),
            "ov_offset" => self
                .overrides
                .as_ref()
                .and_then(|ed| ed.ov.audio_offset_ms)
                .map(f64::from)
                .unwrap_or(0.0),
            "ov_bg" => self
                .overrides
                .as_ref()
                .and_then(|ed| ed.ov.bg_opacity)
                .map(f64::from)
                .unwrap_or(0.7),
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
            "target_star" => {
                let v = v.clamp(0.5, 12.0);
                self.settings.target_star = Some(v);
                // 全局记住最近一次使用的自定义星级
                self.settings.custom_star = Some(v);
            }
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
            // 曲目个性设置的数值草稿(保存时由 save_overrides 落盘)
            "ov_offset" => {
                if let Some(ed) = self.overrides.as_mut() {
                    ed.ov.audio_offset_ms = Some(v.clamp(-200.0, 200.0) as f32);
                }
            }
            "ov_bg" => {
                if let Some(ed) = self.overrides.as_mut() {
                    ed.ov.bg_opacity = Some(v.clamp(0.0, 1.0) as f32);
                }
            }
            // 默认快速加入选项里的星级:写回全局记住的自定义星级
            "quick_star" => {
                let v = v.clamp(0.5, 12.0);
                self.settings.custom_star = Some(v);
                if self.settings.target_star.is_some_and(|t| t > 0.0) {
                    self.settings.target_star = Some(v);
                }
            }
            // 批量导入对话框的自定义星级:只改草稿,不写全局设置
            "batch_star" => {
                let v = v.clamp(0.5, 12.0);
                // 全局记住;若当前全局难度就是「自定义星级」,一并跟随,
                // 保证设置页和批量导入用的是同一个值。
                self.settings.custom_star = Some(v);
                if self.settings.target_star.is_some_and(|t| t > 0.0) {
                    self.settings.target_star = Some(v);
                }
                if let Some(d) = self.mods_dialog.as_mut() {
                    d.batch_diff = BatchDiff::Custom(v);
                }
            }
            _ => {}
        }
        // 只改草稿的数值项不必立刻落盘(保存按钮才写入个性设置)
        if !matches!(kind, "batch_star" | "ov_offset" | "ov_bg") {
            self.persist();
        }
    }

    // ---- 文本编辑 ----

    fn begin_edit(&mut self, field: Field, window: &mut Window) {
        // 进入快捷键捕获:先注销当前注册,否则按同一组合会被系统先吞掉,捕获不到
        if Self::is_hotkey(field) {
            let _ = crate::hotkeys::apply(false, None, None, None);
        }
        let value = match field {
            Field::Search => self.search.clone(),
            Field::LazerDir => self.settings.lazer_dir.clone().unwrap_or_default(),
            Field::StableDir => self.settings.stable_dir.clone().unwrap_or_default(),
            Field::Ffmpeg => self.settings.ffmpeg.clone().unwrap_or_default(),
            Field::Ffprobe => self.settings.ffprobe.clone().unwrap_or_default(),
            Field::Skin => self.settings.skin.clone().unwrap_or_default(),
            Field::HotkeyPrev => self.settings.hotkey_prev.clone().unwrap_or_default(),
            Field::HotkeyToggle => self.settings.hotkey_toggle.clone().unwrap_or_default(),
            Field::HotkeyNext => self.settings.hotkey_next.clone().unwrap_or_default(),
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
            // 快捷键:三条文本一起重新注册(空 = 不绑定);规范化后的文本写回
            Field::HotkeyPrev => {
                self.settings.hotkey_prev = opt(text);
                self.apply_hotkeys();
            }
            Field::HotkeyToggle => {
                self.settings.hotkey_toggle = opt(text);
                self.apply_hotkeys();
            }
            Field::HotkeyNext => {
                self.settings.hotkey_next = opt(text);
                self.apply_hotkeys();
            }
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

    /// 快捷键:把设置里的三条文本交给 `hotkeys` 线程重新注册(未启用 = 全部注销)。
    fn apply_hotkeys(&mut self) {
        match crate::hotkeys::apply(
            self.settings.hotkeys_enabled,
            self.settings.hotkey_prev.as_deref(),
            self.settings.hotkey_toggle.as_deref(),
            self.settings.hotkey_next.as_deref(),
        ) {
            Ok(t) => {
                self.settings.hotkey_prev = t.prev;
                self.settings.hotkey_toggle = t.toggle;
                self.settings.hotkey_next = t.next;
            }
            Err(e) => self.toast(format!("快捷键:{e}")),
        }
    }

    fn on_key(&mut self, field: Field, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.edit.as_ref().map(|e| e.field) != Some(field) {
            return;
        }
        // 快捷键是"捕获"模式:点进输入框后按什么组合就填什么,不用手打文字。
        if Self::is_hotkey(field) {
            let key = ev.keystroke.key.as_str();
            if key == "escape" {
                // 放弃本次捕获,恢复原来的注册
                self.edit = None;
                self.apply_hotkeys();
                cx.notify();
                return;
            }
            let text = if matches!(key, "backspace" | "delete") {
                // 退格 / Delete = 清空这条绑定
                String::new()
            } else {
                match hotkey_chord_text(&ev.keystroke) {
                    Some(t) => t,
                    // 只按了修饰键:继续等主键
                    None => return,
                }
            };
            if let Some(ed) = self.edit.as_mut() {
                ed.buf = text;
            }
            // 捕获到完整组合后立即落账并重新注册
            self.commit_edit();
            cx.notify();
            return;
        }
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
        self.close_ask = false;
        #[cfg(windows)]
        crate::win::block_ask();
        let handle = WINDOW.lock().ok().and_then(|g| *g);
        if let Some(handle) = handle {
            cx.defer(move |cx: &mut gpui::App| {
                let _ = handle.update(cx, |_, window, _| window.minimize_window());
            });
        }
    }

    /// 收到托盘:藏窗口,任务栏不再留按钮。标题栏最小化不走这里。
    /// 先让下一帧把询问框画掉并交出去,再藏。否则还原时会闪出藏起来前的那一帧。
    fn hide_to_tray(&mut self, cx: &mut Context<Self>) {
        self.close_ask = false;
        self.release_ui_caches();
        #[cfg(windows)]
        {
            crate::win::block_ask();
            let handle = WINDOW.lock().ok().and_then(|g| *g);
            if let Some(handle) = handle {
                cx.notify();
                cx.defer(move |cx: &mut gpui::App| {
                    let _ = handle.update(cx, |_, window, _| {
                        window.on_next_frame(|window, _| {
                            // 这一帧会先把询问框画掉并 present。藏窗口放到再下一帧开头。
                            window.on_next_frame(|_, _| {
                                crate::win::hide_window(
                                    MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed),
                                );
                            });
                        });
                    });
                });
            } else {
                crate::win::hide_window(MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed));
            }
        }
        #[cfg(not(windows))]
        self.minimize_window(cx);
    }

    /// 放掉界面上的解码封面。曲库索引留下,否则托盘切歌和再打开窗口都要重读 realm。
    fn release_ui_caches(&mut self) {
        self.thumbs.lock().unwrap().clear();
        self.thumb_order.lock().unwrap().clear();
        self.hero.clear();
        self.hero_order.clear();
        #[cfg(windows)]
        crate::win::trim_working_set();
    }

    /// 延后一帧激活窗口:不能在 update 内部再 update 同一个 App(会 panic)。
    fn activate_window(&mut self, cx: &mut Context<Self>) {
        #[cfg(windows)]
        crate::win::show_window(MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed));
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
        // 拖动结束后再清"拖动期间保留"的滚动条高亮(松手瞬间拖动可能还没结束)
        if self.sb_sticky.get().is_some() && !cx.has_active_drag() {
            self.sb_sticky.set(None);
            self.sb_hover.set(None);
            dirty = true;
        }
        // 曲终自动切歌:单曲循环重播本曲;顺序/随机都走 step(),
        // 随机模式沿洗好的整轮排列前进(不再是"下一首 +1")。
        let ended = {
            let mut st = self.status.lock().unwrap();
            std::mem::take(&mut st.ended)
        };
        if ended && !self.queue.is_empty() {
            match self.settings.play_mode {
                crate::playlist::PlayMode::SingleLoop => self.play_index(self.pos),
                _ => self.step(1),
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
        // 拖动排序在列表外松手不会触发 on_drop:没有活动拖动就清掉让位预览,
        // 否则行会一直保持半透明/错位状态。
        if self.dragging.is_some() && !cx.has_active_drag() {
            self.dragging = None;
            self.drag_over = None;
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
        for action in crate::hotkeys::poll() {
            match action {
                crate::hotkeys::Action::Prev => self.step(-1),
                crate::hotkeys::Action::Toggle => self.toggle_pause(),
                crate::hotkeys::Action::Next => self.step(1),
            }
        }
        let (t, playing, dur) = {
            let st = self.status.lock().unwrap();
            (st.t, st.playing, st.duration)
        };
        if self.awaiting_start {
            // 载入新曲期间:子进程还在报上一首的状态(t_ms 是旧值),
            // 等 loaded 事件(load_seq 变化)再接受,进度条期间保持 0。
            let loaded = self
                .status
                .lock()
                .map(|st| st.load_seq != self.load_seq_seen)
                .unwrap_or(false);
            // 兜底:加载失败/没有 loaded 事件时最多等 8 秒
            if loaded || self.prog_at.elapsed().as_secs() > 8 {
                self.awaiting_start = false;
                // 新曲从 0 开始:不要用可能还是上一首的状态值
                self.playing = true;
                self.prog_ms = 0.0;
                self.prog_at = std::time::Instant::now();
                dirty = true;
            }
        } else {
            self.playing = playing;
            // seek 后 400ms 内忽略迟到的旧状态,避免进度条被拉回去
            if self.seek_guard.elapsed().as_millis() < 400 {
                // 只同步播放状态,不重锚
            } else if (t - self.prog_ms).abs() > 0.05 {
                self.prog_ms = t;
                self.prog_at = std::time::Instant::now();
                dirty = true;
            }
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

/// 谱面集状态文字标签(osu!lazer `BeatmapSetStatus`)。
///
/// 取值(见 `lazer::LazerSet::status`):-3 Unknown / -2 Unsubmitted /
/// 0 Pending / 1 Ranked / 2 Approved / 3 Qualified / 4 Loved /
/// 5 Graveyard / 6 WIP / 7 NeedsUpdate。
/// -3 不显示:osu!stable 源没有在线状态,整库都是 -3,画出来会满屏灰标。
fn status_badge(status: i32) -> impl IntoElement {
    let (label, bg, fg) = match status {
        1 => ("ranked", 0xb3ff66u32, 0x1a2a00u32),
        2 => ("approved", 0x99ee55, 0x1a2a00),
        3 => ("qualified", 0xffd966, 0x2a2000),
        4 => ("loved", 0xff66aa, 0xffffff),
        0 => ("pending", 0x6aa0e0, 0xffffff),
        5 => ("graveyard", 0x777788, 0xffffff),
        6 => ("wip", 0xd8a24a, 0x2a1c00),
        7 => ("needs update", 0xe06666, 0xffffff),
        -2 => ("unsubmitted", 0x777788, 0xffffff),
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

/// 三态下拉的存储值 ↔ 选项 key。
fn tri_key(v: Option<bool>) -> String {
    match v {
        None => "global".to_string(),
        Some(true) => "on".to_string(),
        Some(false) => "off".to_string(),
    }
}

fn tri_value(key: &str) -> Option<bool> {
    match key {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
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
                            st.load_seq = st.load_seq.wrapping_add(1);
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
        // 每帧都跑一次 poll:状态/进度/曲库结果当帧更新,不再靠 250ms 心跳
        // (以前那是 4Hz 的"人为帧率上限")。poll 自己只在真有变化时 notify。
        let _ = self.poll(cx);
        // 只有在"正在播放"时才需要连续出帧(进度条用本地时钟插值)。
        // 其余时间交给事件驱动:悬停动画由 AnimationElement 自己请求帧,
        // 不需要整页每帧重绘 —— 否则设置/关于页的悬停过渡会被挤掉帧。
        if self.playing && !self.awaiting_start {
            window.request_animation_frame();
        }
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
        let titlebar = div()
            .id("custom-titlebar")
            .flex_none()
            .flex()
            .h(px(34.0))
            .items_center()
            .bg(rgb(pal().panel))
            .text_color(rgb(pal().text))
            .child({
                // Windows 把这块标成标题栏后,拖动和双击最大化由系统完成。
                // 自己在鼠标回调里发 WM_NCLBUTTONDOWN 会重入窗口过程并闪退。
                let drag = div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .px_3()
                    .text_sm()
                    .window_control_area(gpui::WindowControlArea::Drag)
                    .child("Aria");
                #[cfg(not(windows))]
                let drag = drag.on_mouse_down(gpui::MouseButton::Left, |event, window, _cx| {
                    if event.click_count >= 2 {
                        window.zoom_window();
                    } else {
                        window.start_window_move();
                    }
                });
                drag
            })
            .child(
                div()
                    .flex_none()
                    .flex()
                    .h_full()
                    .items_center()
                    .child(
                        div()
                            .id("minimize-window")
                            .flex()
                            .w(px(46.0))
                            .h_full()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|this| this.bg(rgb(pal().ctl)))
                            .child("-")
                            .on_click(|_, window, _| window.minimize_window()),
                    )
                    .child(
                        div()
                            .id("zoom-window")
                            .flex()
                            .w(px(46.0))
                            .h_full()
                            .items_center()
                            .justify_center()
                            .window_control_area(gpui::WindowControlArea::Max)
                            .cursor_pointer()
                            .hover(|this| this.bg(rgb(pal().ctl)))
                            .child(if window.is_maximized() { "❐" } else { "□" })
                            .when(cfg!(not(windows)), |this| {
                                this.on_click(|_, window, _| window.zoom_window())
                            }),
                    )
                    .child(
                        div()
                            .id("close-window")
                            .flex()
                            .w(px(46.0))
                            .h_full()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|this| this.bg(rgb(0xc42b3c)).text_color(rgb(0xffffff)))
                            .child("×")
                            .on_click(cx.listener(|this, _event, window, cx| {
                                if CLOSE_TO_TRAY.load(std::sync::atomic::Ordering::Relaxed) {
                                    #[cfg(windows)]
                                    this.hide_to_tray(cx);
                                    #[cfg(not(windows))]
                                    window.minimize_window();
                                } else if CLOSE_TO_MIN.load(std::sync::atomic::Ordering::Relaxed) {
                                    window.minimize_window();
                                } else if CLOSE_TO_EXIT.load(std::sync::atomic::Ordering::Relaxed) {
                                    #[cfg(target_os = "linux")]
                                    let _ = crate::kde::restore_wallpaper();
                                    this.persist();
                                    std::process::exit(0);
                                } else {
                                    this.close_ask = true;
                                    this.close_remember = false;
                                    #[cfg(windows)]
                                    crate::win::set_ask_open(true);
                                    cx.notify();
                                }
                            })),
                    ),
            );

        let header = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .bg(rgb(pal().panel))
            // 左上角 logo(旧 Web UI 头部的 icon.png,38×38)
            .child(
                div().w(px(38.0)).h(px(38.0)).flex_none().child(
                    gpui::img(gpui::ImageSource::Image(self.logo.clone()))
                        .w_full()
                        .h_full()
                        .object_fit(gpui::ObjectFit::Contain),
                ),
            )
            // 标题 + 副标题竖排(与旧 UI 的 .titles 一致)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .child(div().text_lg().child("Aria"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(pal().dim))
                            .child("osu! storyboard 桌面动态壁纸"),
                    ),
            )
            .child(div().flex_1())
            .child(div().text_xs().text_color(rgb(pal().dim)).child(self.status_line.clone()))
            // 运行状态靠右,紧挨「关于」左边
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
                    // 捕获中途点了别处:丢掉草稿并恢复原来的注册
                    if this.edit.as_ref().is_some_and(|e| Self::is_hotkey(e.field)) {
                        this.edit = None;
                        this.apply_hotkeys();
                    }
                    cx.notify();
                }
            }))
            .size_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(rgb(pal().bg))
            .text_color(rgb(pal().text))
            .child(titlebar)
            .child(header)
            .child(body)
            .child(bar);
        if let Some((msg, _)) = toast {
            root = root.child(
                div().px_3().py_1().bg(rgb(pal().toast_bg)).text_sm().text_color(rgb(pal().toast_fg)).child(msg),
            );
        }
        // 下拉菜单打开时垫一层全屏遮罩:点菜单外只关菜单,不穿透到下层
        // 按钮(菜单本身是 deferred,画在遮罩之上)。
        if self.open_menu.is_some() {
            root = root.child(self.menu_closer(cx));
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
        if self.ov_clear_confirm {
            root = root.child(self.ov_clear_modal(cx));
        }
        if self.reset_confirm {
            root = root.child(self.reset_modal(cx));
        }
        #[cfg(windows)]
        {
            // 最小化/隐藏时窗口过程会把询问清掉。这里跟那个状态,避免还原后再画一帧。
            self.close_ask = crate::win::ask_open();
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

    /// 带固定宽度的下拉:按钮宽度不随内容伸缩(超长文本单行省略)。
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
                            // 菜单面板挡住下层:GPUI 默认把所有命中元素都算
                            // hover,不加 occlude 会同时触发菜单下面的按钮。
                            .occlude()
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
                            .child(self.tracked_scrollbar("menu", &self.menu_scroll, cx)),
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
                    .map(|d| match width {
                        Some(w) => d.w(px(w)),
                        None => d,
                    })
                    // 定宽时单行省略:长文本不换行、不把按钮撑高
                    .when(width.is_some(), |d| d.whitespace_nowrap().text_ellipsis())
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

    /// 下拉菜单的全屏点击遮罩:挡住下层控件(否则点空白/点菜单都会把
    /// 事件同时发给下面的按钮),点击任意位置只关闭菜单;滚轮仍可滚动。
    fn menu_closer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("dd-closer")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .block_mouse_except_scroll()
            .on_any_mouse_down(cx.listener(|this, _e, _w, cx| {
                if this.open_menu.take().is_some() {
                    cx.notify();
                }
            }))
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
                match value.as_str() {
                    "hardest" => self.settings.target_star = None,
                    "easiest" => self.settings.target_star = Some(-1.0),
                    // 切到「自定义星级」:用全局记住的上次自定义值
                    "custom" => {
                        let v = self.settings.custom_star_value();
                        self.settings.custom_star = Some(v);
                        self.settings.target_star = Some(v);
                    }
                    other => {
                        self.settings.target_star =
                            other.parse::<f64>().ok().filter(|v| *v > 0.0)
                    }
                }
            }
            DropKind::RenderMode => {
                self.settings.render_mode = value;
                // 运行中切换也要立刻下发,否则子进程还按旧模式走(改了没反应)
                self.send(json!({"cmd": "set_render_mode", "mode": self.settings.render_mode}));
            }
            DropKind::Speed => {
                self.settings.speed = value.parse().unwrap_or(1.0);
                self.send(json!({"cmd": "set_speed", "x": self.settings.speed}));
            }
            DropKind::QueueDiff => {
                // 目标 = 正在预览的那首,其次是实际在放的那首,最后才退回 queue[pos]
                let target_set = self
                    .preview
                    .as_ref()
                    .map(|(id, _)| id.clone())
                    .or_else(|| self.settings.lazer_set.clone())
                    .or_else(|| self.queue.get(self.pos).map(|q| q.set_id.clone()));
                let qi = target_set
                    .as_ref()
                    .and_then(|id| self.queue.iter().position(|q| &q.set_id == id));
                let sha = if value.is_empty() { None } else { Some(value.clone()) };
                match qi {
                    // 在播放列表里:改那一行的难度并重播
                    Some(i) => {
                        if let Some(t) = self.queue.get_mut(i) {
                            t.sha2 = sha;
                        }
                        self.save_queue();
                        self.play_index(i);
                    }
                    // 只是曲库预览、还没进播放列表:直接用它开播(预览态随之清掉)
                    None => {
                        if let Some(id) = target_set {
                            self.play(&id, sha.as_deref(), self.settings.quick_mods);
                        }
                    }
                }
            }
            DropKind::OvStoryboard | DropKind::OvHitsound
            | DropKind::OvBeatmapHitsounds | DropKind::OvGameplayHidden => {
                if let Some(ed) = self.overrides.as_mut() {
                    let v = tri_value(&value);
                    match kind {
                        DropKind::OvStoryboard => ed.ov.storyboard = v,
                        DropKind::OvHitsound => ed.ov.hitsound = v,
                        DropKind::OvBeatmapHitsounds => ed.ov.beatmap_hitsounds = v,
                        DropKind::OvGameplayHidden => ed.ov.gameplay_hidden = v,
                        _ => {}
                    }
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

    /// 传输图标按钮:实心/描边方框 + 悬停底色 + 路径图标(不依赖字体/emoji)。
    fn icon_btn(
        &self,
        id: &'static str,
        icon: TransportIcon,
        filled: bool,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let (bg, fg, hover, w) = if filled {
            // 播放/暂停:主题色圆钮 + 白图标
            (pal().accent, 0xffffff, pal().accent, 46.0)
        } else {
            (pal().ctl, pal().text, pal().ctl_hover, 38.0)
        };
        div()
            .id(gpui::ElementId::Name(format!("ib{id}").into()))
            .w(px(w))
            .h(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(rgb(bg))
            .cursor_pointer()
            .hover(move |st| st.bg(rgb(hover)))
            .on_click(cx.listener(move |this, _e, _w, cx| {
                f(this, cx);
                cx.notify();
            }))
            .child(
                gpui::canvas(
                    |_b, _w, _cx| {},
                    move |b, _, w, _cx| {
                        if let Some(path) = transport_icon_path(icon, b) {
                            w.paint_path(path, rgb(fg));
                        }
                    },
                )
                .w(px(16.0))
                .h(px(16.0)),
            )
    }

    #[track_caller]
    fn small_btn(
        &self,
        label: impl Into<String>,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        self.small_btn_w(label, None, cx, f)
    }

    /// 固定宽度的工具按钮(工具栏对齐用)。
    #[track_caller]
    fn small_btn_w(
        &self,
        label: impl Into<String>,
        width: Option<f32>,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        self.small_btn_full(label, width, None, cx, f)
    }

    /// 同一个函数里渲染多行同名按钮时用:`key` 拼进 id,各行才互不抢点击。
    /// (只用 标签 + 调用点 做 id 时,`path_row` 的四个「浏览…」会共用一个 id,
    /// 表现就是只有第一行能点。)
    #[track_caller]
    fn small_btn_keyed(
        &self,
        label: impl Into<String>,
        key: impl Into<String>,
        width: Option<f32>,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        self.small_btn_full(label, width, Some(key.into()), cx, f)
    }

    #[track_caller]
    fn small_btn_full(
        &self,
        label: impl Into<String>,
        width: Option<f32>,
        key: Option<String>,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let label = label.into();
        // 稳定且唯一的 id:标签 + 调用点(file:line)[+ 调用方给的 key]。只按
        // 标签命名时,同一帧里两个同名按钮(例如队列头的「清空」与清空确认框
        // 的「清空」)会共用交互状态:背景按钮在 capture 阶段把 pending 清掉,
        // 前面那个按钮的点击就丢了(表现为"点了没反应")。id 每帧变化同样会丢点击。
        let loc = std::panic::Location::caller();
        let id = match key {
            Some(k) => format!("btn::{}@{}:{}#{}", label, loc.file(), loc.line(), k),
            None => format!("btn::{}@{}:{}", label, loc.file(), loc.line()),
        };
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

    /// 危险操作按钮(退出/清空/恢复默认/清除个性设置):红底白字。
    #[track_caller]
    fn danger_btn(
        &self,
        label: impl Into<String>,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let label = label.into();
        let loc = std::panic::Location::caller();
        let id = format!("danger::{}@{}:{}", label, loc.file(), loc.line());
        div()
            .id(gpui::ElementId::Name(id.into()))
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(pal().danger))
            .text_color(rgb(0xffffff))
            .text_sm()
            .cursor_pointer()
            .hover(|st| st.bg(rgb(pal().danger_hover)))
            .child(label)
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
                if shown.is_empty() && Self::is_hotkey(field) {
                    // 捕获中:提示"按键",不是让你打字
                    "按下组合键…▏".to_string()
                } else {
                    format!("{}▏", shown)
                }
            } else if shown.is_empty() {
                match field {
                    Field::Search => "搜索".to_string(),
                    Field::HotkeyPrev | Field::HotkeyToggle | Field::HotkeyNext => {
                        "未设置".to_string()
                    }
                    _ => "…".to_string(),
                }
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
                        // 有过滤才高亮:默认(★0–∞)不高亮,和普通按钮一致
                        let filtering = self.star_min > 0.0 || self.star_max > 0.0;
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
                            .bg(if filtering { rgb(pal().accent) } else { rgb(pal().ctl) })
                            .text_color(if filtering { rgb(0xffffff) } else { rgb(pal().text) })
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
                                this.settings.sb_only = this.sb_only;
                                this.apply_filter();
                                this.persist();
                                cx.notify();
                            })),
                    )
                    .child(self.dropdown_w(DropKind::Collection, self.settings.collection.clone().unwrap_or_default(), {
                            let mut opts = vec![("".to_string(), "全部谱面".to_string())];
                            opts.extend(self.collections.iter().map(|c| (c.clone(), c.clone())));
                            opts
                        }, Some(152.0), cx))

                    .child(self.small_btn("全部加入", cx, |this, _| {
                        // 与旧版一致:不直接入列,先弹确认框(数量 + mods)
                        let n = this.rows.len();
                        if n == 0 {
                            this.toast("当前过滤条件下没有谱面");
                            return;
                        }
                        this.mods_dialog = Some(ModsDialog {
                            set_id: String::new(),
                            sha2: None,
                            mods: 0,
                            queue_index: None,
                            bulk: None,
                            batch: Some(n),
                            // 默认跟当前「难度」设置一致,可在弹窗里改
                            batch_diff: match this.settings.target_star {
                                None => BatchDiff::Hardest,
                                Some(t) if t < 0.0 => BatchDiff::Easiest,
                                Some(t) => BatchDiff::Custom(t.clamp(0.5, 12.0)),
                            },
                        });
                    })),
            );

        // 虚拟列表:行 + 展开的难度块各占一项。
        // 这里用算术映射代替"每帧重建 Vec<item>":曲库几万首时,以前每帧都要
        // 分配并填充一遍 items,白白吃掉帧预算。
        let exp_row = self.expanded_row.filter(|i| *i < self.rows.len());
        let item_count = self.rows.len() + usize::from(exp_row.is_some());
        if self.lib_list.item_count() != item_count {
            // 展开/收起难度块会改变条数:保位置,别把曲库滚回开头
            Self::reset_list_keep_scroll(&self.lib_list, item_count);
        }
        let entity = cx.entity();
        let list = gpui::list(self.lib_list.clone(), move |ix, _w, cx| {
            let app = entity.read(cx);
            let (ri, is_chips) = match exp_row {
                // 展开行的下一项是难度块
                Some(k) if ix == k + 1 => (k, true),
                // 它之后的项整体后移一位
                Some(k) if ix > k + 1 => (ix - 1, false),
                _ => (ix, false),
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
                                self.probe_lazer_root
                                    .as_ref()
                                    .map(|p| p.to_string_lossy().into_owned())
                            })
                            .unwrap_or_default(),
                    ),
            )
            .child(list)
    }

    /// 普通滚动容器的跟随型滚动条(轮滚时刷新)。
    /// 自绘滚动条(设置页 / 关于页 / 菜单列表共用),可拖拽、带悬停过渡。
    ///
    /// **gpui 的滚动偏移是负值**:顶部 = 0,越往下越接近 `-max_offset`。所以滑块
    /// 位置必须用 `-offset / max` 换算;以前直接拿 `offset` 当正值算,结果永远是
    /// 0,滑块一直贴在顶端 —— 拖动时内容在动、条不动,看起来就是"滚动条不滚动"。
    fn tracked_scrollbar(
        &self,
        id: &'static str,
        h: &gpui::ScrollHandle,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let max: f32 = h.max_offset().height.into();
        let view: f32 = h.bounds().size.height.into();
        // 不需要滚动(或还没量出高度):不画条,也不留 1px 缝隙
        if max <= 0.5 {
            return div()
                .id(gpui::ElementId::Name(format!("sb{id}hidden").into()))
                .into_any_element();
        }
        // 滑块高度占比;内容比视口短时退化成整条
        let ratio = (view / (view + max)).clamp(0.08, 1.0);
        // 顶部 0 → 底部 1(注意 gpui 的 offset 是负值)
        let frac = scroll_frac(f32::from(h.offset().y), max);

        let hot = self.sb_hover.get() == Some(id);
        let dur = Duration::from_millis(140);
        let (idle, accent) = (pal().thumb, pal().accent);

        // 像素 → 滚动偏移(负值,和 gpui 内部一致;句柄按值捕获,供 'static 回调使用)
        let h_seek = h.clone();
        let seek = move |track_top: f32, track_h: f32, pointer_y: f32| {
            let max: f32 = h_seek.max_offset().height.into();
            let view: f32 = h_seek.bounds().size.height.into();
            if max <= 0.0 {
                return;
            }
            let r = (view / (view + max)).clamp(0.08, 1.0);
            let thumb_h = track_h * r;
            let rel = (pointer_y - track_top - thumb_h / 2.0)
                .clamp(0.0, (track_h - thumb_h).max(0.0));
            let f = if track_h - thumb_h > 0.0 {
                rel / (track_h - thumb_h)
            } else {
                0.0
            };
            h_seek.set_offset(gpui::point(px(0.0), px(-f * max)));
        };

        let thumb = div()
            .id(gpui::ElementId::Name(format!("sbth{id}").into()))
            .absolute()
            .left(px(2.0))
            .right(px(2.0))
            .h(gpui::relative(ratio))
            // top 用比例定位:轨道高度随窗口变化也不用重新算像素
            .top(gpui::relative(frac * (1.0 - ratio)))
            .rounded_full()
            .bg(rgb(idle))
            // 悬停:提亮 + 加宽(动画只挂在滑块上,轨道 id 保持稳定,不打断拖拽)
            .with_animation(
                gpui::ElementId::Name(format!("sbth{id}-{hot}").into()),
                gpui::Animation::new(dur).with_easing(gpui::ease_in_out),
                move |el, t| {
                    let c = if hot {
                        lerp_rgb(idle, accent, t)
                    } else {
                        lerp_rgb(accent, idle, t)
                    };
                    let inset = if hot { 2.0 - t } else { 1.0 + t };
                    el.bg(rgb(c)).left(px(inset)).right(px(inset))
                },
            );

        div()
            .id(gpui::ElementId::Name(format!("sb{id}").into()))
            .absolute()
            .right(px(2.0))
            .top_0()
            .bottom_0()
            .w(px(10.0))
            .rounded_full()
            .bg(rgb(pal().line2))
            .cursor_pointer()
            // 盖在内容上:拖/点滚动条时不要误触下面的控件
            .occlude()
            .on_hover(cx.listener(move |this, hovered: &bool, _w, cx| {
                let next = if *hovered { Some(id) } else { None };
                if next.is_none() && cx.has_active_drag() {
                    // 拖动中指针滑出轨道:不要清高亮,松手后由 poll 收尾
                    this.sb_sticky.set(Some(id));
                    return;
                }
                this.sb_sticky.set(None);
                if this.sb_hover.get() != next && (next.is_some() || this.sb_hover.get() == Some(id)) {
                    this.sb_hover.set(next);
                    cx.notify();
                }
            }))
            // 按住拖动:整条轨道都是热区(点一下不拖不会有位移,和系统滚动条一致)
            .on_drag(ScrollBarDrag(id), |_, _, _, cx| cx.new(|_| DragGhost))
            .on_drag_move::<ScrollBarDrag>(cx.listener(move |this, e: &gpui::DragMoveEvent<ScrollBarDrag>, _w, cx| {
                if e.drag(cx).0 != id {
                    return;
                }
                let track_h: f32 = e.bounds.size.height.into();
                if track_h <= 0.0 {
                    return;
                }
                // 拖动期间保持高亮
                if this.sb_hover.get() != Some(id) {
                    this.sb_hover.set(Some(id));
                }
                // 不节流:每个移动事件都立即生效(界面刷新由逐帧渲染驱动)
                seek(
                    f32::from(e.bounds.origin.y),
                    track_h,
                    f32::from(e.event.position.y),
                );
                cx.notify();
            }))
            .child(thumb)
            .into_any_element()
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
        if max <= 0.5 {
            return div()
                .id(gpui::ElementId::Name(format!("bar{id}hidden").into()))
                .into_any_element();
        }
        let ratio = (view / (view + max)).clamp(0.08, 1.0);
        // ListState 的 scroll_px_offset_for_scrollbar 也是负值(内部返回 -offset),
        // 直接交给 scroll_frac(它只取一次负)。再套一层负号会让比例恒为 0。
        let frac = scroll_frac(
            f32::from(state.scroll_px_offset_for_scrollbar().y),
            max,
        );
        let hot = self.sb_hover.get() == Some(id);
        let dur = Duration::from_millis(140);
        let (idle, accent) = (pal().thumb, pal().accent);

        // 指针 → 滚动位置(ListState 的偏移已经取过负号,这边按正值算)
        let st = state.clone();
        let seek = move |track_top: f32, track_h: f32, pointer_y: f32| {
            let max: f32 = st.max_offset_for_scrollbar().height.into();
            let view: f32 = st.viewport_bounds().size.height.into();
            if max <= 0.0 {
                return;
            }
            let r = (view / (view + max)).clamp(0.08, 1.0);
            let thumb_h = track_h * r;
            let rel = (pointer_y - track_top - thumb_h / 2.0)
                .clamp(0.0, (track_h - thumb_h).max(0.0));
            let f = if track_h - thumb_h > 0.0 {
                rel / (track_h - thumb_h)
            } else {
                0.0
            };
            let cur: f32 = (-st.scroll_px_offset_for_scrollbar().y).into();
            st.scroll_by(px(f * max - cur));
        };

        let thumb = div()
            .id(gpui::ElementId::Name(format!("th{id}").into()))
            .absolute()
            .left(px(2.0))
            .right(px(2.0))
            .h(gpui::relative(ratio))
            .top(gpui::relative(frac * (1.0 - ratio)))
            .rounded_full()
            .bg(rgb(idle))
            // 悬停:提亮 + 加宽(动画只挂在滑块上,轨道 id 保持稳定,不打断拖拽)
            .with_animation(
                gpui::ElementId::Name(format!("th{id}-{hot}").into()),
                gpui::Animation::new(dur).with_easing(gpui::ease_in_out),
                move |el, t| {
                    let c = if hot {
                        lerp_rgb(idle, accent, t)
                    } else {
                        lerp_rgb(accent, idle, t)
                    };
                    let inset = if hot { 2.0 - t } else { 1.0 + t };
                    el.bg(rgb(c)).left(px(inset)).right(px(inset))
                },
            );

        div()
            .id(gpui::ElementId::Name(format!("bar{id}").into()))
            .absolute()
            .right(px(2.0))
            .top_0()
            .bottom_0()
            .w(px(10.0))
            .rounded_full()
            .bg(rgb(pal().line2))
            .cursor_pointer()
            // 滚动条盖在行上:挡住点击,避免拖滚动条时误触下面的行
            .occlude()
            .on_hover(cx.listener(move |this, hovered: &bool, _w, cx| {
                let next = if *hovered { Some(id) } else { None };
                if next.is_none() && cx.has_active_drag() {
                    // 拖动中指针滑出轨道:保留高亮,松手后由 poll 收尾
                    this.sb_sticky.set(Some(id));
                    return;
                }
                this.sb_sticky.set(None);
                if this.sb_hover.get() != next && (next.is_some() || this.sb_hover.get() == Some(id)) {
                    this.sb_hover.set(next);
                    cx.notify();
                }
            }))
            // 整条轨道都能拖(不只滑块)
            .on_drag(ListBarDrag, |_, _, _, cx| cx.new(|_| DragGhost))
            .on_drag_move::<ListBarDrag>(cx.listener(move |this, e: &gpui::DragMoveEvent<ListBarDrag>, _w, cx| {
                let track_h: f32 = e.bounds.size.height.into();
                if track_h <= 0.0 {
                    return;
                }
                // 拖动期间保持高亮(指针可能已经滑出轨道)
                if this.sb_hover.get() != Some(id) {
                    this.sb_hover.set(Some(id));
                }
                seek(
                    f32::from(e.bounds.origin.y),
                    track_h,
                    f32::from(e.event.position.y),
                );
                cx.notify();
            }))
            .child(thumb)
            .into_any_element()
    }

    /// 曲库虚拟列表滚动条:可拖拽 + 悬停加宽提亮。
    fn lib_scrollbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let max: f32 = self.lib_list.max_offset_for_scrollbar().height.into();
        let view: f32 = self.lib_list.viewport_bounds().size.height.into();
        if max <= 0.5 {
            return div()
                .id(gpui::ElementId::Name("libbarhidden".into()))
                .into_any_element();
        }
        let ratio = (view / (view + max)).clamp(0.08, 1.0);
        // 同上:负偏移,别再套负号
        let frac = scroll_frac(
            f32::from(self.lib_list.scroll_px_offset_for_scrollbar().y),
            max,
        );
        let hot = self.sb_hover.get() == Some("lib");
        let dur = Duration::from_millis(140);
        let (idle, accent) = (pal().thumb, pal().accent);

        let st = self.lib_list.clone();
        let seek = move |track_top: f32, track_h: f32, pointer_y: f32| {
            let max: f32 = st.max_offset_for_scrollbar().height.into();
            let view: f32 = st.viewport_bounds().size.height.into();
            if max <= 0.0 {
                return;
            }
            let r = (view / (view + max)).clamp(0.08, 1.0);
            let thumb_h = track_h * r;
            let rel = (pointer_y - track_top - thumb_h / 2.0)
                .clamp(0.0, (track_h - thumb_h).max(0.0));
            let f = if track_h - thumb_h > 0.0 {
                rel / (track_h - thumb_h)
            } else {
                0.0
            };
            let cur: f32 = (-st.scroll_px_offset_for_scrollbar().y).into();
            st.scroll_by(px(f * max - cur));
        };

        let thumb = div()
            .id("libthumb")
            .absolute()
            .left(px(2.0))
            .right(px(2.0))
            .h(gpui::relative(ratio))
            .top(gpui::relative(frac * (1.0 - ratio)))
            .rounded_full()
            .bg(rgb(idle))
            .with_animation(
                gpui::ElementId::Name(format!("libthumb-{hot}").into()),
                gpui::Animation::new(dur).with_easing(gpui::ease_in_out),
                move |el, t| {
                    let c = if hot {
                        lerp_rgb(idle, accent, t)
                    } else {
                        lerp_rgb(accent, idle, t)
                    };
                    let inset = if hot { 2.0 - t } else { 1.0 + t };
                    el.bg(rgb(c)).left(px(inset)).right(px(inset))
                },
            );

        div()
            .id("libbar")
            .absolute()
            .right(px(2.0))
            .top_0()
            .bottom_0()
            .w(px(10.0))
            .rounded_full()
            .bg(rgb(pal().line2))
            .cursor_pointer()
            .occlude()
            .on_hover(cx.listener(|this, hovered: &bool, _w, cx| {
                let next = if *hovered { Some("lib") } else { None };
                if next.is_none() && cx.has_active_drag() {
                    // 拖动中指针滑出轨道:保留高亮,松手后由 poll 收尾
                    this.sb_sticky.set(Some("lib"));
                    return;
                }
                this.sb_sticky.set(None);
                if this.sb_hover.get() != next
                    && (next.is_some() || this.sb_hover.get() == Some("lib"))
                {
                    this.sb_hover.set(next);
                    cx.notify();
                }
            }))
            // 整条轨道都能拖(不只滑块)
            .on_drag(LibBarDrag, |_, _, _, cx| cx.new(|_| DragGhost))
            .on_drag_move::<LibBarDrag>(cx.listener(move |this, e: &gpui::DragMoveEvent<LibBarDrag>, _w, cx| {
                let track_h: f32 = e.bounds.size.height.into();
                if track_h <= 0.0 {
                    return;
                }
                // 拖动期间保持高亮
                if this.sb_hover.get() != Some("lib") {
                    this.sb_hover.set(Some("lib"));
                }
                seek(
                    f32::from(e.bounds.origin.y),
                    track_h,
                    f32::from(e.event.position.y),
                );
                cx.notify();
            }))
            .child(thumb)
            .into_any_element()
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
                        // 双击行的「其余部分」= 在右侧预览这首(不开播、不打断当前播放);
                        // 双击曲子 = 播放(或按设置加入播放列表);
                        // 展开箭头已 stop_propagation,双击它不会走到这里
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
                        batch: None,
                        batch_diff: BatchDiff::Hardest,
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
                    // 展开/收起和整行的单击/双击要分开:这里吃掉事件并停止冒泡,
                    // 否则双击展开会被整行当成"双击播放",单击也会顺手切预览。
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        move |_ev: &gpui::MouseDownEvent, _w, cx: &mut gpui::App| {
                            // 按下即生效(不等抬起、不做防抖);只 stop_propagation,
                            // 让整行的「单击预览/双击播放」收不到这次点击。
                            cx.stop_propagation();
                            let _ = e4.update(cx, |this, cx| {
                                this.expanded = if this.expanded.as_deref() == Some(sid4.as_str()) {
                                    None
                                } else {
                                    Some(sid4.clone())
                                };
                                this.expanded_row = this.expanded.as_ref().and_then(|id| {
                                    this.rows.iter().position(|r| &r.set_id == id)
                                });
                                cx.notify();
                            });
                        },
                    ),
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
                    .on_click(move |e: &gpui::ClickEvent, _w, cx| {
                        // 连点只认第一次:否则快速双击会把同一首播两遍
                        if e.click_count() > 1 {
                            return;
                        }
                        let _ = e1.update(cx, |this, cx| {
                            // 点歌只开播,不收起点开的难度块(否则刚点完就缩回去)
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
                                batch: None,
                                batch_diff: BatchDiff::Hardest,
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
        // 没在预览时以"真正在放的那首"为准:队列位置可能还停在别的歌上
        self.settings
            .lazer_set
            .clone()
            .or_else(|| self.queue.get(self.pos).map(|q| q.set_id.clone()))
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
            // 用 set_idx 直接定位:这是逐帧跑的,别在几万条谱面集里线性查找
            let set_by_id = self
                .lib
                .as_ref()
                .and_then(|l| self.set_idx.get(id.as_str()).and_then(|i| l.sets.get(*i)));
            if let Some(set) = set_by_id {
                // 优先用实际播放/预览的难度,其次最难
                let want = self
                    .preview
                    .as_ref()
                    .and_then(|(pid, sha)| (pid == id).then(|| sha.clone()).flatten())
                    // 正在放这首:用它真正加载的那个难度(队列位置可能不准)
                    .or_else(|| {
                        self.settings
                            .lazer_sha2
                            .clone()
                            .filter(|_| self.settings.lazer_set.as_deref() == Some(id.as_str()))
                    })
                    .or_else(|| {
                        self.queue
                            .iter()
                            .find(|t| &t.set_id == id)
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
        // 点封面要打开的 osu! 网页:优先当前难度 /b/{bid},没有就谱面集 /s/{sid}
        let (beatmap_online_id, set_online_id) = {
            let mut bid = 0;
            let mut sid = 0;
            if let Some(id) = &set_id {
                if let Some(set) = self
                    .lib
                    .as_ref()
                    .and_then(|l| self.set_idx.get(id.as_str()).and_then(|i| l.sets.get(*i)))
                {
                    sid = set.online_id;
                    let want = self
                        .preview
                        .as_ref()
                        .and_then(|(pid, sha)| (pid == id).then(|| sha.clone()).flatten())
                        .or_else(|| {
                            self.settings.lazer_sha2.clone().filter(|_| {
                                self.settings.lazer_set.as_deref() == Some(id.as_str())
                            })
                        })
                        .or_else(|| {
                            self.queue
                                .iter()
                                .find(|t| &t.set_id == id)
                                .and_then(|t| t.sha2.clone())
                        })
                        .or_else(|| {
                            self.queue
                                .get(self.pos)
                                .filter(|t| &t.set_id == id)
                                .and_then(|_| self.policy_pick(id))
                        });
                    bid = want
                        .and_then(|sha| set.beatmaps.iter().find(|b| b.sha2 == sha))
                        .or_else(|| {
                            set.beatmaps
                                .iter()
                                .max_by(|a, b| a.star_rating.total_cmp(&b.star_rating))
                        })
                        .map(|b| b.online_id)
                        .unwrap_or(0);
                }
            }
            (bid, sid)
        };

        div()
            .id("info-card")
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            // 信息卡占满右栏并在内容超出时滚动:标签等有空间就整段展示,
            // 只有窗口/面板受限时才需要滚动。不显示滚动条(滚轮/拖动照常)。
            .flex_1()
            .min_h(px(0.0))
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
                    .cursor_pointer()
                    .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |this, ev: &gpui::MouseDownEvent, _w, cx| {
                        // 只认单击:双击封面不至于连开两个浏览器标签
                        if ev.click_count > 1 {
                            return;
                        }
                        // 点封面 = 用浏览器打开 osu! 网页(不是"加入/更新播放列表条目")
                        if beatmap_online_id > 0 {
                            cx.open_url(&format!("https://osu.ppy.sh/b/{beatmap_online_id}"));
                        } else if set_online_id > 0 {
                            cx.open_url(&format!("https://osu.ppy.sh/s/{set_online_id}"));
                        } else {
                            this.toast("这首没有 osu! 网页(缺少 online id)");
                        }
                        cx.notify();
                    }))
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
                    // 标签栏不单独限高、不单独滚动:下方有空间就整段铺开;
                    // 空间不够时由外层信息卡统一滚动(滚动只用于受限大小)。
                    div()
                        .id("tags-box")
                        .flex_none()
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
                // 总时长只报估算值:1 小时以内按分钟,超过按“X 小时 Y 分钟”,
                // 不再显示 mm:ss(那是精确进度,不是估算)。
                if self.queue.is_empty() {
                    "队列空闲".to_string()
                } else if total <= 0.0 {
                    format!("{} 首 · 时长未知", self.queue.len())
                } else {
                    let total_mins = (secs + 30) / 60;
                    let est = if total_mins == 0 {
                        "不到 1 分钟".to_string()
                    } else if total_mins < 60 {
                        format!("约 {total_mins} 分钟")
                    } else {
                        let h = total_mins / 60;
                        let m = total_mins % 60;
                        if m == 0 {
                            format!("约 {h} 小时")
                        } else {
                            format!("约 {h} 小时 {m} 分钟")
                        }
                    };
                    format!("{} 首 · {}", self.queue.len(), est)
                }
            })
            // 多选模式时「完成」用主题色高亮,提示当前处于选择状态
            .child({
                let on = self.select_mode;
                let hover_bg = if on { pal().accent } else { pal().ctl_hover };
                div()
                    .id("sel-toggle")
                    .w(px(88.0))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .text_sm()
                    .cursor_pointer()
                    .bg(if on { rgb(pal().accent) } else { rgb(pal().ctl) })
                    .text_color(if on { rgb(0xffffff) } else { rgb(pal().text) })
                    .hover(move |st| st.bg(rgb(hover_bg)))
                    .child(if on { "完成" } else { "多选" })
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        this.commit_num_edit();
                        this.select_mode = !this.select_mode;
                        this.selected.clear();
                        cx.notify();
                    }))
            })
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
            // 保位置:删掉一行不该把列表弹回最顶端
            Self::reset_list_keep_scroll(&self.queue_list, count);
        }
        // 让位预览:队列数组不动,只按 (源, 落点) 生成一份显示顺序。
        // 拖动中的目标位置由指针绝对坐标换算(见 on_drag_move),与行内容
        // 重排无关,所以不会出现「越拖越偏」的反馈错位。
        // 不拖动时不建这个 Vec(逐帧 O(n) 分配没有必要),直接用行号。
        let drag_order: Option<Vec<usize>> = match (self.dragging, self.drag_over) {
            (Some(s0), Some(o)) if s0 < count && o < count && s0 != o => {
                let mut order: Vec<usize> = (0..count).collect();
                let moved = order.remove(s0);
                order.insert(o, moved);
                Some(order)
            }
            _ => None,
        };
        let list = gpui::list(self.queue_list.clone(), move |ix, _w, cx| {
            let i = match &drag_order {
                Some(order) => order.get(ix).copied().unwrap_or(ix),
                None => ix,
            };
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
                    // 标题/艺术家按「以 Unicode 显示」现挑:队列里存的是入队时的文本,
                    // 直接用它的话切换开关不会生效。库里有这首就用库里的原名/unicode 名。
                    let (row_title, row_artist) = match set {
                        Some(st) => (
                            pick_text(&st.title, &st.title_unicode, app.settings.display_unicode),
                            pick_text(&st.artist, &st.artist_unicode, app.settings.display_unicode),
                        ),
                        None => (
                            pick_text(&t.title, &t.title_u, app.settings.display_unicode),
                            pick_text(&t.artist, &t.artist_u, app.settings.display_unicode),
                        ),
                    };
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
                    let e5 = entity.clone();
                    let mods = t.mods;
                    let sid_prev = t.set_id.clone();
                    let sha_prev = t.sha2.clone();
                    div()
                        .id(gpui::ElementId::Name(format!("q{i}").into()))
                        .relative()
                        .w_full()
                        .h(px(QUEUE_ROW_H))
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
                                // 落点用拖动换算出的目标(预览重排后行下标会变,不能用 i);
                                // 源以拖动状态为准,避免陈旧载荷。
                                let from = this.dragging.unwrap_or(src);
                                let target = this.drag_over.unwrap_or(from);
                                if this.settings.play_mode != crate::playlist::PlayMode::Random {
                                    this.move_to(from, target);
                                }
                                this.dragging = None;
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
                        .child({
                            // 多选模式下禁止拖动排序,否则会打乱已选中的条目;
                            // 随机模式下顺序无意义,也不给拖。
                            let can_drag = !app.select_mode
                                && app.settings.play_mode != crate::playlist::PlayMode::Random;
                            let mut handle = div()
                                .id(gpui::ElementId::Name(format!("drag{i}").into()))
                                .w(px(16.0))
                                .h(px(QUEUE_ROW_H))
                                .flex_none()
                                .flex()
                                .items_center()
                                .text_sm()
                                .text_color(rgb(pal().dim))
                                .child(if can_drag { "⠿" } else { " " });
                            if can_drag {
                                handle = handle.cursor_pointer();
                                handle.on_drag(i, {
                                    let entity = entity.clone();
                                    let title = pick_text(&t.title, &t.title_u, app.settings.display_unicode);
                                    let artist = pick_text(&t.artist, &t.artist_u, app.settings.display_unicode);
                                    let cover = app.thumb_of(&t.set_id);
                                    // offset 用不到:让位目标由指针绝对坐标/行高换算,
                                    // 这里只记下源下标与初始落点。
                                    move |idx, _offset, _window, cx| {
                                        let idx = *idx;
                                        let _ = entity.update(cx, |this, cx| {
                                            this.dragging = Some(idx);
                                            this.drag_over = Some(idx);
                                            cx.notify();
                                        });
                                        cx.new(|_| DragPreview {
                                            title: title.clone(),
                                            artist: artist.clone(),
                                            cover: cover.clone(),
                                        })
                                    }
                                })
                            } else {
                                handle
                            }
                        })
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
                                // 正在播的那行用实心圆点标记(不用 emoji 三角)
                                .child(if playing { "●".to_string() } else { format!("{}", i + 1) }),
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
                                                .child(if row_title.is_empty() { "…".to_string() } else { row_title.clone() }),
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
                                            let mut sub = if row_artist.is_empty() {
                                                "曲库加载中".to_string()
                                            } else {
                                                row_artist.clone()
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
                        .into_any_element()
                }
            }
            }
        })
        .px_2()
        // 右侧给滚动条留位置(轨道 10px + 2px 边距),否则「✕」会被它盖住
        .pr(px(18.0))
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
                let n = this.queue.len();
                if n == 0 {
                    return;
                }
                // 指针绝对坐标 → 列表内容坐标:减去列表顶,再加回滚动偏移。
                // 行高固定,取整就是指针下的行号;夹到 [0, n-1] 即落点。
                let top: f32 = e.bounds.origin.y.into();
                let y: f32 = e.event.position.y.into();
                let scrolled: f32 = (-this.queue_list.scroll_px_offset_for_scrollbar().y).into();
                let target = drag_slot(y, top, scrolled, n);
                if this.dragging != Some(src) || this.drag_over != Some(target) {
                    this.dragging = Some(src);
                    this.drag_over = Some(target);
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
        let s = &self.settings;
        let mut collections = vec![("".to_string(), "全部".to_string())];
        collections.extend(self.collections.iter().map(|c| (c.clone(), c.clone())));
        let mut monitors = vec![("".to_string(), "主屏".to_string())];
        monitors.extend(self.monitors.iter().map(|(l, d)| (d.clone(), l.clone())));
        let mut audio = vec![("".to_string(), "系统默认".to_string())];
        audio.extend(self.audio_devs.iter().map(|(id, name)| (id.clone(), name.clone())));
        let mut skins = vec![("".to_string(), "内置 Argon".to_string())];
        skins.extend(self.skins.iter().filter_map(|(label, v)| v.clone().map(|v| (v, label.clone()))));

        #[cfg(windows)]
        let restart_explorer = Some(
            self.small_btn("重启资源管理器", cx, |this, _| {
                std::thread::spawn(|| {
                    if let Err(e) = crate::win::restart_explorer() {
                        log::warn!("[shell] 重启资源管理器失败: {e}");
                    }
                });
                this.toast("正在重启资源管理器…");
            })
            .into_any_element(),
        );
        #[cfg(not(windows))]
        let restart_explorer: Option<gpui::AnyElement> = None;

        div()
            .relative()
            .size_full()
            .child(
        div()
            .id("settings")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.settings_scroll)
            .on_scroll_wheel(cx.listener(|_t, _e, _w, cx| cx.notify()))
            .flex()
            .flex_col()
            .pl(px(12.0))
            .pr(px(26.0))
            .child(section("外观"))
            .child({
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
            })
            .child({
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
            })
            // 曲库/播放条显示原曲名还是 unicode 名(默认开)
            .child(self.toggle_row("以 Unicode 显示", s.display_unicode, "display_unicode", cx))
            // 播放分组的题头:内容整体在画面上面,但保留分组标题
            .child(section("播放"))
            // 曲库源(从曲库读取哪一份数据)
            .child({
                // 用采集好的结果(不逐帧读注册表 / storage.ini)
                let laz_ok = self.probe_realm.is_some();
                let stb_ok = self.probe_stable_root.is_some();
                let btn = |name: &'static str, active: bool, available: bool, cx: &mut Context<Self>| {
                    let mut b = div()
                        .id(gpui::ElementId::Name(format!("src{name}").into()))
                        .px_4()
                        .py_2()
                        .rounded_md()
                        .text_sm()
                        .child(name);
                    if !available {
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
            })
            .child(self.toggle_row("单击封面快速加入", s.quick_add, "quick_add", cx))
            .child(self.toggle_row("双击后添加到播放列表", s.dblclick_add, "dblclick_add", cx))
            // 难度不再单独成行:统一在「默认快速加入选项」里设置
            .child(
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
                                match s.target_star {
                                    None => "最难".to_string(),
                                    Some(t) if t < 0.0 => "最容易".to_string(),
                                    Some(t) => format!("最接近 ★{t:.1}"),
                                }
                            ))
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.quick_opts = true;
                                cx.notify();
                            })),
                    ),
            )
            .child(section("画面"))
            .child(self.dropdown_row(
                "渲染模式",
                DropKind::RenderMode,
                s.render_mode.clone(),
                vec![
                    // 与 Tauri 版 ui/index.html 的 #render-mode 同五项、同文案
                    ("always".into(), "始终渲染".into()),
                    ("autopause".into(), "全屏时不渲染画面".into()),
                    ("fs_pause".into(), "全屏时暂停播放".into()),
                    ("fs_sleep".into(), "全屏时暂停并休眠".into()),
                    ("off".into(), "播放器模式".into()),
                ],
                cx,
            ))
            // 帧率属于画面渲染,放在画面组靠前位置
            .child(self.dropdown_row(
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
            ))
            .child(self.dropdown_row("显示器", DropKind::Monitor, s.monitor.clone().unwrap_or_default(), monitors, cx))
            .child(self.dropdown_row(
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
            ))
            .children(if s.upscale.starts_with("anime4k") {
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
            })
            .child(self.toggle_row("storyboard/视频", s.storyboard, "storyboard", cx))
            .child(self.toggle_row("HD (Hidden)", s.hidden, "hidden", cx))
            .child(self.toggle_row("HUD", s.hud, "hud", cx))
            .child(self.toggle_row("PP 计数", s.pp, "pp", cx))
            .child(self.toggle_row("隐藏游玩画面", s.gameplay_hidden, "gameplay_hidden", cx))
            .child(self.toggle_row("休息段变亮", s.break_lighten, "break_lighten", cx))
            .child(self.toggle_row("减少打击动画", s.reduce_anim, "reduce_anim", cx))
            .child(self.slider_row("背景亮度", "bg_opacity", s.bg_opacity, format!("{:.0}%", s.bg_opacity * 100.0), cx))
            .child(self.toggle_row("滑条渐入", s.snaking_in, "snaking_in", cx))
            .child(self.toggle_row("滑条渐出", s.snaking_out, "snaking_out", cx))
            .child(section("音频"))
            .child(self.toggle_row("打击音效", s.hitsound, "hitsound", cx))
            .child(self.toggle_row("谱面自带音效", s.beatmap_hitsounds, "beatmap_hitsounds", cx))
            .child(self.toggle_row("BGM 淡入淡出", s.fade_audio, "fade_audio", cx))
            .child(self.step_row("音效偏移", format!("{:.0} ms", s.audio_offset_ms), "offset", cx))
            .child(self.slider_row("BGM 音量", "bgm", s.volume, format!("{:.0}%", s.volume * 100.0), cx))
            .child(self.slider_row("打击音效音量", "hits", s.hits_volume, format!("{:.0}%", s.hits_volume * 100.0), cx))
            .child(self.step_row("播放速度", format!("{:.2}x", s.speed), "speed", cx))
            .child(self.slider_row("总音量", "volume", s.master_volume, format!("{:.0}%", s.master_volume * 100.0), cx))
            .child(self.dropdown_row("音频输出", DropKind::AudioDevice, s.audio_device.clone().unwrap_or_default(), audio, cx))
            .child(section("皮肤"))
            .child(self.dropdown_row("皮肤", DropKind::Skin, s.skin.clone().unwrap_or_default(), skins, cx))
            .child(self.toggle_row("强制皮肤颜色", s.force_skin_colours, "force_skin_colours", cx))
            .child(self.toggle_row("光标", s.cursor, "cursor", cx))
            .child(self.toggle_row("光标轨迹", s.cursor_trail, "cursor_trail", cx))
            .child(self.toggle_row("光标波纹", s.cursor_ripples, "cursor_ripples", cx))
            .child(self.step_row("光标大小", format!("{:.2}", s.cursor_size), "cursor_size", cx))
            .child(section("快捷键"))
            .child(self.toggle_row("启用全局快捷键", s.hotkeys_enabled, "hotkeys_enabled", cx))
            .child(self.hotkey_row("上一首", Field::HotkeyPrev, s.hotkey_prev.clone(), cx))
            .child(self.hotkey_row("播放 / 暂停", Field::HotkeyToggle, s.hotkey_toggle.clone(), cx))
            .child(self.hotkey_row("下一首", Field::HotkeyNext, s.hotkey_next.clone(), cx))
            .child(section("系统"))
            .child(self.toggle_row("启动后自动播放", s.play_on_start, "play_on_start", cx))
            .child(self.toggle_row("开机自启", s.autostart, "autostart", cx))
            .child(self.toggle_row("记录日志", s.log_enabled, "log", cx))
            .child(self.toggle_row("调试窗口(下次启动)", s.debug_window, "debug_window", cx))
            .child(self.dropdown_row(
                "关闭窗口",
                DropKind::CloseAction,
                s.close_action.clone().unwrap_or_default(),
                vec![
                    ("".into(), "询问".into()),
                    ("min".into(), "最小化".into()),
                    ("tray".into(), "最小化到托盘".into()),
                    ("exit".into(), "退出".into()),
                ],
                cx,
            ))
            .child(section("路径"))
            .child(self.path_row("osu!lazer 目录", Field::LazerDir, s.lazer_dir.clone(), cx))
            .child(self.path_row("osu!stable 目录", Field::StableDir, s.stable_dir.clone(), cx))
            .child(self.path_row("ffmpeg", Field::Ffmpeg, s.ffmpeg.clone(), cx))
            .child(self.path_row("ffprobe", Field::Ffprobe, s.ffprobe.clone(), cx))
            // 安装包不再内置 ffmpeg:给一个直达下载页的按钮
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .pb_2()
                    .child(self.small_btn("下载 ffmpeg", cx, |_this, cx| {
                        cx.open_url("https://ffmpeg.org/download.html");
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_2()
                    .py_3()
                    .child(self.small_btn("导出日志", cx, |this, _| this.export_log()))
                    .child(self.small_btn("还原桌面壁纸", cx, |this, _| this.restore_wallpaper()))
                    .child(self.small_btn("重启壁纸进程", cx, |this, _| this.restart_child()))
                    // Windows 特有:桌面层次乱了 / 壁纸挂不上去时,重启 shell 重建
                    // Progman / DefView / WorkerW(任务栏会闪一下;壁纸进程自己重挂)。
                    .children(restart_explorer)
                    .child(self.small_btn(
                        format!("清除全部个性设置 ({})", self.settings.set_overrides.len()),
                        cx,
                        |this, _| {
                            if this.settings.set_overrides.is_empty() {
                                this.toast("还没有任何个性设置");
                            } else {
                                this.ov_clear_confirm = true;
                            }
                        },
                    ))
                    .child(self.small_btn("恢复默认设置", cx, |this, _| {
                        this.reset_confirm = true;
                    }))
                    .child(self.danger_btn("退出 Aria", cx, |this, _| {
                        #[cfg(target_os = "linux")]
                        let _ = crate::kde::restore_wallpaper();
                        this.persist();
                        std::process::exit(0);
                    })),

            )

            )
            .child(self.tracked_scrollbar("settings", &self.settings_scroll, cx))
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

    /// 原生文件/目录选择(Windows IFileDialog / Linux XDG portal;后台线程,
    /// 结果在 poll 里落账)。
    fn browse(&mut self, field: Field) {
        // 起始目录用当前值:手填过路径时直接打开到那儿,省得每次从头翻
        let start = match field {
            Field::LazerDir => self.settings.lazer_dir.clone(),
            Field::StableDir => self.settings.stable_dir.clone(),
            _ => None,
        }
        .map(PathBuf::from)
        .filter(|p| p.is_dir());
        #[cfg(windows)]
        let parent = MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed);
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new();
            if let Some(dir) = &start {
                dialog = dialog.set_directory(dir);
            }
            // 有 owner 才会前置/模态;主窗口还没建好时退化成无 owner
            #[cfg(windows)]
            {
                if let Some(h) = std::num::NonZeroIsize::new(parent) {
                    dialog = dialog.set_parent(&RawParent(h));
                }
            }
            let picked = match field {
                Field::Ffmpeg | Field::Ffprobe => dialog.pick_file(),
                _ => dialog.pick_folder(),
            };
            if let Some(p) = picked {
                PICKED.lock().unwrap().push((field, p));
            }
        });
    }

    /// 在 PATH 里找可执行文件,返回第一个命中的完整路径。
    ///
    /// Windows 上要按 `PATHEXT` 补扩展名:命令行里写 `ffmpeg` 时系统会自己
    /// 找到 `ffmpeg.exe`,但 `dir.join("ffmpeg").is_file()` 找不到 —— 之前
    /// 设置页明明 PATH 里有 ffmpeg 却显示 ✗ 就是这个原因。
    fn which_in_path(name: &str) -> Option<PathBuf> {
        let paths = std::env::var_os("PATH")?;
        let mut exts: Vec<String> = vec![String::new()];
        if cfg!(windows) {
            let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
            exts.extend(
                pathext
                    .split(';')
                    .map(str::trim)
                    .filter(|e| !e.is_empty())
                    .map(str::to_ascii_lowercase),
            );
        }
        std::env::split_paths(&paths)
            .filter(|dir| !dir.as_os_str().is_empty())
            .find_map(|dir| {
                exts.iter()
                    .map(|ext| dir.join(format!("{name}{ext}")))
                    .find(|p| p.is_file())
            })
    }
    /// 采集"自动检测"的路径(注册表 / storage.ini / PATH 扫描)。
    ///
    /// 这些探测是真实 I/O,而 gpui 每帧都会重跑一遍 `render`;所以只在
    /// **可能变化时**采集(启动、改路径、换曲库源、刷新曲库、切到设置页),
    /// 而不是逐帧做,也不靠 TTL 过期。
    fn refresh_probes(&mut self) {
        self.probe_lazer_root = crate::lazer::data_root();
        self.probe_stable_root = crate::stable::stable_root();
        self.probe_realm = crate::lazer::realm_path();
        self.probe_ffmpeg = Self::which_in_path("ffmpeg");
        self.probe_ffprobe = Self::which_in_path("ffprobe");
    }

    fn path_ok(&self, field: Field, v: &Option<String>) -> bool {
        match field {
            Field::LazerDir | Field::StableDir => {
                v.as_ref().map(|p| Path::new(p).is_dir()).unwrap_or(true)
            }
            Field::Ffmpeg => v
                .as_deref()
                .map(|p| Path::new(p).is_file())
                .unwrap_or_else(|| self.probe_ffmpeg.is_some()),
            Field::Ffprobe => v
                .as_deref()
                .map(|p| Path::new(p).is_file())
                .unwrap_or_else(|| self.probe_ffprobe.is_some()),
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
        // 重启出来的进程同样要补齐设置(渲染模式/帧率等),否则会退回默认
        if self.child.is_some() {
            self.send_child_settings();
        }
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

    /// 0..1 的横向滑杆(设置页的音量/背景亮度):点击轨道即定位、按住
    /// 拖动自由调值(按 1% 量化);右侧仍有数值输入框可直接键入。
    fn fader(&self, kind: &'static str, frac: f32, width: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let f = frac.clamp(0.0, 1.0);
        // MouseDownEvent 不带元素 bounds,点击定位要在 prepaint 时记下轨道
        // 的绝对位置和宽度(第一个子元素是铺满整条的背景轨)。
        let track_bounds: std::rc::Rc<std::cell::Cell<Option<(f32, f32)>>> =
            std::rc::Rc::new(std::cell::Cell::new(None));
        let bounds_sink = track_bounds.clone();
        let fader = div()
            .relative()
            .w(px(width))
            .h(px(20.0))
            .flex()
            .items_center()
            .on_children_prepainted(move |children, _w, _cx| {
                if let Some(bg) = children.first() {
                    let w: f32 = bg.size.width.into();
                    if w > 0.0 {
                        bounds_sink.set(Some((f32::from(bg.origin.x), w)));
                    }
                }
            })
            .id(gpui::ElementId::Name(format!("fader{kind}").into()));
        fader
            .cursor_pointer()
            .on_mouse_down(gpui::MouseButton::Left, {
                let track_bounds = track_bounds.clone();
                cx.listener(move |this, e: &gpui::MouseDownEvent, _w, cx| {
                    // 先落账正在编辑的数值,避免随后又被输入框里的旧值覆盖
                    this.commit_num_edit();
                    if let Some((x0, w)) = track_bounds.get() {
                        if w > 0.0 {
                            let r = (f32::from(e.position.x) - x0) / w;
                            if this.set_fader_ratio(kind, r) {
                                cx.notify();
                            }
                        }
                    }
                })
            })
            .on_drag(SettingDrag(kind), |_, _, _, cx| cx.new(|_| DragGhost))
            .on_drag_move::<SettingDrag>(cx.listener(move |this, e: &gpui::DragMoveEvent<SettingDrag>, _w, cx| {
                // 所有滑杆共用一个载荷类型:只处理自己那条
                if e.drag(cx).0 != kind {
                    return;
                }
                let w: f32 = e.bounds.size.width.into();
                if w <= 0.0 {
                    return;
                }
                let x: f32 = e.event.position.x.into();
                let r = (x - f32::from(e.bounds.origin.x)) / w;
                if this.set_fader_ratio(kind, r) {
                    cx.notify();
                }
            }))
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(7.0))
                    .h(px(6.0))
                    .rounded_full()
                    .bg(rgb(pal().line)),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(7.0))
                    .h(px(6.0))
                    .w(gpui::relative(f))
                    .rounded_full()
                    .bg(rgb(pal().accent)),
            )
            .child(
                div()
                    .absolute()
                    .top(px(2.0))
                    .left(gpui::relative(f))
                    .ml(px(-8.0))
                    .size(px(16.0))
                    .rounded_full()
                    .bg(rgb(0xffffff))
                    .border_2()
                    .border_color(rgb(pal().accent)),
            )
    }

    /// 滑杆比例 → 1% 量化后的值并落账;返回值是否有变化(没变就不重绘)。
    fn set_fader_ratio(&mut self, kind: &'static str, r: f32) -> bool {
        let v = ((f64::from(r.clamp(0.0, 1.0)) * 100.0).round() / 100.0).clamp(0.0, 1.0);
        if (self.num_value(kind) - v).abs() > 0.004 {
            self.set_num(kind, v);
            true
        } else {
            false
        }
    }

    /// 滑杆 + 数值输入的行(替代 step_row 的 −/+,仍可直接键入数值)。
    fn slider_row(
        &self,
        label: &'static str,
        kind: &'static str,
        frac: f32,
        value: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let field = Self::num_field(kind);
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
                    .items_center()
                    .gap_2()
                    .child(self.fader(kind, frac, 180.0, cx))
                    .child(match field {
                        Some(f) => self
                            .text_field(f, &value, 84.0, cx)
                            .into_any_element(),
                        None => div()
                            .w(px(84.0))
                            .text_xs()
                            .text_color(rgb(pal().dim))
                            .child(value.clone())
                            .into_any_element(),
                    }),
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
        let ok = self.path_ok(field, &value);
        // 未手动指定时显示自动检测到的路径(探测结果带 5s 缓存,不逐帧读注册表/扫 PATH)
        // 未手动指定时显示自动检测到的路径(采集结果,不在这里现场探测)
        let effective = value.clone().or_else(|| match field {
            Field::LazerDir => self
                .probe_lazer_root
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            Field::StableDir => self
                .probe_stable_root
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            // PATH 自动检测:显示命中的完整路径(能看出用的是哪一个),找不到就是 None
            Field::Ffmpeg => self.probe_ffmpeg.as_ref().map(|p| p.display().to_string()),
            Field::Ffprobe => self.probe_ffprobe.as_ref().map(|p| p.display().to_string()),
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
            // 四行按钮同标签、同调用点:必须带 field 当 key,否则只有第一行能点
            .child(self.small_btn_keyed("浏览…", format!("{field:?}"), None, cx, move |this, _| this.browse(field)))
            .child(self.small_btn_keyed("恢复默认", format!("{field:?}"), Some(96.0), cx, move |this, _| {
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

    /// 一行快捷键:说明 + 组合键文本框 + 清除。
    fn hotkey_row(
        &self,
        label: &'static str,
        field: Field,
        value: Option<String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let shown = value.unwrap_or_default();
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
            .child(label)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(self.text_field(field, &shown, 200.0, cx))
                    // id 按 field 区分:三行同标签按钮不能共用交互状态
                    .child(
                        div()
                            .id(gpui::ElementId::Name(format!("hkclear{field:?}").into()))
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(rgb(pal().ctl))
                            .text_sm()
                            .cursor_pointer()
                            .hover(|st| st.bg(rgb(pal().ctl_hover)))
                            .child("清除")
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.commit_num_edit();
                                match field {
                                    Field::HotkeyPrev => this.settings.hotkey_prev = None,
                                    Field::HotkeyToggle => this.settings.hotkey_toggle = None,
                                    Field::HotkeyNext => this.settings.hotkey_next = None,
                                    _ => {}
                                }
                                this.apply_hotkeys();
                                this.persist();
                                cx.notify();
                            })),
                    ),
            )
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
        // MouseDownEvent 不带元素 bounds(只有拖动事件带),点击换算比例要
        // 自己记:第一个子元素是铺满轨道的背景条(left_0/right_0),prepaint
        // 时把它的位置和宽度存下来,按下时就能立刻定位。
        let track_bounds: std::rc::Rc<std::cell::Cell<Option<(f32, f32)>>> =
            std::rc::Rc::new(std::cell::Cell::new(None));
        let track_bounds_sink = track_bounds.clone();
        let mut track = div()
            .relative()
            .h(px(hit_h))
            .flex()
            .items_center()
            .on_children_prepainted(move |children, _w, _cx| {
                if let Some(bg) = children.first() {
                    let w: f32 = bg.size.width.into();
                    if w > 0.0 {
                        track_bounds_sink.set(Some((f32::from(bg.origin.x), w)));
                    }
                }
            })
            .id(id);
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
                // 单击即跳:GPUI 的 on_drag_move 要越过拖动阈值才触发,纯点击
                // 收不到任何回调,所以按下时直接用记录的轨道宽度换算。
                .on_mouse_down(gpui::MouseButton::Left, {
                    let track_bounds = track_bounds.clone();
                    cx.listener(move |this, e: &gpui::MouseDownEvent, _w, cx| {
                        if let Some((x0, w)) = track_bounds.get() {
                            let r = (f32::from(e.position.x) - x0) / w;
                            if this.seek_to_frac(r) {
                                cx.notify();
                            }
                        }
                    })
                })
                .on_drag(SeekDrag, |_, _, _, cx| cx.new(|_| DragGhost))
                .on_drag_move::<SeekDrag>(cx.listener(|this, e: &gpui::DragMoveEvent<SeekDrag>, _w, cx| {
                    let w: f32 = e.bounds.size.width.into();
                    if w <= 0.0 {
                        return;
                    }
                    let x: f32 = e.event.position.x.into();
                    let r = (x - f32::from(e.bounds.origin.x)) / w;
                    if this.seek_to_frac(r) {
                        cx.notify();
                    }
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

    /// 点击播放条封面:如果"正在放的那首"在播放列表里,就切到列表并定位到它;
    /// 不在列表里(曲库直接点播)则什么都不做 —— 不跳视图、不滚动、不清预览。
    fn focus_playing_in_queue(&mut self) {
        let Some(set_id) = self.settings.lazer_set.clone() else {
            return;
        };
        let sha = self.settings.lazer_sha2.clone();
        // 同一个谱面集可能在列表里出现多次(不同难度/mods):优先 set+难度 精确匹配
        let idx = sha
            .as_ref()
            .and_then(|s| {
                self.queue
                    .iter()
                    .position(|q| q.set_id == set_id && q.sha2.as_ref() == Some(s))
            })
            .or_else(|| self.queue.iter().position(|q| q.set_id == set_id));
        let Some(i) = idx else {
            return;
        };
        self.pos = i;
        self.settings.playlist_pos = i;
        self.preview = None;
        self.view = View::Queue;
        // 普通模式:只滚到当前曲目,不进入多选、也不自动勾选
        self.select_mode = false;
        self.selected.clear();
        self.sel_anchor = None;
        self.ctx = None;
        self.ctx_multi = false;
        self.persist();
        // 先用当前队列长度同步 item 数,否则下面设置的定位会被 reset 清掉
        let count = self.queue.len();
        if self.queue_list.item_count() != count {
            self.queue_list.reset(count);
        }
        self.queue_list.scroll_to(gpui::ListOffset {
            item_ix: i,
            offset_in_item: px(0.0),
        });
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
            // 新曲还没报回状态(正在 load)时不要本地插值,否则进度条
            // 会在真正开始播放前先自己走一段。
            if self.playing && !self.awaiting_start {
                let elapsed = self.prog_at.elapsed().as_secs_f64();
                t += elapsed * f64::from(self.settings.speed).max(0.25);
                if dur > 0.0 {
                    t = t.min(dur);
                }
            }
            (t, dur)
        };
        let frac = if dur > 0.0 { (t / dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
        // 播放条只反映"实际在放的那首":
        // 以真正下发 load 的目标为准(settings.lazer_set / lazer_sha2),
        // 队列只是它的一个来源 —— 从曲库直接点播时,队列位置可能还停在别的歌上。
        let playing_set = self
            .settings
            .lazer_set
            .clone()
            .or_else(|| self.queue.get(self.pos).map(|q| q.set_id.clone()));
        let playing_sha = self.settings.lazer_sha2.clone().or_else(|| {
            self.queue
                .iter()
                .find(|q| Some(&q.set_id) == playing_set.as_ref())
                .and_then(|q| q.sha2.clone())
        });
        // 难度显示名:优先查库,库还没加载完就用上次记下的名字(绝不显示 sha2)
        let playing_diff_name = playing_sha
            .as_ref()
            .and_then(|sha| {
                let set = self.lib.as_ref().and_then(|l| {
                    self.set_idx
                        .get(playing_set.as_deref()?)
                        .and_then(|i| l.sets.get(*i))
                })?;
                set.beatmaps.iter().find(|b| &b.sha2 == sha).map(|b| b.name.clone())
            })
            .or_else(|| self.settings.lazer_diff.clone());
        let now = {
            // 优先用库里的原名/unicode 名现挑(队列里存的是入队时的文本)
            let unicode = self.settings.display_unicode;
            let from_lib = playing_set.as_ref().and_then(|id| {
                self.all_rows.iter().find(|r| &r.set_id == id).map(|r| {
                    format!(
                        "{} - {}",
                        pick_text(&r.title_raw, &r.title_u, unicode),
                        pick_text(&r.artist_raw, &r.artist_u, unicode)
                    )
                })
            });
            from_lib
                .or_else(|| {
                    playing_set
                        .as_ref()
                        .and_then(|id| self.queue.iter().find(|q| &q.set_id == id))
                        .map(|q| {
                            format!(
                                "{} - {}",
                                pick_text(&q.title, &q.title_u, unicode),
                                pick_text(&q.artist, &q.artist_u, unicode)
                            )
                        })
                })
                .filter(|t| t != " - ")
                .unwrap_or_else(|| "未播放".to_string())
        };

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
                    .child(self.icon_btn("prev", TransportIcon::Prev, false, cx, |this, _| this.step(-1)))
                    .child(self.icon_btn(
                        "toggle",
                        if self.playing { TransportIcon::Pause } else { TransportIcon::Play },
                        true,
                        cx,
                        |this, _| this.toggle_pause(),
                    ))
                    .child(self.icon_btn("next", TransportIcon::Next, false, cx, |this, _| this.step(1)))
                    .child({
                        // 难度框的"主体":预览优先(预览时就是想在播放条挑难度开播),
                        // 没有预览时才是正在放的那首。曲库预览的曲目还没进播放列表,
                        // 以前这里会因为没有队列行而一个难度都列不出来。
                        let ui_set = self
                            .preview
                            .as_ref()
                            .map(|(id, _)| id.clone())
                            .or_else(|| playing_set.clone());
                        let previewing = self.preview.is_some();
                        let ui_sha = if previewing {
                            self.preview.as_ref().and_then(|(_, s)| s.clone())
                        } else {
                            playing_sha.clone()
                        };
                        let cur_set = ui_set.as_ref().and_then(|id| {
                            self.lib
                                .as_ref()
                                .and_then(|l| self.set_idx.get(id.as_str()).and_then(|i| l.sets.get(*i)))
                        });
                        let (mut opts, cur) = match cur_set {
                            Some(set) => {
                                let mut bs: Vec<&crate::lazer::LazerBeatmap> =
                                    set.beatmaps.iter().collect();
                                bs.sort_by(|a, b| a.star_rating.total_cmp(&b.star_rating));
                                let opts: Vec<(String, String)> = bs
                                    .iter()
                                    .map(|b| {
                                        (b.sha2.clone(), format!("{} ★{:.2}", b.name, b.star_rating))
                                    })
                                    .collect();
                                let cur = ui_sha
                                    .clone()
                                    .or_else(|| {
                                        self.queue
                                            .iter()
                                            .find(|q| Some(&q.set_id) == ui_set.as_ref())
                                            .and_then(|q| q.sha2.clone())
                                    })
                                    .or_else(|| ui_set.as_ref().and_then(|id| self.policy_pick(id)))
                                    .unwrap_or_default();
                                (opts, cur)
                            }
                            None => (Vec::new(), ui_sha.clone().unwrap_or_default()),
                        };
                        opts.truncate(60);
                        // 库还没就绪(=这一档不在选项里)时,别把 sha2 当难度名显示
                        if !cur.is_empty() && !opts.iter().any(|(v, _)| v == &cur) {
                            // 预览时不能借用"正在播的那首"的难度名,会串台
                            let name = if previewing {
                                "…".to_string()
                            } else {
                                playing_diff_name.clone().unwrap_or_else(|| "…".to_string())
                            };
                            opts.insert(0, (cur.clone(), name));
                        }
                        // 播放条难度框固定长度(240px):不随难度名长短伸缩,
                        // 超长名字单行省略,避免把这一排其它控件挤走。
                        self.dropdown_w(DropKind::QueueDiff, cur, opts, Some(240.0), cx)
                    })
                    .child({
                        // 封面同样只跟"实际在放的那首",预览不参与
                        let cur = playing_set.clone();
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
                            .id("pb-cover")
                            .w(px(34.0))
                            .h(px(34.0))
                            .flex_none()
                            .overflow_hidden()
                            .cursor_pointer()
                            // 点封面:只有"正在放的这首确实在播放列表里"才跳过去定位;
                            // 曲库直接点播(不在列表里)就什么都不做
                            .child(inner)
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.focus_playing_in_queue();
                                cx.notify();
                            }))
                    })
                    // 题头:给一个最小宽度,窗口窄时也不会被难度框/音量挤没
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.0))
                            .overflow_hidden()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.0))
                                            .text_sm()
                                            .whitespace_nowrap()
                                            .truncate()
                                            .overflow_hidden()
                                            .text_color(rgb(pal().text))
                                            .child(now),
                                    ),
                            ),
                    )
                    // 倍率跟在封面 + 题头后面(原先排在难度框和封面之间)
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
                    .child(div().text_xs().text_color(rgb(pal().dim)).child("音量"))
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
        let is_batch = dlg.batch.is_some();
        let is_bulk = dlg.bulk.is_some();
        // 批量导入 / 多选批量编辑:难度在弹窗里一起选
        let show_diff = is_batch || is_bulk;
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
                        // HR↔EZ、DT/HT/NC 互斥:同组再选会自动取消冲突项
                        d.mods = toggle_mod(d.mods, bit);
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
            // 模态遮罩:挡住下层控件,避免点击穿透
            .occlude()
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
                    .child(div().text_lg().child(if is_batch {
                        "加入播放列表"
                    } else if is_bulk {
                        "编辑所选条目(mods / 难度)"
                    } else if dlg.queue_index.is_some() {
                        "编辑条目(mods / 难度)"
                    } else {
                        "加入播放列表"
                    }))
                    .child(div().text_sm().text_color(rgb(pal().dim)).child(if let Some(n) = dlg.batch {
                        format!("{n} 个谱面集将加入播放列表(已在列表中的会更新)")
                    } else if let Some(sel) = &dlg.bulk {
                        format!("已选 {} 首", sel.len())
                    } else {
                        title
                    }))
                    .children(if show_diff {
                        // 批量跨谱面集:没有单一难度可选(按策略为每首挑一档)
                        None
                    } else {
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
                    .children(if show_diff {
                        let diff = dlg.batch_diff;
                        let diff_btn = |label: &'static str, on: bool, val: BatchDiff, cx: &mut Context<Self>| {
                            div()
                                .id(gpui::ElementId::Name(format!("bd{label}").into()))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_xs()
                                .cursor_pointer()
                                .hover(|st| st.bg(rgb(pal().ctl_hover)))
                                .bg(if on { rgb(pal().accent) } else { rgb(pal().ctl) })
                                .text_color(if on { rgb(0xffffff) } else { rgb(pal().text) })
                                .child(label)
                                .on_click(cx.listener(move |this, _e, _w, cx| {
                                    if let Some(d) = this.mods_dialog.as_mut() {
                                        d.batch_diff = val;
                                    }
                                    cx.notify();
                                }))
                        };
                        let custom_star = match diff {
                            BatchDiff::Custom(t) => t,
                            _ => self.settings.custom_star_value(),
                        };
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
                                        .items_center()
                                        .gap_1()
                                        .child(diff_btn("最难", diff == BatchDiff::Hardest, BatchDiff::Hardest, cx))
                                        .child(diff_btn("最容易", diff == BatchDiff::Easiest, BatchDiff::Easiest, cx))
                                        .child(diff_btn(
                                            "自定义星级",
                                            matches!(diff, BatchDiff::Custom(_)),
                                            BatchDiff::Custom(custom_star),
                                            cx,
                                        ))
                                        .children(if let BatchDiff::Custom(t) = diff {
                                            Some(
                                                self.text_field(
                                                    Field::NumBatchStar,
                                                    &format!("★{t:.1}"),
                                                    84.0,
                                                    cx,
                                                )
                                                .into_any_element(),
                                            )
                                        } else {
                                            None
                                        }),
                                ),
                        )
                    } else {
                        None
                    })
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .child(self.small_btn("取消", cx, |this, _| this.mods_dialog = None))
                            .child(self.small_btn(if is_batch { "加入" } else { "确定" }, cx, |this, _| {
                                if let Some(d) = this.mods_dialog.take() {
                                    if d.batch.is_some() {
                                        this.add_filtered_with_mods(d.mods, d.batch_diff);
                                        return;
                                    }
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
                                                // 多选:mods + 按所选难度策略为每首挑一档
                                                let policy = match d.batch_diff {
                                                    BatchDiff::Hardest => {
                                                        crate::playlist::PickPolicy::Hardest
                                                    }
                                                    BatchDiff::Easiest => {
                                                        crate::playlist::PickPolicy::Easiest
                                                    }
                                                    BatchDiff::Custom(t) => {
                                                        crate::playlist::PickPolicy::Custom(t)
                                                    }
                                                };
                                                for i in &sel {
                                                    let sha = this
                                                        .queue
                                                        .get(*i)
                                                        .map(|t| t.set_id.clone())
                                                        .and_then(|id| this.pick_with_policy(&id, policy));
                                                    if let Some(t) = this.queue.get_mut(*i) {
                                                        t.mods = d.mods;
                                                        if let Some(sha) = sha {
                                                            t.sha2 = Some(sha);
                                                        }
                                                    }
                                                }
                                                let playing = sel.contains(&this.pos);
                                                this.save_queue();
                                                if playing {
                                                    this.play_index(this.pos);
                                                }
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
            // 模态遮罩:挡住下层控件,避免点击穿透
            .occlude()
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
                                // 记住用户设置:写回 settings 并立即落盘。
                                // 旧实现只改内存,重启后星级过滤又回到旧值。
                                this.settings.star_min = this.star_min;
                                this.settings.star_max = Some(this.star_max);
                                this.star_popup = false;
                                this.apply_filter();
                                this.persist();
                            })),
                    ),
            )
    }

    /// 默认快速加入选项:mods + 默认难度(含自定义星级)。
    fn quick_opts_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = &self.settings;
        let mods = s.quick_mods;
        let star = s.custom_star_value();
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
                    // 互斥规则与导入对话框一致
                    this.settings.quick_mods = toggle_mod(this.settings.quick_mods, bit);
                    this.persist();
                    cx.notify();
                }))
        };
        let policy = s.quick_policy.clone();
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
                    this.commit_num_edit();
                    this.settings.quick_policy = value.to_string();
                    // 「难度」只有一处:快速加入的默认难度就是全局难度
                    this.settings.target_star = match value {
                        "easiest" => Some(-1.0),
                        "custom" => Some(this.settings.custom_star_value()),
                        _ => None,
                    };
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
            // 模态遮罩:挡住下层控件,避免点击穿透
            .occlude()
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
                    .child(div().text_xs().text_color(rgb(pal().dim)).child("仅影响快速加入的难度策略"))
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
                                    .child(if policy == "custom" {
                                        self.text_field(
                                            Field::NumStar,
                                            &format!("★{:.1}", star),
                                            84.0,
                                            cx,
                                        )
                                        .into_any_element()
                                    } else {
                                        div()
                                            .w(px(84.0))
                                            .text_sm()
                                            .text_color(rgb(pal().dim))
                                            .child(format!("★{:.1}", star))
                                            .into_any_element()
                                    }),
                            ),
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
            // 模态遮罩:挡住下层控件,避免点击穿透
            .occlude()
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
                            .child("最小化到托盘会收起窗口并放掉封面缓存,后台继续播放。退出会还原桌面壁纸。"),
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
                            .child(self.small_btn("取消", cx, |this, _| {
                                this.close_ask = false;
                                #[cfg(windows)]
                                crate::win::set_ask_open(false);
                            }))
                            .child(self.small_btn("最小化到托盘", cx, |this, cx| {
                                if this.close_remember {
                                    this.settings.close_action = Some("tray".to_string());
                                    sync_close_mode(&this.settings);
                                    this.persist();
                                }
                                this.close_ask = false;
                                this.hide_to_tray(cx);
                            }))
                            .child(self.danger_btn("退出", cx, |this, _| {
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
            // 模态遮罩:挡住下层控件,避免点击穿透
            .occlude()
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
                            .child(self.danger_btn("清空", cx, |this, _| {
                                this.queue.clear();
                                this.selected.clear();
                                this.pos = 0;
                                this.save_queue();
                                this.clear_confirm = false;
                            })),
                    ),
            )
    }

    /// 清除全部个性设置的确认框。
    fn ov_clear_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000aa))
            .occlude()
            .child(
                div()
                    .w(px(420.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(pal().panel))
                    .border_1()
                    .border_color(rgb(pal().line))
                    .child(div().text_lg().child("清除全部个性设置?"))
                    .child(div().text_sm().text_color(rgb(pal().dim)).child(format!(
                        "将删除全部 {} 个谱面集的个性设置,恢复跟随全局,不可撤销。",
                        self.settings.set_overrides.len()
                    )))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .child(self.small_btn("取消", cx, |this, _| this.ov_clear_confirm = false))
                            .child(self.danger_btn("清除", cx, |this, _| {
                                this.settings.set_overrides.clear();
                                this.ov_clear_confirm = false;
                                this.persist();
                                this.toast("已清除全部个性设置");
                            })),
                    ),
            )
    }

    /// 恢复默认设置的确认框。
    fn reset_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000aa))
            .occlude()
            .child(
                div()
                    .w(px(480.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(pal().panel))
                    .border_1()
                    .border_color(rgb(pal().line))
                    .child(div().text_lg().child("恢复默认设置?"))
                    .child(div().text_sm().text_color(rgb(pal().dim)).child(
                        "外观、画面、音频、播放、皮肤、系统与路径设置都会恢复默认值;播放列表与谱面集个性设置保持不变。此操作不可撤销。",
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .child(self.small_btn("取消", cx, |this, _| this.reset_confirm = false))
                            .child(self.danger_btn("恢复默认", cx, |this, _| this.reset_settings())),
                    ),
            )
    }

    /// 队列右键菜单(单选/多选)。
    fn ctx_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (x, y) = self.ctx_pos;
        let n_sel = self.selected.len();
        let mut items: Vec<gpui::AnyElement> = Vec::new();
        if self.ctx_multi {
            // 多选:mods 与难度放在同一个弹窗里选(与「全部加入」一致)
            items.push(self.ctx_item(format!("选择 mods / 难度…({} 首)", n_sel), "bulk_mods", cx).into_any_element());
            items.push(self.ctx_item(format!("移除({} 首)", n_sel), "bulk_remove", cx).into_any_element());
        } else {
            items.push(self.ctx_item("修改…(mods / 难度)".to_string(), "mods", cx).into_any_element());
            items.push(self.ctx_item("个性设置…".to_string(), "override", cx).into_any_element());
            items.push(self.ctx_item("移除".to_string(), "remove", cx).into_any_element());
        }
        // 记录菜单实际 bounds:只有点在菜单外才关闭。否则全屏遮罩会在
        // 鼠标按下时先关菜单,菜单项永远点不中。
        let menu_bounds: std::rc::Rc<std::cell::Cell<Option<(f32, f32, f32, f32)>>> =
            std::rc::Rc::new(std::cell::Cell::new(None));
        let bounds_sink = menu_bounds.clone();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            // 遮挡下层:右键菜单打开时点任何地方都不应触发下面的按钮
            .block_mouse_except_scroll()
            .on_children_prepainted(move |children, _w, _cx| {
                if let Some(b) = children.first() {
                    let x0: f32 = b.origin.x.into();
                    let y0: f32 = b.origin.y.into();
                    bounds_sink.set(Some((
                        x0,
                        y0,
                        x0 + f32::from(b.size.width),
                        y0 + f32::from(b.size.height),
                    )));
                }
            })
            .id("ctx-layer")
            .on_mouse_down(gpui::MouseButton::Left, {
                let menu_bounds = menu_bounds.clone();
                cx.listener(move |this, e: &gpui::MouseDownEvent, _w, cx| {
                    let inside = menu_bounds.get().is_some_and(|(x0, y0, x1, y1)| {
                        let px: f32 = e.position.x.into();
                        let py: f32 = e.position.y.into();
                        px >= x0 && px <= x1 && py >= y0 && py <= y1
                    });
                    if inside {
                        return;
                    }
                    this.ctx = None;
                    this.ctx_multi = false;
                    cx.notify();
                })
            })
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
            // 移除类操作标红
            .text_color(if action == "remove" || action == "bulk_remove" {
                rgb(pal().danger_hover)
            } else {
                rgb(pal().text)
            })
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
                                batch: None,
                                batch_diff: BatchDiff::Hardest,
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
                                    batch: None,
                                    batch_diff: this.default_batch_diff(),
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
        let enabled = ed.ov.enabled;
        let text_c = if enabled { rgb(pal().text) } else { rgb(pal().dim) };
        let tri_opts = vec![
            ("global".to_string(), "跟随全局".to_string()),
            ("on".to_string(), "开".to_string()),
            ("off".to_string(), "关".to_string()),
        ];
        let tri_label = |v: Option<bool>| match v {
            None => "跟随全局".to_string(),
            Some(true) => "开".to_string(),
            Some(false) => "关".to_string(),
        };
        // 三态项:开启时是下拉;关闭个性设置时整行置灰、不可点
        let tri_row = |label: &'static str, kind: DropKind, value: Option<bool>, cx: &mut Context<Self>| {
            let row = div()
                .flex()
                .flex_row()
                .justify_between()
                .items_center()
                .py_1()
                .text_sm()
                .text_color(text_c)
                .child(label);
            if enabled {
                row.child(self.dropdown(kind, tri_key(value), tri_opts.clone(), cx))
            } else {
                row.opacity(0.45)
                    .child(div().px_2().py_1().rounded_md().bg(rgb(pal().ctl)).child(tri_label(value)))
            }
        };
        let offset = ed.ov.audio_offset_ms.unwrap_or(0.0);
        let bg = ed.ov.bg_opacity.unwrap_or(0.7).clamp(0.0, 1.0);
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000aa))
            .occlude()
            .child(
                div()
                    .w(px(540.0))
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
                    // 启用开关:开 = 强调色,关 = 普通灰
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .items_center()
                            .py_1()
                            .text_sm()
                            .child("启用个性设置")
                            .child(
                                div()
                                    .id("ov-enable")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(if enabled { rgb(pal().accent) } else { rgb(pal().ctl) })
                                    .text_color(if enabled {
                                        rgb(0xffffff)
                                    } else {
                                        rgb(pal().dim)
                                    })
                                    .child(if enabled { "开" } else { "关" })
                                    .on_click(cx.listener(|this, _e, _w, cx| {
                                        if let Some(ed) = this.overrides.as_mut() {
                                            ed.ov.enabled = !ed.ov.enabled;
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(tri_row("storyboard/视频", DropKind::OvStoryboard, ed.ov.storyboard, cx))
                    .child(tri_row("打击音效", DropKind::OvHitsound, ed.ov.hitsound, cx))
                    .child(tri_row("谱面自带音效", DropKind::OvBeatmapHitsounds, ed.ov.beatmap_hitsounds, cx))
                    .child(tri_row("隐藏游玩画面", DropKind::OvGameplayHidden, ed.ov.gameplay_hidden, cx))
                    // 音效偏移:直接输入数值
                    .child({
                        let row = div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .items_center()
                            .py_1()
                            .text_sm()
                            .text_color(text_c)
                            .child("音效偏移");
                        if enabled {
                            row.child(self.text_field(
                                Field::OvOffset,
                                &format!("{:.0} ms", offset),
                                96.0,
                                cx,
                            ))
                        } else {
                            row.opacity(0.45).child(
                                div()
                                    .px_2()
                                    .rounded_md()
                                    .bg(rgb(pal().ctl))
                                    .child(format!("{:.0} ms", offset)),
                            )
                        }
                    })
                    // 背景亮度:滑杆 + 数值输入
                    .child({
                        let row = div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .items_center()
                            .py_1()
                            .text_sm()
                            .text_color(text_c)
                            .child("背景亮度");
                        if enabled {
                            row.child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .child(self.fader("ov_bg", bg, 180.0, cx))
                                    .child(self.text_field(
                                        Field::OvBg,
                                        &format!("{:.0}%", bg * 100.0),
                                        84.0,
                                        cx,
                                    )),
                            )
                        } else {
                            row.opacity(0.45).child(
                                div()
                                    .px_2()
                                    .rounded_md()
                                    .bg(rgb(pal().ctl))
                                    .child(format!("{:.0}%", bg * 100.0)),
                            )
                        }
                    })
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
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
                        cx.open_url(url);
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
            // 模态遮罩:挡住下层控件,避免点击穿透
            .occlude()
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
                                cx.open_url("https://github.com/Ohdmire/Aria");
                                cx.notify();
                            })),
                    )
                    .child(scroll_section("使用的开源库", oss.iter().map(|(n, u, d)| item(n, u, d, cx).into_any_element()).collect(), "oss"))
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
                                        cx.open_url("https://afdian.com/a/ATRI1024");
                                        cx.notify();
                                    })),
                            ),
                    )
                    // 感谢名单放在赞助下面:限高滚动区,名单再长也不会把
                    // 关于窗口撑高。
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xs().text_color(rgb(0x6a6a75)).child("感谢名单"))
                            .child(
                                div()
                                    .relative()
                                    .w_full()
                                    .child(
                                        div()
                                            .id("about-thanks")
                                            .max_h(px(120.0))
                                            .pr(px(12.0))
                                            .overflow_y_scroll()
                                            .track_scroll(&self.about_thanks_scroll)
                                            .on_scroll_wheel(cx.listener(|_t, _e, _w, cx| cx.notify()))
                                            .flex()
                                            .flex_col()
                                            .gap_1()
                                            .children(
                                                thanks
                                                    .iter()
                                                    .map(|(n, u, d)| item(n, u, d, cx).into_any_element())
                                                    .collect::<Vec<_>>(),
                                            ),
                                    )
                                    .child(self.tracked_scrollbar("thanks", &self.about_thanks_scroll, cx)),
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
    // 开机自启的命令行带 --hidden:不弹出主窗口,只留托盘。
    let start_hidden = std::env::args().any(|a| a == "--hidden");
    Application::new().run(move |cx: &mut gpui::App| {
        let options = WindowOptions {
            // 用最大化:窗口尺寸随屏幕,不会再出现窗口比屏幕大/跑出屏幕
            window_bounds: Some(gpui::WindowBounds::Maximized(gpui::Bounds {
                origin: gpui::point(px(40.), px(40.)),
                size: gpui::size(px(1100.), px(640.)),
            })),
            titlebar: None,
            window_decorations: Some(gpui::WindowDecorations::Client),
            // Wayland 托盘恢复窗口需要用 app_id 申请激活令牌。
            app_id: Some("com.ohdmire.aria".into()),
            show: !start_hidden,
            focus: !start_hidden,
            ..Default::default()
        };
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
            // 记下主窗口 HWND:原生文件对话框拿它当 owner(否则可能弹在窗口后面)
            #[cfg(windows)]
            {
                if let Ok(h) = raw_window_handle::HasWindowHandle::window_handle(&*window) {
                    if let raw_window_handle::RawWindowHandle::Win32(w32) = h.as_raw() {
                        let raw = w32.hwnd.get();
                        MAIN_HWND.store(raw, std::sync::atomic::Ordering::Relaxed);
                        crate::win::watch_ask(raw);
                    }
                }
            }
            let view = cx.new(App::new);
            let close_view = view.clone();
            window.on_window_should_close(cx, move |window, cx| {
                on_should_close(window, cx, &close_view)
            });
            view_slot = Some(view.clone());
            view
        }) {
            if start_hidden {
                // Windows:窗口创建时就不显示。Linux:最小化,和「最小化到托盘」同一行为。
                #[cfg(windows)]
                crate::win::block_ask();
                #[cfg(not(windows))]
                let _ = handle.update(cx, |_, window, _| window.minimize_window());
            } else {
                let _ = handle.update(cx, |_, window, _| window.activate_window());
            }
            *WINDOW.lock().unwrap() = Some(handle);
        }
        // 兜底轮询:窗口最小化/被遮挡时 gpui 不再出帧,render 里的 poll 也就不会跑,
        // 曲终自动切歌/进度/曲库结果都会停。这里只做"后台兜底 + 有变化才 notify",
        // 它不限制帧率(界面刷新是逐帧的,见 render 里的 request_animation_frame)。
        // 托盘:gpui-tray 的菜单项直接分发 gpui Action,这里把动作接到 App 方法上。
        if let Some(view) = view_slot.clone() {
            let weak = view.downgrade();
            {
                let w = weak.clone();
                cx.on_action(move |_: &crate::tray::ShowWindow, cx: &mut gpui::App| {
                    let _ = w.update(cx, |this, cx| {
                        this.activate_window(cx);
                        cx.notify();
                    });
                });
            }
            {
                let w = weak.clone();
                cx.on_action(move |_: &crate::tray::PrevTrack, cx: &mut gpui::App| {
                    let _ = w.update(cx, |this, cx| {
                        this.step(-1);
                        cx.notify();
                    });
                });
            }
            {
                let w = weak.clone();
                cx.on_action(move |_: &crate::tray::TogglePlay, cx: &mut gpui::App| {
                    let _ = w.update(cx, |this, cx| {
                        this.toggle_pause();
                        cx.notify();
                    });
                });
            }
            {
                let w = weak.clone();
                cx.on_action(move |_: &crate::tray::NextTrack, cx: &mut gpui::App| {
                    let _ = w.update(cx, |this, cx| {
                        this.step(1);
                        cx.notify();
                    });
                });
            }
            cx.on_action(move |_: &crate::tray::QuitApp, _cx: &mut gpui::App| {
                #[cfg(target_os = "linux")]
                let _ = crate::kde::restore_wallpaper();
                std::process::exit(0);
            });
            crate::tray::init(cx);
        }
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

#[cfg(test)]
mod drag_slot_tests {
    use super::*;

    /// 指针 → 行号:边界、滚动、越界夹紧。拖动错位就栽在这套换算上。
    #[test]
    fn pointer_maps_to_visible_row() {
        let top = 100.0;
        // 无滚动:行边界 100 / 184 / 268 …
        assert_eq!(drag_slot(100.0, top, 0.0, 5), 0);
        assert_eq!(drag_slot(183.9, top, 0.0, 5), 0);
        assert_eq!(drag_slot(184.0, top, 0.0, 5), 1);
        assert_eq!(drag_slot(268.0, top, 0.0, 5), 2);
        // 滚动 2 行:可视区第一行是队列第 2 行
        assert_eq!(drag_slot(100.0, top, 2.0 * QUEUE_ROW_H, 5), 2);
        assert_eq!(drag_slot(100.0 + QUEUE_ROW_H, top, 2.0 * QUEUE_ROW_H, 5), 3);
        // 拖出列表上下边界:夹到首/尾
        assert_eq!(drag_slot(0.0, top, 0.0, 5), 0);
        assert_eq!(drag_slot(10_000.0, top, 0.0, 5), 4);
        // 空列表不 panic
        assert_eq!(drag_slot(150.0, top, 0.0, 0), 0);
    }

    /// 滚动条滑块位置:gpui 的 offset 是负值,顶部 0 / 底部 -max。
    /// 之前按正值算,比例恒为 0 → 滑块永远停在顶端。
    #[test]
    fn scrollbar_frac_uses_negative_offset() {
        assert_eq!(scroll_frac(0.0, 200.0), 0.0, "顶部");
        assert_eq!(scroll_frac(-50.0, 200.0), 0.25);
        assert_eq!(scroll_frac(-100.0, 200.0), 0.5);
        assert_eq!(scroll_frac(-200.0, 200.0), 1.0, "底部");
        assert_eq!(scroll_frac(-400.0, 200.0), 1.0, "越界夹到 1");
        assert_eq!(scroll_frac(-10.0, 0.0), 0.0, "不可滚动");
    }

    /// 快捷键捕获:gpui 的按键事件 → 快捷键文本,且必须能被真正注册用的解析器接受。
    #[test]
    fn hotkey_capture_maps_keystrokes() {
        let chord = |s: &str| {
            let ks = gpui::Keystroke::parse(s).expect("keystroke 应能解析");
            hotkey_chord_text(&ks)
        };
        assert_eq!(chord("ctrl-alt-right").as_deref(), Some("Ctrl+Alt+Right"));
        assert_eq!(chord("f8").as_deref(), Some("F8"));
        assert_eq!(chord("shift-alt-a").as_deref(), Some("Alt+Shift+A"));
        assert_eq!(chord("ctrl").as_deref(), None, "只按修饰键不产生组合");
        assert_eq!(chord("shift").as_deref(), None, "只按修饰键不产生组合");

        #[cfg(windows)]
        for s in ["ctrl-alt-right", "f8", "ctrl-shift-f5", "alt-space", "ctrl-a"] {
            let text = chord(s).unwrap_or_else(|| panic!("{s} 应产生组合"));
            crate::hotkeys::parse_binding(crate::hotkeys::Action::Next, &text)
                .unwrap_or_else(|e| panic!("捕获到 {text} 但解析失败: {e}"));
        }
    }
}

#[cfg(test)]
mod shuffle_tests {
    use super::*;

    /// 洗牌必须是完整排列:每个下标恰好出现一次。
    #[test]
    fn shuffle_is_a_permutation() {
        for n in [0usize, 1, 2, 3, 17, 250] {
            let mut rng = 0x1234_5678_9ABC_DEF1u64;
            let order = shuffled_indices(&mut rng, n, None);
            assert_eq!(order.len(), n);
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..n).collect::<Vec<_>>());
        }
    }

    /// 不同种子洗出不同排列,且不等于原顺序 —— 随机模式真的在随机。
    #[test]
    fn shuffle_actually_shuffles() {
        let n = 100;
        let a = shuffled_indices(&mut 1u64, n, None);
        let b = shuffled_indices(&mut 2u64, n, None);
        assert_ne!(a, b, "不同种子必须洗出不同排列");
        assert_ne!(a, (0..n).collect::<Vec<_>>(), "洗牌结果不应等于原顺序");
    }

    /// 轮末重洗:首项避开刚播完的那首;空列表/单曲不 panic。
    #[test]
    fn shuffle_avoids_previous_track() {
        let n = 30;
        for avoid in 0..n {
            let mut rng = 0xDEAD_BEEFu64.wrapping_add(avoid as u64 * 7919);
            let order = shuffled_indices(&mut rng, n, Some(avoid));
            assert_ne!(order[0], avoid, "重洗后第一首不能还是上一首");
        }
        assert_eq!(shuffled_indices(&mut 7u64, 1, Some(0)), vec![0]);
        assert!(shuffled_indices(&mut 7u64, 0, Some(0)).is_empty());
    }
}

#[cfg(test)]
mod mods_tests {
    use super::*;

    /// 导入/快速选项的 mods 互斥:HR↔EZ、DT/HT/NC 三选一,再点取消。
    #[test]
    fn mods_toggle_enforces_exclusive_groups() {
        // HR 与 EZ 互斥
        assert_eq!(toggle_mod(2, 16), 16, "开 HR 关 EZ");
        assert_eq!(toggle_mod(16, 2), 2, "开 EZ 关 HR");
        // DT / HT / NC 三选一
        assert_eq!(toggle_mod(256, 64), 64, "开 DT 关 HT");
        assert_eq!(toggle_mod(64, 512), 512, "开 NC 关 DT");
        assert_eq!(toggle_mod(64 | 256, 512), 512, "开 NC 关 DT+HT");
        // 再点同一位取消
        assert_eq!(toggle_mod(8, 8), 0);
        // 不同组不互相影响(HD 可与 HR 共存)
        assert_eq!(toggle_mod(8, 16), 8 | 16);
        assert_eq!(toggle_mod(8 | 64, 512), 8 | 512);
    }
}
