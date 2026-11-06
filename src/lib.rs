//! 播放器里和窗口挂接有关的代码。探测程序与正式壁纸进程共用这一份。

#[cfg(windows)]
pub mod win;

#[cfg(target_os = "linux")]
pub mod wayland;