//! SetParent 返回值语义实证(诊断"桌面壁纸层不可用")—— 独立诊断 exe v2。
//!
//! 复刻 src/win.rs::attach 的全部父级选择逻辑:
//! 1. shell 布局深扫:Progman / raised / DefView 位置(不在 Progman 下时,
//!    枚举顶层窗口找它真正的容器,即应用 classic 分支的 container 循环);
//! 2. 列出全部顶层 WorkerW(Z 序 + 归属进程);
//! 3. 对应用真实会用的两类目标分别做 SetParent 探测:
//!    B1 = "DefView 容器之后的 WorkerW"(应用主查找),
//!    B2 = "任意顶层 WorkerW"(应用兜底,失败即报壁纸层不可用),
//!    另附 A(raised 复刻)与 C(裸挂 Progman)对照。
//! 每次探测报告 crate Ok/Err、Err 错误码、前后 GetLastError、以及
//! GetAncestor(GA_PARENT) 证实的真实父子关系。
//! 全部输出同时写入 exe 旁 sp_probe_result.txt(UTF-8 BOM)。
//! 进程名识别:OpenProcess 被拒时退回 tasklist /FI 查询。

use std::cell::RefCell;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, SetLastError, COLORREF, HWND, LPARAM, WPARAM, WIN32_ERROR,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FindWindowW, GetAncestor, GetWindowLongPtrW, SendMessageTimeoutW,
    SetLayeredWindowAttributes, SetParent, SetWindowLongPtrW, GA_PARENT, GWL_EXSTYLE, GWL_STYLE,
    GWLP_HWNDPARENT, LWA_ALPHA, SMTO_NORMAL, WS_CHILD, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_CAPTION, WS_POPUP,
};

const SPAWN_WORKER: u32 = 0x052C;
const WS_EX_NOREDIRECTIONBITMAP: isize = 0x0020_0000;

fn hwnd_us(h: HWND) -> usize {
    h.0 as usize
}

fn class_of(h: HWND) -> String {
    if h.is_invalid() {
        return "<null>".into();
    }
    let mut buf = [0u16; 64];
    let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClassNameW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn pid_of(h: HWND) -> u32 {
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(h, None) }
}

/// tasklist 兜底查进程名(绕开 OpenProcess 被安全软件/权限拒绝的情形)。
fn tasklist_name(pid: u32) -> Option<String> {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}")])
        .output()
        .ok()?;
    let text = String::from_utf16_lossy(&String::from_utf8_lossy(&out.stdout).encode_utf16().collect::<Vec<u16>>());
    // 兼容本地化表头:直接找"第二个字段等于 pid"的行
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 2 && f[1] == pid.to_string() {
            return Some(f[0].to_string());
        }
    }
    None
}

/// 窗口归属:pid + 进程名(OpenProcess 优先,tasklist 兜底)。
fn owner_of(h: HWND) -> String {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let pid = pid_of(h);
    if pid == 0 {
        return "pid=?".into();
    }
    let name = unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(p) => {
                let mut buf = [0u16; 512];
                let mut len = buf.len() as u32;
                if QueryFullProcessImageNameW(
                    p,
                    PROCESS_NAME_WIN32,
                    windows::core::PWSTR(buf.as_mut_ptr()),
                    &mut len,
                )
                .is_ok()
                {
                    let s = String::from_utf16_lossy(&buf[..len as usize]);
                    s.rsplit(['\\', '/']).next().unwrap_or(&s).to_string()
                } else {
                    tasklist_name(pid).unwrap_or_else(|| "?".into())
                }
            }
            Err(_) => tasklist_name(pid).unwrap_or_else(|| "<查无进程>".into()),
        }
    };
    format!("pid={pid} {name}")
}

