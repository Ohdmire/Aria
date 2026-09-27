//! Win32 桌面附加 —— 逐行移植 Lively Wallpaper (rocksdanister/lively,
//! WinDesktopCore.cs)的实现,含现代 Win11 "raised desktop" 分支:
//!
//! 微软官方说明(Lively 内嵌引用):现代 Windows 的桌面背景为支持 HDR 等
//! 场景改为单一 Progman 顶层窗口(WS_EX_NOREDIRECTIONBITMAP,无 GDI 重定向
//! 位图),图标层 SHELLDLL_DefView 是其 WS_EX_LAYERED 子窗口;桌面"升起"
//! 时 shell 会创建一个排在 DefView 之下的子 WorkerW 渲染壁纸。应用要做
//! 的是:创建**自己的 WS_EX_LAYERED 子窗口(alpha=255)**,SetParent 到
//! Progman,并用 SetWindowPos 把 z 序精确放在 DefView 正下方 —— 这样
//! 图标、右键菜单、框选全部由 DefView 正常处理,壁纸在其下渲染。
//!
//! 旧布局(经典 WorkerW):壁纸层是顶层 WorkerW,SetParent 进去即可。
//!
//! Win11 26xxx 实测的两个坑(见 [`find_progman`] / [`attach`]):
//! - FindWindow 仅按类名可能找不到 Progman(类名+标题一起给才行),
//!   按父句柄枚举子级不受影响;
//! - shell 会偶发重建桌面窗口(Progman 句柄换新),旧句柄上的
//!   SetParent 无声失败(GetLastError=0)—— 附加必须带"重新发现 +
//!   重试"。0x052C 可能触发这次重组,所以发完立刻重新找 Progman,
//!   失败交给外层重试,不在旧句柄上继续挂。
//!
//! 0x052C 消息按 Lively 的参数发:wParam=0xD、lParam=0x1。raised
//! 桌面也要发一次,让 shell 生出画静态壁纸的子 WorkerW;自己的窗口
//! 夹在 DefView 和这个 WorkerW 之间,并把 WorkerW 压到 Progman 最底
//! ([`keep_workerw_under`],Lively `EnsureWorkerWZOrder`)。WorkerW
//! 浮上来会把画面整个盖住。

use anyhow::{Result, bail};
use windows::Win32::Foundation::{GetLastError, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW, MapWindowPoints,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, FindWindowW, GA_PARENT, GWL_EXSTYLE, GWL_STYLE, GetAncestor,
    GetClientRect, GetWindowLongPtrW, GetWindowThreadProcessId, HWND_BOTTOM, IsIconic, IsWindow,
    IsWindowVisible, LWA_ALPHA, MONITORINFOF_PRIMARY, SMTO_NORMAL, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNOACTIVATE, SendMessageTimeoutW,
    SetLayeredWindowAttributes, SetParent, SetWindowLongPtrW, SetWindowPos, ShowWindow, WS_CAPTION,
    WS_CHILD, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::BOOL;
use windows::core::{PCWSTR, w};

/// 一台显示器的描述(虚拟屏绝对坐标;device 如 `\\.\DISPLAY2`,跨会话
/// 稳定,持久化选择用)。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Monitor {
    pub device: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub primary: bool,
}

/// 枚举当前桌面显示器(Lively DisplayManager 同款:EnumDisplayMonitors +
/// MONITORINFOEXW)。失败/空表时返回空。
pub fn list_monitors() -> Vec<Monitor> {
    unsafe {
        unsafe extern "system" fn mon_enum(
            hmon: HMONITOR,
            _hdc: HDC,
            _rect: *mut RECT,
            lparam: LPARAM,
        ) -> BOOL {
            let out = &mut *(lparam.0 as *mut Vec<Monitor>);
            unsafe {
                let mut mi = MONITORINFOEXW::default();
                mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
                if GetMonitorInfoW(
                    hmon,
                    &mut mi as *mut MONITORINFOEXW
                        as *mut windows::Win32::Graphics::Gdi::MONITORINFO,
                )
                .as_bool()
                {
                    let device = String::from_utf16_lossy(
                        &mi.szDevice[..mi
                            .szDevice
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(mi.szDevice.len())],
                    );
                    let r = mi.monitorInfo.rcMonitor;
                    out.push(Monitor {
                        device,
                        x: r.left,
                        y: r.top,
                        w: r.right - r.left,
                        h: r.bottom - r.top,
                        primary: (mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0,
                    });
                }
            }
            true.into()
        }
        let mut out: Vec<Monitor> = Vec::new();
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(mon_enum),
            LPARAM(&mut out as *mut Vec<Monitor> as isize),
        );
        out
    }
}

