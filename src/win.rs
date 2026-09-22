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
//!   重试",且已 raised 的桌面不再发 0x052C 以免触发重组。
//!
//! 0x052C 消息按 Lively 的参数发:wParam=0xD、lParam=0x1(仅经典布局)。

use anyhow::{Result, bail};
use windows::Win32::Foundation::{GetLastError, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW, MapWindowPoints,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, FindWindowW, GA_PARENT, GWL_EXSTYLE, GWL_STYLE, GetAncestor,
    GetClientRect, GetWindowLongPtrW, GetWindowThreadProcessId, HWND_BOTTOM, IsIconic, IsWindow,
    IsWindowVisible, LWA_ALPHA, MONITORINFOF_PRIMARY, SMTO_NORMAL, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOZORDER, SendMessageTimeoutW, SetLayeredWindowAttributes, SetParent,
    SetWindowLongPtrW, SetWindowPos, WS_CAPTION, WS_CHILD, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
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
pub fn attach(child: HWND, bounds: Option<MonitorBounds>) -> WallpaperHost {
    unsafe {
        // 重定父前先按屏幕绝对坐标就位(SetParent 保留视觉位置,挂靠后
        // 再按换算出的父相对坐标精确定位)
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

    // raised desktop:Progman 无 GDI 重定向位图。**raised 时不再发 0x052C**:
    // 26100+ 桌面默认即 raised,消息对已 raised 的桌面没有意义,反而可能
    // 触发 shell 重组桌面窗口(实测 Progman 句柄被换新,正好赶上挂接
    // 就是被无声拒绝的一种来路)。经典布局分支按旧逻辑发。
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
        return host;
    }

    // ── 经典布局:枚举顶层窗口,找"直接子级含 DefView"的容器,
    //    壁纸层 = 容器之后(全局 Z 序更低)、**shell 属主**的第一个
    //    顶层 WorkerW。属主校验必须做:桌面美化/壁纸软件会留下大量
    //    属于第三方进程的顶层 WorkerW,SetParent 进去会被拒(实测
    //    ERROR_INVALID_PARAMETER),旧版"任意 WorkerW"兜底正是
    //    "壁纸层不可用"的来源。 ──
    // 经典桌面才需要 0x052C 让 shell 生成壁纸 WorkerW(Lively 参数
    // 0xD/0x1);raised 分支不发,见 attach_once 注释。
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
    let shell_pid = pid_of(progman);
    let mut find_container = || {
        let dv = FindWindowExW(Some(progman), None, w!("SHELLDLL_DefView"), PCWSTR::null())
            .unwrap_or_default();
        if !dv.is_invalid() {
            return progman;
        }
        let mut top_after: Option<HWND> = None;
        for _ in 0..512 {
            let wnd = FindWindowExW(None, top_after, None, PCWSTR::null()).unwrap_or_default();
            if wnd.is_invalid() {
                break;
            }
            let dv = FindWindowExW(Some(wnd), None, w!("SHELLDLL_DefView"), PCWSTR::null())
                .unwrap_or_default();
            if !dv.is_invalid() {
                return wnd;
            }
            top_after = Some(wnd);
        }
        HWND::default()
    };
    let mut container = if !defview.is_invalid() {
        progman
    } else {
        find_container()
    };
    if container.is_invalid() {
        // 首次 0x052C 可能因 shell 忙而超时(1s),DefView 尚未就位:
        // 加时重发一次再找。
        let mut retry = 0usize;
        let _ = SendMessageTimeoutW(
            progman,
            SPAWN_WORKER,
            WPARAM(0xD),
            LPARAM(0x1),
            SMTO_NORMAL,
            3000,
            Some(&mut retry),
        );
        container = find_container();
    }
    let mut parent = HWND::default();
    if !container.is_invalid() {
        let mut cand =
            FindWindowExW(None, Some(container), w!("WorkerW"), PCWSTR::null()).unwrap_or_default();
        while !cand.is_invalid() {
            if pid_of(cand) == shell_pid {
                parent = cand;
                break;
            }
            cand =
                FindWindowExW(None, Some(cand), w!("WorkerW"), PCWSTR::null()).unwrap_or_default();
        }
        // 24H2+ 新布局:壁纸 WorkerW 是 Progman 的直接子窗口,顶层
        // 枚举找不到 —— 补查一次
        if parent.is_invalid() {
            parent = shell_workerw_child(progman);
        }
    }
    if parent.is_invalid() {
        // 兜底:挂 DefView 容器本身并压底(HWND_BOTTOM 位于图标层
        // 之下)。容器是 Progman(标准布局)或持有 DefView 的
        // WorkerW(被第三方重排的布局)时都成立。绝不挂非 shell
        // 属主的 WorkerW。
        parent = if container.is_invalid() {
            progman
        } else {
            container
        };
        log::warn!(
            "未找到 shell 属主的壁纸 WorkerW,退回挂 DefView 容器压底 ({:?})",
            parent.0
        );
    }

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
