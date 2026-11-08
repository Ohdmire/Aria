//! 系统托盘(独立 GTK 线程;GPUI 没有托盘 API,tray-icon 需要 GTK 主循环)。
//!
//! 菜单事件写进全局队列,GPUI 侧 250ms 心跳轮询执行(切歌/暂停/退出等)。

use std::sync::Mutex;

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

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

/// 在后台线程起托盘(失败静默:没有托盘也能用窗口)。
pub fn spawn() {
    std::thread::Builder::new()
        .name("aria-tray".into())
        .spawn(|| {
            if gtk::init().is_err() {
                log::warn!("[tray] gtk 初始化失败,托盘不可用");
                return;
            }
            MenuEvent::set_event_handler(Some(|e: MenuEvent| {
                match e.id().0.as_str() {
                    "show" => push(TrayAction::Show),
                    "prev" => push(TrayAction::Prev),
                    "toggle" => push(TrayAction::Toggle),
                    "next" => push(TrayAction::Next),
                    "quit" => push(TrayAction::Quit),
                    _ => {}
                }
            }));
            let menu = Menu::new();
            let _ = menu.append(&MenuItem::with_id("show", "显示窗口", true, None));
            let _ = menu.append(&PredefinedMenuItem::separator());
            let _ = menu.append(&MenuItem::with_id("prev", "上一首", true, None));
            let _ = menu.append(&MenuItem::with_id("toggle", "播放 / 暂停", true, None));
            let _ = menu.append(&MenuItem::with_id("next", "下一首", true, None));
            let _ = menu.append(&PredefinedMenuItem::separator());
            let _ = menu.append(&MenuItem::with_id("quit", "退出", true, None));

            let icon = include_bytes!("../icons/32x32.png");
            let icon = image::load_from_memory(icon)
                .ok()
                .map(|i| i.to_rgba8())
                .and_then(|rgba| {
                    let (w, h) = rgba.dimensions();
                    Icon::from_rgba(rgba.into_raw(), w, h).ok()
                });

            let mut builder = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("Aria");
            if let Some(icon) = icon {
                builder = builder.with_icon(icon);
            }
            // 托盘必须活到 gtk::main 结束
            let _tray = match builder.build() {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("[tray] 创建失败: {e}");
                    return;
                }
            };
            gtk::main();
        })
        .ok();
}