/// 未公开的 Progman 消息:让 shell 整理桌面层(生成 WorkerW)。
const SPAWN_WORKER: u32 = 0x052C;
/// 未在 windows crate 常量里的扩展样式:无 GDI 重定向位图("raised desktop"标志)。
const WS_EX_NOREDIRECTIONBITMAP: isize = 0x0020_0000;
/// 未导出常量:GWL 索引读窗口属主/父窗口句柄。

/// 壁纸宿主信息。
#[derive(Clone, Copy)]
pub struct WallpaperHost {
    pub parent: HWND,
    pub child: HWND,
    pub width: u32,
    pub height: u32,
}

impl WallpaperHost {
    fn invalid() -> WallpaperHost {
        WallpaperHost {
            parent: HWND::default(),
            child: HWND::default(),
            width: 0,
            height: 0,
        }
    }
}

/// 诊断:打印 Progman 直接子级与顶层 shell 窗口(定位问题用)。
unsafe fn log_shell_layout(progman: HWND) {
    let mut lines = Vec::new();
    let mut after: Option<HWND> = None;
    for _ in 0..16 {
        let child = FindWindowExW(Some(progman), after, None, PCWSTR::null()).unwrap_or_default();
        if child.is_invalid() {
            break;
        }
        let class = class_name(child);
        let (w, h) = client_size(child);
        lines.push(format!("{class}({w}x{h})"));
        after = Some(child);
    }
    let ex = GetWindowLongPtrW(progman, GWL_EXSTYLE);
    log::info!(
        "shell layout: Progman(ex={ex:#x}) -> [{}]",
        lines.join(", ")
    );
}

unsafe fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// 当前显示器的刷新率(Hz,读不到按 60)。
pub fn monitor_refresh_hz() -> u32 {
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
    unsafe {
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut dm).as_bool() {
            (dm.dmDisplayFrequency as u32).max(1)
        } else {
            60
        }
    }
}

// ---------- 桌面遮挡检测(遮挡时不渲染画面) ----------

/// 枚举回调上下文:各显示器工作区 + 是否已被某可见窗口完全覆盖。
struct CoverCtx {
    work_areas: Vec<RECT>,
    covered: Vec<bool>,
    /// 壁纸窗口自身(排除)。
    except: HWND,
}

unsafe extern "system" fn cover_enum(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut CoverCtx);
    unsafe {
        if hwnd == ctx.except || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return true.into();
        }
        let class = class_name(hwnd);
        if class == "Progman" || class == "WorkerW" || class == "SHELLDLL_DefView" {
            return true.into();
        }
        // DWM 挂起窗口(UWP 隐藏/cloaked)有残影矩形,必须排除
        if dwm_cloaked(hwnd) {
            return true.into();
        }
        let Some(rect) = window_bounds(hwnd) else {
            return true.into();
        };
        for (i, wa) in ctx.work_areas.iter().enumerate() {
            if !ctx.covered[i]
                && rect.left <= wa.left
                && rect.top <= wa.top
                && rect.right >= wa.right
                && rect.bottom >= wa.bottom
            {
                ctx.covered[i] = true;
            }
        }
    }
    true.into()
}

/// DWMWA_CLOAKED:窗口被 UWP 挂起/虚拟桌面隐藏等不可见。
unsafe fn dwm_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    let ok = unsafe {
        windows::Win32::Graphics::Dwm::DwmGetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    ok.is_ok() && cloaked != 0
}

/// DWMWA_EXTENDED_FRAME_BOUNDS:窗口可见边界(不含阴影,DPI 物理坐标)。
unsafe fn window_bounds(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    let ok = unsafe {
        windows::Win32::Graphics::Dwm::DwmGetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut core::ffi::c_void,
            std::mem::size_of::<RECT>() as u32,
        )
    };
    ok.is_ok().then_some(rect)
}