thread_local! {
    static LINES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// 输出一行:stdout + 收集(最后落盘)。
fn p(line: impl AsRef<str>) {
    let line = line.as_ref();
    println!("{line}");
    LINES.with(|l| l.borrow_mut().push(line.to_string()));
}

fn os_info() -> String {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    let k = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    match k {
        Ok(k) => {
            let get = |name: &str| -> String {
                k.get_value::<String, _>(name).unwrap_or_else(|_| "?".into())
            };
            format!(
                "{} ({} build {})",
                get("ProductName"),
                get("DisplayVersion"),
                get("CurrentBuild")
            )
        }
        Err(e) => format!("注册表读取失败: {e}"),
    }
}

/// 单次 SetParent 探测:打印 crate 结果 + 原始 GetLastError + 真实父窗口。
unsafe fn probe(tag: &str, child: HWND, parent: HWND) {
    unsafe {
        SetLastError(WIN32_ERROR(0));
        let before = GetLastError().0;
        let r = SetParent(child, Some(parent));
        let raw_le = GetLastError().0;
        let actual = GetAncestor(child, GA_PARENT);
        match r {
            Ok(prev) => p(format!(
                "[{tag}] SetParent -> Ok(prev={:#x} class={}) | GetLastError(前={before} 后={raw_le}) | \
                 实际父 = {:#x} (期望 {:#x}, 匹配={})",
                hwnd_us(prev),
                class_of(prev),
                hwnd_us(actual),
                hwnd_us(parent),
                actual == parent
            )),
            Err(e) => p(format!(
                "[{tag}] SetParent -> Err({e}, code={:#x}) | GetLastError(前={before} 后={raw_le}) | \
                 实际父 = {:#x} (期望 {:#x}, 挂接实际成功={})",
                e.code().0,
                hwnd_us(actual),
                hwnd_us(parent),
                actual == parent
            )),
        }
    }
}

/// 经典样式(TRANSPARENT,无 WS_CHILD)后按 tag 探测,再还原为顶层无父。
unsafe fn classic_probe(tag: &str, child: HWND, parent: HWND, base_ex: isize, base_style: isize) {
    unsafe {
        SetWindowLongPtrW(child, GWL_EXSTYLE, base_ex | WS_EX_TRANSPARENT.0 as isize);
        SetWindowLongPtrW(child, GWL_STYLE, base_style);
        probe(tag, child, parent);
        let _ = SetParent(child, None);
        SetWindowLongPtrW(child, GWL_EXSTYLE, base_ex);
    }
}

fn write_result_file() -> std::path::PathBuf {
    let text = {
        let mut body = String::from("\u{feff}"); // BOM:Win10 记事本按 UTF-8 识别
        LINES.with(|l| {
            for line in l.borrow().iter() {
                body.push_str(line);
                body.push_str("\r\n");
            }
        });
        body
    };
    let mut path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("sp_probe_result.txt")))
        .unwrap_or_else(|| std::path::PathBuf::from("sp_probe_result.txt"));
    if std::fs::write(&path, &text).is_err() {
        path = std::path::PathBuf::from("sp_probe_result.txt");
        let _ = std::fs::write(&path, &text);
    }
    path
}

