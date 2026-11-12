//! 系统托盘:统一走 [`gpui_tray`](https://crates.io/crates/gpui-tray)。
//!
//! 以前是自己写两套(Windows:`tray-icon` + 自建隐藏窗口的消息泵;Linux:GTK
//! 主循环 + `tray-icon`),菜单事件只能塞进全局队列、靠 250ms 心跳取走执行。
//!
//! 换成 gpui-tray 之后:
//! - Windows / macOS / Linux(StatusNotifierItem over D-Bus)同一份代码;
//! - 菜单项就是 `gpui::MenuItem` + 普通 gpui Action,点击由 gpui 直接分发;
//! - 不再需要额外线程、消息泵和轮询队列。
//!
//! 托盘实例必须一直存活(drop 掉图标就消失),所以放进 gpui 的 Global 里。

use gpui::{actions, App, MenuItem};

actions!(
    aria_tray,
    [ShowWindow, PrevTrack, TogglePlay, NextTrack, QuitApp]
);

/// 托盘实例的持有者(只为了不被 drop;字段不读是刻意的)。
struct TrayHandle(#[allow(dead_code)] gpui_tray::Tray);

impl gpui::Global for TrayHandle {}

/// 创建托盘。失败只记日志,不影响主程序正常运行。
pub fn init(cx: &mut App) {
    let mut builder = gpui_tray::Tray::builder()
        .tooltip("Aria")
        // 左键点图标 = 显示主窗口
        .on_activate(ShowWindow)
        .menu(|_cx| menu_items());
    if let Some(icon) = icon() {
        builder = builder.icon(icon);
    }
    match builder.build(cx) {
        Ok(tray) => cx.set_global(TrayHandle(tray)),
        Err(e) => log::warn!("[tray] 创建系统托盘失败: {e}"),
    }
}

/// 菜单:显示窗口 / 上一首 / 播放暂停 / 下一首 / 退出。
/// 打开菜单时按当前语言现组,语言切换后要 `refresh`。
fn menu_items() -> Vec<MenuItem> {
    vec![
        MenuItem::action(crate::t!("tray.show"), ShowWindow),
        MenuItem::Separator,
        MenuItem::action(crate::t!("action.prev"), PrevTrack),
        MenuItem::action(crate::t!("action.toggle"), TogglePlay),
        MenuItem::action(crate::t!("action.next"), NextTrack),
        MenuItem::Separator,
        MenuItem::action(crate::t!("action.quit"), QuitApp),
    ]
}

/// 语言改了之后重画托盘菜单。
pub fn refresh(cx: &mut gpui::App) {
    let Some(tray) = cx.try_global::<TrayHandle>().map(|h| h.0.clone()) else {
        return;
    };
    if let Err(e) = tray.refresh_menu(cx) {
        log::warn!("[tray] 刷新菜单失败: {e}");
    }
}

/// 托盘图标:和窗口图标同一张 32×32 PNG。
fn icon() -> Option<gpui_tray::Icon> {
    let rgba = image::load_from_memory(include_bytes!("../icons/32x32.png"))
        .ok()?
        .to_rgba8();
    let (w, h) = rgba.dimensions();
    gpui_tray::Icon::from_rgba(rgba.into_raw(), w, h).build().ok()
}