/// 桌面壁纸是否被完全遮挡:**每个**显示器的工作区都被某个可见窗口
/// 完全盖住 —— 其他应用全屏(盖整屏)或最大化(恰好盖工作区)时的
/// 姿势。此时壁纸不可见,渲染可暂停省电。`except` 为壁纸窗口自身。
pub fn desktop_covered(except: HWND) -> bool {
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HMONITOR, MONITORINFO,
    };
    unsafe {
        unsafe extern "system" fn mon_enum(
            hmon: HMONITOR,
            _hdc: windows::Win32::Graphics::Gdi::HDC,
            _rect: *mut RECT,
            lparam: LPARAM,
        ) -> BOOL {
            let out = &mut *(lparam.0 as *mut Vec<RECT>);
            unsafe {
                let mut mi = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(hmon, &mut mi).as_bool() {
                    out.push(mi.rcWork);
                }
            }
            true.into()
        }
        let mut work_areas: Vec<RECT> = Vec::new();
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(mon_enum),
            LPARAM(&mut work_areas as *mut Vec<RECT> as isize),
        );
        if work_areas.is_empty() {
            return false; // 枚举失败(极罕见):按未遮挡处理,保渲染
        }
        let mut ctx = CoverCtx {
            covered: vec![false; work_areas.len()],
            work_areas,
            except,
        };
        let _ = EnumWindows(Some(cover_enum), LPARAM(&mut ctx as *mut CoverCtx as isize));
        ctx.covered.iter().all(|&c| c)
    }
}

pub fn client_size(hwnd: HWND) -> (u32, u32) {
    unsafe {
        let mut r = RECT::default();
        if GetClientRect(hwnd, &mut r).is_ok() {
            (
                (r.right - r.left).max(0) as u32,
                (r.bottom - r.top).max(0) as u32,
            )
        } else {
            (0, 0)
        }
    }
}

/// 窗口属主进程 pid(WorkerW 候选的 shell 属主校验用)。
fn pid_of(hwnd: HWND) -> u32 {
    unsafe { GetWindowThreadProcessId(hwnd, None) }
}

/// 取 winit 窗口的原生 HWND。
pub fn window_hwnd(window: &impl raw_window_handle::HasWindowHandle) -> Result<HWND> {
    let handle = window.window_handle().map_err(|e| anyhow::anyhow!("{e}"))?;
    match handle.as_raw() {
        raw_window_handle::RawWindowHandle::Win32(h) => {
            Ok(HWND(h.hwnd.get() as *mut core::ffi::c_void))
        }
        _ => bail!("非 Win32 窗口"),
    }
}

/// 重定父(lively WindowUtil.TrySetParent 同款):裸 SetParent,失败即
/// 失败 —— 没有 GWLP_HWNDPARENT 直写一类的绕过(直写内部句柄会被
/// 安全软件的行为拦截判定为注入,得不偿失)。
/// SetParent 返回的是"旧父句柄",存在 NULL 歧义(windows-rs 会把
/// 成功误包装成 Err),故成败只认事后的真实父子关系。失败时打出
/// 真实 GetLastError(5=权限拒绝,87=参数错误),供定位环境问题。
unsafe fn reparent(child: HWND, parent: HWND) -> bool {
    let _ = SetParent(child, Some(parent));
    // GetLastError 必须在任何其他 Win32 调用(含下行的 GetAncestor)之前取走
    let err = GetLastError().0;
    let ok = GetAncestor(child, GA_PARENT) == parent;
    if !ok {
        log::warn!("[attach] SetParent 挂 {parent:?} 失败 (GetLastError={err})");
    }
    ok
}

/// 在 Progman 的**直接子级**里找 shell 属主的 WorkerW。24H2+ 新布局里
/// 系统壁纸 WorkerW 从顶层窗口变成了 Progman 的子窗口(微软:raised
/// desktop "we create a child WorkerW window that is z-ordered under the
/// DefView that will render the wallpaper"),它就是现成的壁纸层 ——
/// SetParent 到 Progman 本体被拒的新构建上,实测可挂这里(仍处于
/// DefView 之下,图标交互不受影响)。找不到返回无效句柄。
unsafe fn shell_workerw_child(progman: HWND) -> HWND {
    let shell_pid = pid_of(progman);
    let mut after: Option<HWND> = None;
    for _ in 0..16 {
        let cand =
            FindWindowExW(Some(progman), after, w!("WorkerW"), PCWSTR::null()).unwrap_or_default();
        if cand.is_invalid() {
            break;
        }
        if pid_of(cand) == shell_pid {
            return cand;
        }
        after = Some(cand);
    }
    HWND::default()
}

