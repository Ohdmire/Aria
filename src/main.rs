//! Aria —— osu! storyboard 桌面动态壁纸播放器。
//!
//! - 默认:GPUI 窗口 + 系统托盘,spawn 自身 `--wallpaper` 渲染子进程;
//! - `--wallpaper`:winit + wgpu 渲染进程,stdin/stdout JSON-lines 受控。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod ctl;
mod ipc;
#[cfg(target_os = "linux")]
mod kde;
#[cfg(target_os = "linux")]
mod dmabuf;
mod gpui_ui;
mod hotkeys;
mod i18n;
mod lazer;
mod loader;
mod logging;
mod playlist;
mod settings;
mod soundtouch;
mod tray;
mod stable;
mod wall;

#[cfg(windows)]
pub use aria::win;
#[cfg(target_os = "linux")]
pub use aria::wayland as win;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--wallpaper") {
        std::process::exit(wall::main());
    }
    #[cfg(target_os = "linux")]
    if args.iter().any(|a| a == "--restore-wallpaper") {
        if let Err(e) = kde::restore_wallpaper() {
            eprintln!("还原桌面壁纸失败: {e}");
        }
        return;
    }
    let _ = args;
    gpui_ui::run();
}
