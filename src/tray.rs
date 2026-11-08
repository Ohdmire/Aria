//! 系统托盘:独立线程 + 该平台自己的消息循环(GPUI 没有托盘 API)。
//!
//! 菜单事件写进全局队列,GPUI 侧 250ms 心跳轮询执行(切歌/暂停/退出等)。
//!
//! - Linux:GTK 主循环 + `tray-icon`(libayatana-appindicator);
//! - Windows:`tray-icon` 的 Win32 后端(它自己建隐藏窗口),线程里跑消息泵。

use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    Show,
    Prev,
    Toggle,
    Next,
    Quit,
}

static ACTIONS: Mutex<Vec<TrayAction>> = Mutex::new(Vec::new());

/// 取出待执行动作(由 GPUI 心跳调用)。
pub fn poll() -> Vec<TrayAction> {
    std::mem::take(&mut *ACTIONS.lock().unwrap())
}

fn push(a: TrayAction) {
    ACTIONS.lock().unwrap().push(a);
}

/// 菜单 id → 动作(两个平台共用)。
fn dispatch(id: &str) {
    match id {
        "show" => push(TrayAction::Show),
        "prev" => push(TrayAction::Prev),
        "toggle" => push(TrayAction::Toggle),
        "next" => push(TrayAction::Next),
        "quit" => push(TrayAction::Quit),
        _ => {}
    }
}

#[cfg(any(target_os = "linux", windows))]
mod sys {
    use super::dispatch;
    use tray_icon::TrayIcon;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIconBuilder};

    /// 菜单:显示窗口 / 上一首 / 播放暂停 / 下一首 / 退出。
    fn menu() -> Menu {
        let menu = Menu::new();
        let _ = menu.append(&MenuItem::with_id("show", "显示窗口", true, None));
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&MenuItem::with_id("prev", "上一首", true, None));
        let _ = menu.append(&MenuItem::with_id("toggle", "播放 / 暂停", true, None));
        let _ = menu.append(&MenuItem::with_id("next", "下一首", true, None));
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&MenuItem::with_id("quit", "退出", true, None));
        menu
    }

    /// 托盘图标:和窗口图标同一张 32×32 PNG。
    fn icon() -> Option<Icon> {
        let rgba = image::load_from_memory(include_bytes!("../icons/32x32.png"))
            .ok()?
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        Icon::from_rgba(rgba.into_raw(), w, h).ok()
    }

    /// 建托盘。返回的 `TrayIcon` 必须活到消息循环结束,drop 掉图标就没了。
    pub(super) fn create() -> Option<TrayIcon> {
        MenuEvent::set_event_handler(Some(|e: MenuEvent| dispatch(e.id().0.as_str())));
        let mut builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu()))
            .with_tooltip("Aria");
        if let Some(icon) = icon() {
            builder = builder.with_icon(icon);
        }
        match builder.build() {
            Ok(t) => Some(t),
            Err(e) => {
                log::warn!("[tray] 创建失败: {e}");
                None
            }
        }
    }
}

/// 在后台线程起托盘(失败静默:没有托盘也能用窗口)。
#[cfg(target_os = "linux")]
pub fn spawn() {
    std::thread::Builder::new()
        .name("aria-tray".into())
        .spawn(|| {
            if gtk::init().is_err() {
                log::warn!("[tray] gtk 初始化失败,托盘不可用");
                return;
            }
            // 托盘必须活到 gtk::main 结束
            let Some(_tray) = sys::create() else { return };
            gtk::main();
        })
        .ok();
}

/// 在后台线程起托盘(失败静默:没有托盘也能用窗口)。
///
/// Win32 下托盘图标依附在这个线程上创建的隐藏窗口,所以消息泵也由这个线程跑;
/// 菜单回调(TrackPopupMenu 的模态循环)同样在这个泵里完成,不需要 GPUI 帮忙。
#[cfg(windows)]
pub fn spawn() {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, MSG, TranslateMessage,
    };

    std::thread::Builder::new()
        .name("aria-tray".into())
        .spawn(|| {
            let Some(_tray) = sys::create() else { return };
            unsafe {
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        })
        .ok();
}

/// 其它平台:没有托盘实现,静默降级。
#[cfg(not(any(target_os = "linux", windows)))]
pub fn spawn() {}