/// 让 shell 生成壁纸 WorkerW(Lively 参数 wParam=0xD, lParam=0x1)。
/// 已存在时 shell 通常无操作;个别构建会趁机换掉 Progman 句柄。
unsafe fn spawn_workerw(progman: HWND) {
    let mut result = 0usize;
    let _ = SendMessageTimeoutW(
        progman,
        SPAWN_WORKER,
        WPARAM(0xD),
        LPARAM(0x1),
        SMTO_NORMAL,
        1000,
        Some(&mut result),
    );
}

/// 5 秒内只发一次。挂接和每秒保活都会走到这里,连发会把 Progman 句柄换掉。
unsafe fn spawn_workerw_throttled(progman: HWND) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let prev = LAST_WORKERW_SPAWN.load(std::sync::atomic::Ordering::Relaxed);
    if now.saturating_sub(prev) < 5_000 {
        return false;
    }
    LAST_WORKERW_SPAWN.store(now, std::sync::atomic::Ordering::Relaxed);
    spawn_workerw(progman);
    true
}

/// Progman 的直接子窗口,按 z 序从顶到底。
unsafe fn direct_children(parent: HWND) -> Vec<HWND> {
    let mut out = Vec::new();
    let mut after: Option<HWND> = None;
    for _ in 0..64 {
        let child = FindWindowExW(Some(parent), after, None, PCWSTR::null()).unwrap_or_default();
        if child.is_invalid() {
            break;
        }
        out.push(child);
        after = Some(child);
    }
    out
}

/// raised desktop 保活(Lively `EnsureWorkerWZOrder` + WorkerW 销毁后重排):
/// 静态壁纸 WorkerW 必须留在 Progman 最底,自己的窗口留在 DefView 正下方。
/// WorkerW 不见了就再请求 shell 生一次(5s 节流,避免每秒重组桌面)。
/// 只在 z 序真的错了时才 SetWindowPos。
pub fn keep_workerw_under(child: HWND) {
    unsafe {
        if child.is_invalid() || !IsWindow(Some(child)).as_bool() {
            return;
        }
        let mut progman = find_progman();
        if progman.is_invalid() || !IsWindow(Some(progman)).as_bool() {
            return;
        }
        if (GetWindowLongPtrW(progman, GWL_EXSTYLE) & WS_EX_NOREDIRECTIONBITMAP) == 0 {
            return;
        }
        let mut workerw = shell_workerw_child(progman);
        if workerw.is_invalid() && spawn_workerw_throttled(progman) {
            log::info!("[attach] 壁纸 WorkerW 缺失,已请求 shell 重建");
            progman = find_progman();
            if progman.is_invalid() {
                return;
            }
            workerw = shell_workerw_child(progman);
        }
        if workerw.is_invalid() || !IsWindow(Some(workerw)).as_bool() {
            return;
        }
        let kids = direct_children(progman);
        if kids.last().copied() != Some(workerw) {
            let _ = SetWindowPos(
                workerw,
                Some(HWND_BOTTOM),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            log::info!("[attach] WorkerW {:?} 压回 Progman 最底", workerw.0);
        }
        if GetAncestor(child, GA_PARENT) != progman {
            return;
        }
        let defview = FindWindowExW(Some(progman), None, w!("SHELLDLL_DefView"), PCWSTR::null())
            .unwrap_or_default();
        if defview.is_invalid() {
            return;
        }
        let kids = direct_children(progman);
        let above = |h: HWND| kids.iter().position(|c| *c == h);
        // 索引越小越靠上。目标:DefView 在上,自己居中,WorkerW 在下。
        let buried = match (above(defview), above(child), above(workerw)) {
            (Some(d), Some(c), Some(w)) => !(d < c && c < w),
            _ => false,
        };
        if buried {
            let _ = SetWindowPos(
                child,
                Some(defview),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            log::info!("[attach] 壁纸窗口重新压到 DefView {:?} 之下", defview.0);
        }
    }
}

static LAST_WORKERW_SPAWN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 目标显示器 bounds(虚拟屏绝对坐标 x/y/w/h):None = 铺满宿主客户区
/// (旧行为,双屏下即"span"效果)。
pub type MonitorBounds = (i32, i32, u32, u32);

/// 找 Progman。Win11 26xxx 实测仅按**类名** FindWindowW 可能被新加的
/// 匹配限制挡掉(类名+标题一起给才行),故先带标题找,再 EnumWindows
/// 按类名兜底(取可见的那个)。按父句柄枚举子级不受该限制影响。
unsafe fn find_progman() -> HWND {
    let h = FindWindowW(w!("Progman"), w!("Program Manager")).unwrap_or_default();
    if !h.is_invalid() {
        return h;
    }
    struct Ctx {
        progman: HWND,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut Ctx);
        unsafe {
            if ctx.progman.is_invalid() && class_name(hwnd) == "Progman" {
                // 只认可见的(历史崩溃可留下不可见的残留桌面窗口)
                if IsWindowVisible(hwnd).as_bool() {
                    ctx.progman = hwnd;
                }
            }
        }
        true.into()
    }
    let mut ctx = Ctx {
        progman: HWND::default(),
    };
    let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize));
    ctx.progman
}

