//! Linux / Wayland 平台层。
//!
//! 只放**父进程(设置窗口)与子进程都要用**的公共类型和显示器枚举;
//! 真正把画面挂到桌面背景层的协议驱动在 [`crate::wall::linux`]
//! (wlr-layer-shell-unstable-v1: KWin 6 / wlroots 系合成器都支持)。
//!
//! 只考虑 Wayland:X11 / XWayland 后端在 Cargo.toml 里已经被关掉,
//! 没有 xcb 相关代码路径。

use smithay_client_toolkit::{
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::wl_output,
    Connection, QueueHandle,
};

/// 一台显示器在合成器**逻辑坐标**里的矩形(x, y, w, h)。
/// 与 Windows 侧 [`crate::win::MonitorBounds`] 同义,子进程按它选输出。
pub type MonitorBounds = (i32, i32, u32, u32);

/// 一台显示器的描述。`device` 用 Wayland 输出名(`DP-1` / `eDP-1` …),
/// 跨会话稳定,持久化选择用。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Monitor {
    pub device: String,
    pub model: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub primary: bool,
    /// 输出缩放因子(整数;KWin 分数缩放下按物理像素算)。
    pub scale: i32,
    /// 当前模式的刷新率(Hz,读不到按 60)。
    pub refresh_hz: u32,
}

/// 壁纸宿主:Wayland 下没有父窗口/子窗口的概念,只有当前面尺寸。
#[derive(Clone, Copy)]
pub struct Host {
    pub width: u32,
    pub height: u32,
}

/// 枚举当前所有输出(短连接 + roundtrip,几十毫秒)。
/// 失败(没有 Wayland 会话等)返回空表 —— 调用方按"无显示器"处理。
pub fn list_monitors() -> Vec<Monitor> {
    let Ok(conn) = Connection::connect_to_env() else {
        log::warn!("[wayland] 连接合成器失败(不是 Wayland 会话?)");
        return Vec::new();
    };
    let Ok((globals, mut queue)) = registry_queue_init::<Probe>(&conn) else {
        log::warn!("[wayland] 读取 wl_registry 失败");
        return Vec::new();
    };
    let qh = queue.handle();
    let mut probe = Probe {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
    };
    if queue.roundtrip(&mut probe).is_err() {
        return Vec::new();
    }

    let mut out = Vec::new();
    for (i, output) in probe.output_state.outputs().enumerate() {
        let Some(info) = probe.output_state.info(&output) else { continue };
        let (x, y) = info.logical_position.unwrap_or(info.location);
        // 逻辑尺寸优先(xdg-output);拿不到就退回当前模式像素尺寸。
        let (w, h) = match info.logical_size {
            Some(s) => s,
            None => info
                .modes
                .iter()
                .find(|m| m.current)
                .map(|m| m.dimensions)
                .unwrap_or((0, 0)),
        };
        if w <= 0 || h <= 0 {
            continue;
        }
        let refresh_hz = info
            .modes
            .iter()
            .find(|m| m.current)
            .map(|m| (m.refresh_rate / 1000).max(1) as u32)
            .unwrap_or(60);
        let device = info
            .name
            .clone()
            .or_else(|| info.description.clone())
            .unwrap_or_else(|| format!("output-{}", info.id));
        out.push(Monitor {
            device,
            model: if info.model.is_empty() { info.make.clone() } else { info.model.clone() },
            x,
            y,
            w,
            h,
            // Wayland 没有"主屏"概念:按注册顺序首个当默认屏。
            primary: i == 0,
            scale: info.scale_factor.max(1),
            refresh_hz,
        });
    }
    // 同坐标时按名字排序,让"第一个"在多次枚举间稳定。
    out.sort_by(|a, b| (a.x, a.y, &a.device).cmp(&(b.x, b.y, &b.device)));
    if let Some(first) = out.first_mut() {
        first.primary = true;
    }
    out
}

/// 按 bounds 找输出(找不到退回主屏/首个)。
pub fn pick(monitors: &[Monitor], bounds: Option<MonitorBounds>) -> Option<&Monitor> {
    let by_bounds = bounds.and_then(|(x, y, w, h)| {
        monitors.iter().find(|m| m.x == x && m.y == y && m.w == w as i32 && m.h == h as i32)
    });
    by_bounds
        .or_else(|| monitors.iter().find(|m| m.primary))
        .or_else(|| monitors.first())
}

/// 目标屏刷新率(bounds 为空/失配时取主屏)。
pub fn refresh_hz(bounds: Option<MonitorBounds>) -> u32 {
    let monitors = list_monitors();
    pick(&monitors, bounds).map(|m| m.refresh_hz).unwrap_or(60)
}

/// Linux 没有 Windows 那种工作集裁剪,保留同名空实现让调用点统一。
pub fn trim_working_set() {}

/// 单实例窗口的 DPI 处理:Wayland 由合成器按 surface 的 buffer scale
/// 处理,无需进程级声明。
pub fn enable_per_monitor_dpi() {}

/// 显示器尺寸(**物理像素**:场景按物理分辨率渲染,QML 侧再按逻辑
/// 尺寸摆放,分数缩放下正好 1:1 映射,不会糊)。
pub fn host_of(bounds: Option<MonitorBounds>) -> Host {
    let monitors = list_monitors();
    match pick(&monitors, bounds) {
        Some(m) => Host {
            width: (m.w.max(1) as i64 * m.scale as i64) as u32,
            height: (m.h.max(1) as i64 * m.scale as i64) as u32,
        },
        None => Host { width: 1920, height: 1080 },
    }
}

// ---------- 枚举用的最小状态 ----------

struct Probe {
    registry_state: RegistryState,
    output_state: OutputState,
}

impl OutputHandler for Probe {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ProvidesRegistryState for Probe {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_registry!(Probe);
smithay_client_toolkit::delegate_dispatch2!(Probe);