fn main() {
    p("== aria 壁纸层诊断 sp_probe v2 ==");
    p(format!("系统: {}", os_info()));

    let event_loop = winit::event_loop::EventLoop::new().unwrap();
    let attrs = winit::window::WindowAttributes::default()
        .with_title("sp_probe")
        .with_decorations(false)
        .with_resizable(false)
        .with_visible(false)
        .with_inner_size(winit::dpi::PhysicalSize::new(64, 64));
    let window = event_loop.create_window(attrs).unwrap();

    unsafe {
        use raw_window_handle::HasWindowHandle;
        let child = match window.window_handle().unwrap().as_raw() {
            raw_window_handle::RawWindowHandle::Win32(x) => {
                HWND(x.hwnd.get() as *mut core::ffi::c_void)
            }
            _ => panic!("非 Win32 窗口"),
        };

        p(format!(
            "子窗口(winit 无父顶层)hwnd={:#x},GWLP_HWNDPARENT={:#x}",
            hwnd_us(child),
            GetWindowLongPtrW(child, GWLP_HWNDPARENT)
        ));

        let progman = FindWindowW(w!("Progman"), PCWSTR::null()).unwrap_or_default();
        if progman.is_invalid() {
            p("!! 未找到 Progman —— shell 窗口布局异常(第三方 shell / Explorer 未运行?)");
            let path = write_result_file();
            println!("结果已写入 {}", path.display());
            std::process::exit(2);
        }
        let mut res = 0usize;
        let _ = SendMessageTimeoutW(
            progman,
            SPAWN_WORKER,
            WPARAM(0xD),
            LPARAM(0x1),
            SMTO_NORMAL,
            1000,
            Some(&mut res),
        );
        let raised =
            (GetWindowLongPtrW(progman, GWL_EXSTYLE) & WS_EX_NOREDIRECTIONBITMAP) != 0;
        p(format!(
            "Progman={:#x} (raised={raised}, {})",
            hwnd_us(progman),
            owner_of(progman)
        ));

        // ── DefView 位置:先查 Progman 直接子级;不在则枚举顶层找容器 ──
        let mut defview =
            FindWindowExW(Some(progman), None, w!("SHELLDLL_DefView"), PCWSTR::null())
                .unwrap_or_default();
        let mut container = HWND::default();
        if !defview.is_invalid() {
            container = progman;
            p(format!(
                "DefView={:#x} 在 Progman 下(标准布局)",
                hwnd_us(defview)
            ));
        } else {
            // 应用 classic 分支的容器循环:找"直接子级含 DefView"的顶层窗口
            let mut after: Option<HWND> = None;
            for _ in 0..512 {
                let wnd =
                    FindWindowExW(None, after, None, PCWSTR::null()).unwrap_or_default();
                if wnd.is_invalid() {
                    break;
                }
                let dv = FindWindowExW(
                    Some(wnd),
                    None,
                    w!("SHELLDLL_DefView"),
                    PCWSTR::null(),
                )
                .unwrap_or_default();
                if !dv.is_invalid() {
                    defview = dv;
                    container = wnd;
                    break;
                }
                after = Some(wnd);
            }
            if !container.is_invalid() {
                p(format!(
                    "!! DefView={:#x} 不在 Progman 下,实际挂在顶层 {}={:#x} ({}) —— \
                     布局被第三方改过(其他壁纸/桌面软件)",
                    hwnd_us(defview),
                    class_of(container),
                    hwnd_us(container),
                    owner_of(container)
                ));
            } else {
                p("!! 全部顶层窗口的直接子级里都没有 SHELLDLL_DefView —— 图标层异常");
            }
        }

        // ── 顶层 WorkerW 清单(Z 序,最多 8 个)──
        let mut workerws: Vec<HWND> = Vec::new();
        let mut after: Option<HWND> = None;
        for _ in 0..512 {
            let wnd = FindWindowExW(None, after, None, PCWSTR::null()).unwrap_or_default();
            if wnd.is_invalid() {
                break;
            }
            if class_of(wnd) == "WorkerW" {
                workerws.push(wnd);
                if workerws.len() >= 8 {
                    break;
                }
            }
            after = Some(wnd);
        }
        if workerws.is_empty() {
            p("顶层 WorkerW:无");
        } else {
            p(format!("顶层 WorkerW 共 {} 个(按 Z 序):", workerws.len()));
            for (i, ww) in workerws.iter().enumerate() {
                p(format!(
                    "  #{} hwnd={:#x} {}",
                    i + 1,
                    hwnd_us(*ww),
                    owner_of(*ww)
                ));
            }
        }

        // 公共样式调整(与 win.rs 一致)
        let base_ex = GetWindowLongPtrW(child, GWL_EXSTYLE)
            | WS_EX_NOACTIVATE.0 as isize
            | WS_EX_TOOLWINDOW.0 as isize;
        let mut base_style = GetWindowLongPtrW(child, GWL_STYLE);
        base_style &= !(WS_POPUP.0 as isize | WS_CAPTION.0 as isize);

        // ── A:Win11 raised 路径复刻(WS_CHILD + WS_EX_LAYERED,挂 Progman)──
        let mut ex = base_ex | WS_EX_LAYERED.0 as isize;
        SetWindowLongPtrW(child, GWL_EXSTYLE, ex);
        let mut style = base_style | WS_CHILD.0 as isize;
        SetWindowLongPtrW(child, GWL_STYLE, style);
        let _ = SetLayeredWindowAttributes(child, COLORREF(0), 255, LWA_ALPHA);
        probe("A raised: WS_CHILD+LAYERED→Progman", child, progman);
        // 还原
        let _ = SetParent(child, None);
        ex &= !WS_EX_LAYERED.0 as isize;
        SetWindowLongPtrW(child, GWL_EXSTYLE, ex);
        style &= !WS_CHILD.0 as isize;
        SetWindowLongPtrW(child, GWL_STYLE, style);

        // ── B1:应用 classic 主查找 —— DefView 容器之后的第一个顶层 WorkerW ──
        if !container.is_invalid() {
            let parent = FindWindowExW(None, Some(container), w!("WorkerW"), PCWSTR::null())
                .unwrap_or_default();
            if parent.is_invalid() {
                p("B1 主查找:DefView 容器之后无 WorkerW(未命中)");
            } else {
                p(format!(
                    "B1 主查找目标 = 容器 {} 之后的 WorkerW {:#x} ({})",
                    class_of(container),
                    hwnd_us(parent),
                    owner_of(parent)
                ));
                classic_probe("B1 classic: 容器后WorkerW", child, parent, base_ex, base_style);
            }
        }

        // ── B2:应用兜底 —— 任意第一个顶层 WorkerW ──
        let any_ww = FindWindowExW(None, None, w!("WorkerW"), PCWSTR::null())
            .unwrap_or_default();
        if any_ww.is_invalid() {
            p("B2 兜底:无任何顶层 WorkerW(应用会退回挂 Progman 压底)");
            classic_probe("B2b classic: 退回Progman", child, progman, base_ex, base_style);
        } else {
            p(format!(
                "B2 兜底目标 = Z 序第一个 WorkerW {:#x} ({})",
                hwnd_us(any_ww),
                owner_of(any_ww)
            ));
            classic_probe("B2 classic: 任意WorkerW兜底", child, any_ww, base_ex, base_style);
        }

        // ── C:对照 —— 不设任何样式,原样 SetParent 到 Progman ──
        probe("C 裸调用→Progman", child, progman);
    }

    let path = write_result_file();
    println!("结果已写入 {}", path.display());
    std::process::exit(0);
}