/// 把已创建(仍隐藏)的窗口附加为壁纸(Lively TryAttachToDesktop 移植)。
/// `bounds` 指定只占某一显示器(Lively TrySetWallpaperPerScreen 两步坐标
/// 法:重定父前按屏幕绝对坐标定位,挂靠后 MapWindowPoints 换算父客户区
/// 坐标再落位)。显示交给调用方。
///
/// 带重试:shell 可能在附加瞬间重建桌面(实测 26200:Progman 句柄被换新,
/// 旧句柄上的 SetParent 无声失败,GetLastError=0)—— 每轮重新发现全部
/// 句柄再试,总计 3 轮。
struct ClassicLayer {
    defview: HWND,
    owner: HWND,
    worker: HWND,
}

/// Lively 的枚举:顶层窗口里谁直接挂着 SHELLDLL_DefView,它后面第一个
/// shell 属主的 WorkerW 就是壁纸层。`skip` 是我们自己的窗口。
unsafe fn locate_classic_layer(skip: HWND, shell_pid: u32) -> ClassicLayer {
    struct Ctx {
        skip: HWND,
        shell_pid: u32,
        defview: HWND,
        owner: HWND,
        worker: HWND,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        let ctx = &mut *(lp.0 as *mut Ctx);
        if hwnd == ctx.skip {
            return BOOL(1);
        }
        let dv = FindWindowExW(Some(hwnd), None, w!("SHELLDLL_DefView"), PCWSTR::null())
            .unwrap_or_default();
        if dv.is_invalid() {
            return BOOL(1);
        }
        ctx.defview = dv;
        ctx.owner = hwnd;
        let mut next =
            FindWindowExW(None, Some(hwnd), w!("WorkerW"), PCWSTR::null()).unwrap_or_default();
        while !next.is_invalid() {
            if next != ctx.skip && pid_of(next) == ctx.shell_pid {
                ctx.worker = next;
                break;
            }
            next =
                FindWindowExW(None, Some(next), w!("WorkerW"), PCWSTR::null()).unwrap_or_default();
        }
        BOOL(1)
    }
    let mut ctx = Ctx {
        skip,
        shell_pid,
        defview: HWND::default(),
        owner: HWND::default(),
        worker: HWND::default(),
    };
    let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize));
    // Progman 的子 WorkerW(部分 Win10 把壁纸层挂在这里,而不是顶层兄弟)
    if ctx.worker.is_invalid() {
        let progman = find_progman();
        let child_w = shell_workerw_child(progman);
        if !child_w.is_invalid() && child_w != skip {
            ctx.worker = child_w;
        }
    }
    ClassicLayer {
        defview: ctx.defview,
        owner: ctx.owner,
        worker: ctx.worker,
    }
}

unsafe fn log_classic_candidates(skip: HWND, shell_pid: u32) {
    let mut after: Option<HWND> = None;
    let mut n = 0u32;
    for _ in 0..32 {
        let w = FindWindowExW(None, after, w!("WorkerW"), PCWSTR::null()).unwrap_or_default();
        if w.is_invalid() {
            break;
        }
        n += 1;
        let dv = FindWindowExW(Some(w), None, w!("SHELLDLL_DefView"), PCWSTR::null())
            .unwrap_or_default();
        log::info!(
            "[attach] 顶层 WorkerW #{n} {:?} pid={} shell={} 自己={} 含DefView={}",
            w.0,
            pid_of(w),
            pid_of(w) == shell_pid,
            w == skip,
            !dv.is_invalid()
        );
        after = Some(w);
    }
    if n == 0 {
        log::info!("[attach] 没有顶层 WorkerW");
    }
}

/// 与探测程序相同:进窗口前声明 Per-Monitor V2,客户区按物理像素计算。
pub fn enable_per_monitor_dpi() {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetProcessDpiAwarenessContext(value: *mut core::ffi::c_void) -> i32;
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(-4isize as *mut core::ffi::c_void);
    }
}

pub fn attach(child: HWND, bounds: Option<MonitorBounds>) -> WallpaperHost {
    attach_impl(child, bounds, true)
}

/// [`attach`] 之后 winit `set_visible` 会按自己的 WindowFlags 整页重写
/// `GWL_STYLE` / `GWL_EXSTYLE`，把 `WS_CHILD` 和 `WS_EX_LAYERED` 清掉。
/// raised desktop 上 Progman 没有 GDI 重定向位图，只合成带 LAYERED 的
/// 子窗口 —— 样式被清掉后挂接日志仍是成功，桌面上看不到画面。
/// 显示之后再走一遍挂接把样式和 z 序盖回去。此时窗口多半已经是
/// 桌面子窗口，不能再按屏幕坐标预定位。
pub fn reattach(child: HWND, bounds: Option<MonitorBounds>) -> WallpaperHost {
    attach_impl(child, bounds, false)
}

/// 不激活地显示。`set_visible` 改完样式后再调一次，避免样式回写把
/// 可见位一起弄丢。
pub fn show_no_activate(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

fn attach_impl(child: HWND, bounds: Option<MonitorBounds>, screen_place: bool) -> WallpaperHost {
    unsafe {
        // raised 且还没有画静态壁纸的子 WorkerW:先让 shell 生出来。
        // 这一步可能把 Progman 句柄换新,所以放在重试循环之前,循环里
        // 每次都重新 find_progman。已有 WorkerW 就不再发,避免每次
        // 重新封样式都触发一次桌面重组。
        let progman = find_progman();
        if !progman.is_invalid()
            && (GetWindowLongPtrW(progman, GWL_EXSTYLE) & WS_EX_NOREDIRECTIONBITMAP) != 0
            && shell_workerw_child(progman).is_invalid()
            && spawn_workerw_throttled(progman)
        {
            log::info!("[attach] raised desktop 无壁纸 WorkerW,已请求 shell 生成");
        }
        // 重定父前先按屏幕绝对坐标就位(SetParent 保留视觉位置,挂靠后
        // 再按换算出的父相对坐标精确定位)。已经挂上之后再调用时跳过:
        // 子窗口的 SetWindowPos 坐标是父客户区坐标,屏幕坐标会把它挪飞。
        if screen_place {
            if let Some((x, y, w, h)) = bounds {
                let _ = SetWindowPos(
                    child,
                    None,
                    x,
                    y,
                    w as i32,
                    h as i32,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
        }
        let mut host = WallpaperHost::invalid();
        for attempt in 0..3 {
            if attempt > 0 {
                log::info!("[attach] 第 {attempt} 次附加未成,重新发现桌面结构后重试");
                std::thread::sleep(std::time::Duration::from_millis(600));
            }
            host = attach_once(child, bounds);
            if host.width > 0 {
                return host;
            }
        }
        log::error!("附加失败:窗口无法挂到桌面(Progman/壁纸 WorkerW 均被拒)");
        host
    }
}

/// 单轮附加(发现 → 选分支 → 挂接)。失败返回 invalid(host.width=0),
/// 由 [`attach`] 决定是否重试。
unsafe fn attach_once(child: HWND, bounds: Option<MonitorBounds>) -> WallpaperHost {
    let progman = find_progman();
    if progman.is_invalid() || !IsWindow(Some(progman)).as_bool() {
        log::warn!("[attach] 未找到 Progman");
        return WallpaperHost::invalid();
    }

    // raised desktop:Progman 无 GDI 重定向位图。0x052C 在 attach_impl
    // 里、本轮发现之前发(仅当还没有子 WorkerW)。这里用的是发完之后
    // 重新找到的 Progman。
    let raised = (GetWindowLongPtrW(progman, GWL_EXSTYLE) & WS_EX_NOREDIRECTIONBITMAP) != 0;
    let defview = FindWindowExW(Some(progman), None, w!("SHELLDLL_DefView"), PCWSTR::null())
        .unwrap_or_default();
    log_shell_layout(progman);
    log::info!("raised desktop = {raised}, DefView = {:?}", defview.0);

    // 公共扩展样式:不抢焦点 / 不进 Alt-Tab
    let mut ex = GetWindowLongPtrW(child, GWL_EXSTYLE);
    ex |= WS_EX_NOACTIVATE.0 as isize | WS_EX_TOOLWINDOW.0 as isize;
    // 公共窗口样式:去弹窗/标题
    let mut style = GetWindowLongPtrW(child, GWL_STYLE);
    style &= !(WS_POPUP.0 as isize | WS_CAPTION.0 as isize);

    let mut host = WallpaperHost::invalid();
    // 调试开关:强制走经典布局分支(验证 WorkerW 选择/压底兜底),
    // 正常环境勿设。
    let force_classic = std::env::var_os("ARIA_FORCE_CLASSIC").is_some();
    if raised && !force_classic && !defview.is_invalid() {
        // ── 微软官方姿势(见文件头注释):WS_CHILD + WS_EX_LAYERED
        //    (alpha=255 保证 DX/swapchain 呈现不受损)挂 Progman,
        //    z 序压到 DefView 正下方 ──
        // 注意顺序:样式与 LAYERED 属性必须在 SetParent 之前生效;
        // 挂接失败则回滚 WS_CHILD —— 留下"有子样式无父窗口"的
        // 半附加状态会让窗口掉进不可控的桌面层。
        ex |= WS_EX_LAYERED.0 as isize;
        SetWindowLongPtrW(child, GWL_EXSTYLE, ex);
        style |= WS_CHILD.0 as isize;
        SetWindowLongPtrW(child, GWL_STYLE, style);
        SetLayeredWindowAttributes(child, Default::default(), 255, LWA_ALPHA).log_unwrap();
        // Progman 挂不上时的保险:改挂 shell 自建的壁纸 WorkerW
        //(24H2+ 它是 Progman 的直接子窗口,恒在 DefView 之下)——
        // 同样处于图标层之下,只是改为盖住系统静态壁纸,本来就是
        // 壁纸窗口的用途。两种挂法在健康桌面上实测都可行;失败
        // 最常见的来路其实是句柄已死(桌面刚被 shell 重建),交外层
        // 重试重新发现。
        let mut parent = progman;
        if !reparent(child, progman) {
            let ww = shell_workerw_child(progman);
            if ww.is_invalid() || !reparent(child, ww) {
                style &= !(WS_CHILD.0 as isize);
                SetWindowLongPtrW(child, GWL_STYLE, style);
                // 常见来路:桌面刚被 shell 重建,发现的句柄已死
                //(SetParent 无声失败)—— 外层重试会重新发现
                log::warn!("[attach] 本轮挂 Progman/壁纸 WorkerW 均被拒");
                return WallpaperHost::invalid();
            }
            parent = ww;
            log::warn!("[attach] Progman 拒绝挂载,已回退挂壁纸 WorkerW {ww:?}");
        }
        let (w, h) = match bounds {
            Some(b) => (b.2, b.3),
            None => client_size(parent),
        };
        if w > 0 && h > 0 {
            // 挂 Progman 时:insertAfter = DefView,紧贴图标层之下;
            // 回退挂 WorkerW 时:DefView 不是兄弟窗口(它是 WorkerW
            // 的兄弟),置顶即可 —— WorkerW 自身已在 DefView 之下。
            // 坐标 = 目标屏在父客户区的映射(bounds 时)或 (0,0) 铺满
            let (px, py) = match bounds {
                Some((x, y, _, _)) => {
                    let mut pt = [windows::Win32::Foundation::POINT { x, y }];
                    MapWindowPoints(None, Some(parent), &mut pt);
                    (pt[0].x, pt[0].y)
                }
                None => (0, 0),
            };
            let insert_after = if parent == progman {
                Some(defview)
            } else {
                None
            };
            let _ = SetWindowPos(
                child,
                insert_after,
                px,
                py,
                w as i32,
                h as i32,
                SWP_FRAMECHANGED | SWP_NOACTIVATE,
            );
        }
        host = WallpaperHost {
            parent,
            child,
            width: w.max(1),
            height: h.max(1),
        };
        log::info!(
            "raised desktop 附加: 父窗口 {:?} 客户区 {}×{},压在 DefView {:?} 之下",
            parent,
            host.width,
            host.height,
            defview.0
        );
        if parent == progman {
            keep_workerw_under(child);
        }
        return host;
    }

    // ── 经典布局,与 Lively SetupDesktopLayer 相同:
    // 先 0x052C,再枚举顶层窗口,找到「直接子级是 SHELLDLL_DefView」的
    // 那个窗口,取它后面(z 序更低)、shell 属主的第一个 WorkerW。
    // 自己的 HWND 一律跳过:winit 的类名是 "Window Class",挂到 Progman
    // 之后会出现在子窗口列表里,不能把它当成壳层。
    // 找不到 WorkerW 就失败返回,不再挂 Progman 充数 —— 那一层没有
    // 图标 DefView,画面出不来,外层还会把这次当成成功。
    // 0x052C 走节流:reattach 只是补样式,不能再发,否则 shell 重排桌面。
    let shell_pid = pid_of(progman);
    let mut layer = locate_classic_layer(child, shell_pid);
    if layer.worker.is_invalid() && spawn_workerw_throttled(progman) {
        log::info!("[attach] 经典布局没有壁纸 WorkerW,已请求 shell 生成");
        std::thread::sleep(std::time::Duration::from_millis(300));
        layer = locate_classic_layer(child, shell_pid);
    }
    log_classic_candidates(child, shell_pid);
    if layer.worker.is_invalid() {
        log::warn!(
            "[attach] 未找到 shell 壁纸 WorkerW (DefView={:?} 宿主={:?} {}),不挂 Progman",
            layer.defview.0,
            layer.owner.0,
            if layer.owner.is_invalid() {
                String::new()
            } else {
                class_name(layer.owner)
            }
        );
        return WallpaperHost::invalid();
    }
    let parent = layer.worker;
    log::info!(
        "[attach] 经典布局 WorkerW {:?} (DefView {:?} 在 {:?} 之下)",
        parent.0,
        layer.defview.0,
        layer.owner.0
    );

    SetWindowLongPtrW(child, GWL_EXSTYLE, ex | WS_EX_TRANSPARENT.0 as isize);
    SetWindowLongPtrW(child, GWL_STYLE, style);
    if !reparent(child, parent) {
        log::warn!("[attach] 本轮挂 WorkerW/Progman 被拒,交由外层重试");
        return WallpaperHost::invalid();
    }
    let (w, h) = match bounds {
        Some(b) => (b.2, b.3),
        None => client_size(parent),
    };
    if w > 0 && h > 0 {
        let (px, py) = match bounds {
            Some((x, y, _, _)) => {
                let mut pt = [windows::Win32::Foundation::POINT { x, y }];
                MapWindowPoints(None, Some(parent), &mut pt);
                (pt[0].x, pt[0].y)
            }
            None => (0, 0),
        };
        let _ = SetWindowPos(
            child,
            Some(HWND_BOTTOM),
            px,
            py,
            w as i32,
            h as i32,
            SWP_FRAMECHANGED | SWP_NOACTIVATE,
        );
    }
    host = WallpaperHost {
        parent,
        child,
        width: w.max(1),
        height: h.max(1),
    };
    log::info!(
        "经典布局附加: parent={:?} ({}×{})",
        parent.0,
        host.width,
        host.height
    );
    host
}

/// 子窗口铺满宿主客户区(或指定显示器 bounds),保持既有 z 序
/// (分辨率轮询跟随尺寸用)。
pub fn fill_parent(child: HWND, parent: HWND, bounds: Option<MonitorBounds>) -> (u32, u32) {
    unsafe {
        let (w, h) = match bounds {
            Some(b) => (b.2, b.3),
            None => client_size(parent),
        };
        if w > 0 && h > 0 {
            let (px, py) = match bounds {
                Some((x, y, _, _)) => {
                    let mut pt = [windows::Win32::Foundation::POINT { x, y }];
                    MapWindowPoints(None, Some(parent), &mut pt);
                    (pt[0].x, pt[0].y)
                }
                None => (0, 0),
            };
            let _ = SetWindowPos(
                child,
                None,
                px,
                py,
                w as i32,
                h as i32,
                SWP_NOZORDER | SWP_FRAMECHANGED,
            );
        }
        (w, h)
    }
}

/// 修剪当前进程工作集(-1,-1 = 收缩到最小):把已释放内存的堆保留页
/// 还给 OS。加载完成(字体光栅/背景解码/皮肤解码等高水位瞬态已死)与
/// 渲染暂停(GPU 会话已拆)后调用,显示占用直降一两百 MB;代价是后续
/// 访问少量软缺页,恢复渲染时本就要重建会话,无感。
pub fn trim_working_set() {
    use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

trait LogUnwrap {
    fn log_unwrap(self);
}
impl LogUnwrap for windows::core::Result<()> {
    fn log_unwrap(self) {
        if let Err(e) = self {
            log::warn!("SetLayeredWindowAttributes: {e}");
        }
    }
}
