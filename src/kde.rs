//! KDE Plasma 壁纸插件桥。
//!
//! ## 为什么要走 Plasma 壁纸插件
//!
//! KWin 把 wlr-layer-shell 的 background/bottom 面放在 **plasmashell 桌面
//! 窗口之上**,直接挂 layer-shell 会盖住桌面图标。Plasma 的壁纸则是桌面
//! containment 里的一个 QML item,图标由同一个窗口画在它上面 —— 图标
//! 天然可见。做法与 wallpaper-engine-kde-plugin / Smart Video Wallpaper
//! Reborn 一致:安装一个 `Plasma/Wallpaper` KPackage,把渲染画面交给它
//! 显示。
//!
//! ## 画面怎么过去
//!
//! 渲染子进程仍在跑完整的 wgpu 场景(音频/时钟/控制在它手里),只是把
//! 每帧写进 POSIX 共享内存(双缓冲 + 序号);包里自带的 QML 控件
//! (`libariaplugin.so`,QQuickItem)按序号取最新帧上传成纹理,画在壁纸
//! item 里。不经过合成器、不需要屏幕捕获权限。
//!
//! 共享内存布局(小端):
//! ```text
//! +0    magic   u32  "ARIA"
//! +4    version u32  1
//! +8    width   u32
//! +12   height  u32
//! +16   stride  u32  每行字节数(可能 > width*4,GPU 回读按 256 对齐)
//! +20   format  u32  0 = BGRA8
//! +24   seq     u64  帧序号,0 = 尚无画面
//! +32   bytes   u64  单帧字节数
//! +64   保留到 HEADER_RESERVE
//! +4096 帧缓冲 A/B/C(三槽轮转)
//! ```
//! 写者:写缓冲 BUF[seq % 3] → 递增 seq。读者:读 seq → 取 BUF[(seq-1)%3]
//! 深拷贝 → 再核对 seq 未变。**三缓冲**:60/165fps 的写者与 ~240Hz 轮询
//! 的读者之间,写者要连续推进 3 帧才会绕回读者正在读的那块(≈18ms
//! @165fps),拷贝 14MB 只需 ~1ms,冲突实际不可能发生;真撞上也只是
//! 丢掉这一帧,绝不出撕裂画面。

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

// ---------- 插件包安装 ----------

/// KPackage 标识(目录名 / metadata.json 的 Id / plasma 配置里的插件名)。
pub const PLUGIN_ID: &str = "com.ohdmire.aria";

const PLUGIN_SO: &str = "libariaplugin.so";

/// 当前会话是不是 KDE(Wayland 或 X11 都能装 Plasma 插件;应用本身只在
/// Wayland 下支持)。
pub fn is_kde() -> bool {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let kde = desktop.split(':').any(|s| s.eq_ignore_ascii_case("KDE"));
    kde || std::env::var_os("KDE_FULL_SESSION").is_some()
}

/// Plasma 的用户级数据目录(`$XDG_DATA_HOME`,缺省 `~/.local/share`)。
fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

/// 已安装的壁纸插件目录。
pub fn plugin_dir() -> Option<PathBuf> {
    data_home().map(|d| d.join("plasma/wallpapers").join(PLUGIN_ID))
}

/// 插件是否完整可用(QML 入口 + 原生控件都在)。
pub fn plugin_installed() -> bool {
    plugin_dir().is_some_and(|d| {
        d.join("metadata.json").is_file()
            && d.join("contents/ui/main.qml").is_file()
            && d.join("contents/ui/aria").join(PLUGIN_SO).is_file()
    })
}

/// 随包分发的插件源目录(开发时在仓库里,安装后与可执行文件同级)。
fn package_source() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("ARIA_KDE_PACKAGE") {
        candidates.push(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("kde/wallpaper"));
            candidates.push(dir.join("share/aria/kde/wallpaper"));
            candidates.push(dir.join("../share/aria/kde/wallpaper"));
            candidates.push(dir.join("resources/kde/wallpaper"));
        }
    }
    // 开发构建:仓库里的源目录(编译期路径,只对本机开发有用)
    if let Some(dir) = option_env!("CARGO_MANIFEST_DIR") {
        candidates.push(Path::new(dir).join("kde/wallpaper"));
    }
    candidates.into_iter().find(|p| p.join("metadata.json").is_file())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("创建 {} 失败:{e}", dst.display()))?;
    let entries = std::fs::read_dir(src).map_err(|e| format!("读取 {} 失败:{e}", src.display()))?;
    for entry in entries.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            // 先写同目录临时文件再 rename:**绝不原地覆盖**。plasmashell
            // 可能正映射着旧的 libariaplugin.so,原地重写会让进程
            // SIGILL(实测)。rename 保留旧 inode,已加载的进程不受影响,
            // 下次加载才换新。
            let tmp = to.with_extension("aria-tmp");
            std::fs::copy(&from, &tmp)
                .map_err(|e| format!("复制 {} → {} 失败:{e}", from.display(), tmp.display()))?;
            std::fs::rename(&tmp, &to)
                .map_err(|e| format!("替换 {} 失败:{e}", to.display()))?;
        }
    }
    Ok(())
}

/// 把随包插件装到 `~/.local/share/plasma/wallpapers/com.ohdmire.aria/`
/// (幂等覆盖)。找不到源目录或缺少原生控件时报错,由调用方退回窗口模式。
pub fn install_plugin() -> Result<(), String> {
    let src = package_source().ok_or("找不到随包分发的 KDE 壁纸插件目录")?;
    if !src.join("contents/ui/aria").join(PLUGIN_SO).is_file() {
        return Err(format!(
            "插件缺少原生控件 {};请先运行 scripts/build-kde-plugin.sh",
            src.join("contents/ui/aria").join(PLUGIN_SO).display()
        ));
    }
    let dst = plugin_dir().ok_or("无法定位 Plasma 数据目录")?;
    copy_dir(&src, &dst)?;
    log::info!("[kde] 壁纸插件已安装: {}", dst.display());
    Ok(())
}

/// 备份文件:切换前把"原来的壁纸插件列表"落盘。进程被 `kill -9` /
/// 崩溃时内存里的备份会丢,下次启动靠这个文件还原,不会把 Aria 自己
/// 当成"原壁纸"而恢复成卡住的最后一帧。
fn backup_file() -> Option<PathBuf> {
    data_home()
        .map(|d| d.join("com.ohdmire.aria").join("kde-wallpaper.json"))
}

fn read_backup() -> Option<Vec<String>> {
    let text = std::fs::read_to_string(backup_file()?).ok()?;
    serde_json::from_str::<Vec<String>>(&text).ok().filter(|v| !v.is_empty())
}

fn write_backup(plugins: &[String]) {
    if let Some(path) = backup_file() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string(plugins) {
            let _ = std::fs::write(path, json);
        }
    }
}

fn clear_backup() {
    if let Some(path) = backup_file() {
        let _ = std::fs::remove_file(path);
    }
}

/// 当前每个桌面的壁纸插件 id。
fn current_plugins() -> Result<Vec<String>, String> {
    let out = run_plasma_script(r#"print(desktops().map(function(d){return d.wallpaperPlugin;}).join(","));"#)?;
    Ok(out.trim().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

/// 桌面壁纸当前是不是 Aria 插件。
pub fn is_active() -> bool {
    current_plugins().map(|v| v.iter().any(|p| p == PLUGIN_ID)).unwrap_or(false)
}

fn switch_to(plugins: &[String]) -> Result<(), String> {
    if plugins.is_empty() {
        return Ok(());
    }
    let list = plugins
        .iter()
        .map(|s| format!("\"{}\"", s.replace('"', "")))
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        r#"
        var ds = desktops();
        var ids = [{list}];
        for (var i = 0; i < ds.length; i++) {{
            var id = ids[Math.min(i, ids.length - 1)];
            if (id && id !== "") {{ ds[i].wallpaperPlugin = id; }}
        }}
        "#
    );
    run_plasma_script(&script).map(|_| ())
}

/// 用 PlasmaShell 的脚本接口把桌面壁纸切换成 Aria。
/// 返回切换前的壁纸插件列表(用于退出时还原)。
///
/// 如果切换前已经是 Aria(上次崩溃/被强杀没还原),用落盘的备份当
/// "原壁纸",避免把 Aria 自己记成原壁纸 —— 那会导致退出时"还原"到
/// 自己、桌面停在最后一帧。
pub fn enable_wallpaper() -> Result<Vec<String>, String> {
    let current = current_plugins().unwrap_or_default();
    let previous = if current.iter().any(|p| p == PLUGIN_ID) {
        read_backup().unwrap_or_default()
    } else {
        current.clone()
    };
    if !previous.is_empty() {
        write_backup(&previous);
    }
    let targets = vec![PLUGIN_ID.to_string(); current.len().max(1)];
    switch_to(&targets)?;
    Ok(previous)
}

/// 退出时还原桌面壁纸:**只在当前仍是 Aria 时**还原(用户中途自己换了
/// 壁纸就尊重用户的选择)。没有可用备份时退回 KDE 的图片壁纸。
pub fn restore_wallpaper() -> Result<(), String> {
    let current = match current_plugins() {
        Ok(v) => v,
        Err(e) => return Err(e),
    };
    if !current.iter().any(|p| p == PLUGIN_ID) {
        clear_backup();
        return Ok(());
    }
    let mut backup = read_backup().unwrap_or_default();
    backup.retain(|p| p != PLUGIN_ID);
    if backup.is_empty() {
        backup = vec!["org.kde.image".to_string()];
    }
    switch_to(&backup)?;
    clear_backup();
    Ok(())
}

/// qdbus6 > qdbus(Plasma 6 的脚本接口签名两代一致)。
fn run_plasma_script(script: &str) -> Result<String, String> {
    for dbus in ["qdbus6", "qdbus"] {
        let out = std::process::Command::new(dbus)
            .args([
                "org.kde.plasmashell",
                "/PlasmaShell",
                "org.kde.PlasmaShell.evaluateScript",
                script,
            ])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                return Ok(String::from_utf8_lossy(&o.stdout).into_owned());
            }
            Ok(o) => {
                return Err(format!(
                    "PlasmaScript 失败:{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                ));
            }
            Err(_) => continue,
        }
    }
    Err("找不到 qdbus6/qdbus,无法切换 Plasma 壁纸".into())
}

// ---------- 共享内存发布端(渲染子进程) ----------

const MAGIC: u32 = 0x4149_5241; // "ARIA"
const VERSION: u32 = 3;
/// header.format:0 = BGRA8 共享内存帧,1 = dmabuf(槽位 fd 走 unix socket)。
const FORMAT_BGRA8: u32 = 0;
const FORMAT_DMABUF: u32 = 1;
const HEADER_RESERVE: usize = 4096;
/// 场景长边封顶 2560、16:9 → 最大 2560×1440(`wall::SCENE_MAX_LONG`)。
const MAX_W: usize = 2560;
const MAX_H: usize = 1440;
const MAX_STRIDE: usize = MAX_W * 4;
const MAX_FRAME: usize = MAX_STRIDE * MAX_H;
/// 缓冲槽数(写者 seq % SLOTS;读者 (seq-1) % SLOTS)。
pub const SLOTS: usize = 3;
const SHM_LEN: usize = HEADER_RESERVE + MAX_FRAME * SLOTS;

/// 共享内存文件名(每用户一个,`/dev/shm/aria-wallpaper-<uid>.shm`)。
pub fn shm_name() -> String {
    let uid = unsafe { libc::getuid() };
    format!("/aria-wallpaper-{uid}.shm")
}

/// 帧发布端:映射共享内存,把 GPU 回读的 BGRA 帧双缓冲写入。
/// 只在渲染线程上用(非 Send 也无妨)。
pub struct Publisher {
    base: *mut u8,
    header: *mut Header,
    /// 共享内存名(诊断用)。
    name: String,
}

#[repr(C)]
struct Header {
    magic: u32,
    version: u32,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
    /// 原子递增(跨进程可见;渲染线程写,QML 线程读)。
    seq: AtomicU64,
    bytes: u64,
}

impl Publisher {
    /// 打开/创建共享内存并重置为"尚无画面"。
    pub fn connect() -> Result<Publisher, String> {
        let name = shm_name();
        let cname = CString::new(name.clone()).map_err(|e| e.to_string())?;
        unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600);
            if fd < 0 {
                return Err(format!(
                    "shm_open({name}) 失败:{}",
                    std::io::Error::last_os_error()
                ));
            }
            if libc::ftruncate(fd, SHM_LEN as libc::off_t) != 0 {
                let err = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(format!("ftruncate({name}) 失败:{err}"));
            }
            let base = libc::mmap(
                std::ptr::null_mut(),
                SHM_LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            libc::close(fd);
            if base == libc::MAP_FAILED {
                return Err(format!(
                    "mmap({name}) 失败:{}",
                    std::io::Error::last_os_error()
                ));
            }
            let base = base as *mut u8;
            let header = base as *mut Header;
            std::ptr::write(
                header,
                Header {
                    magic: MAGIC,
                    version: VERSION,
                    width: 0,
                    height: 0,
                    stride: 0,
                    format: FORMAT_BGRA8,
                    seq: AtomicU64::new(0),
                    bytes: 0,
                },
            );
            Ok(Publisher { base, header, name })
        }
    }

    /// 发布一帧(BGRA8;`data` 行距 = `stride`,必须覆盖 height 行)。
    /// 尺寸/行距变化时先更新头再递增序号,读者只会看到一致的帧。
    pub fn publish(&mut self, width: u32, height: u32, stride: u32, data: &[u8]) {
        let frame = stride as usize * height as usize;
        if frame == 0
            || frame > MAX_FRAME
            || stride as usize > MAX_STRIDE
            || width as usize > MAX_W
            || height as usize > MAX_H
            || data.len() < frame
        {
            return;
        }
        let seq = unsafe { (*self.header).seq.load(Ordering::Relaxed) };
        let idx = (seq % SLOTS as u64) as usize;
        unsafe {
            let (w, h, s, b) = (
                (*self.header).width,
                (*self.header).height,
                (*self.header).stride,
                (*self.header).bytes,
            );
            if w != width || h != height || s != stride || b != frame as u64 {
                *std::ptr::addr_of_mut!((*self.header).width) = width;
                *std::ptr::addr_of_mut!((*self.header).height) = height;
                *std::ptr::addr_of_mut!((*self.header).stride) = stride;
                *std::ptr::addr_of_mut!((*self.header).bytes) = frame as u64;
            }
            let dst = self.base.add(HEADER_RESERVE + idx * MAX_FRAME);
            std::ptr::copy_nonoverlapping(data.as_ptr(), dst, frame);
            (*self.header).seq.store(seq + 1, Ordering::Release);
        }
    }

    /// dmabuf 模式:只更新几何 + 递增序号(像素在 dmabuf 里,不拷)。
    pub fn publish_dmabuf(&mut self, width: u32, height: u32, stride: u32) {
        if width == 0 || height == 0 || width as usize > MAX_W || height as usize > MAX_H {
            return;
        }
        let seq = unsafe { (*self.header).seq.load(Ordering::Relaxed) };
        unsafe {
            let (w, h, s, f) = (
                (*self.header).width,
                (*self.header).height,
                (*self.header).stride,
                (*self.header).format,
            );
            if w != width || h != height || s != stride || f != FORMAT_DMABUF {
                *std::ptr::addr_of_mut!((*self.header).width) = width;
                *std::ptr::addr_of_mut!((*self.header).height) = height;
                *std::ptr::addr_of_mut!((*self.header).stride) = stride;
                *std::ptr::addr_of_mut!((*self.header).format) = FORMAT_DMABUF;
                *std::ptr::addr_of_mut!((*self.header).bytes) = 0;
            }
            (*self.header).seq.store(seq + 1, Ordering::Release);
        }
    }

    /// 诊断:共享内存名 + 累计发布帧数。
    pub fn debug_info(&self) -> String {
        let seq = unsafe { (*self.header).seq.load(Ordering::Relaxed) };
        format!("{} (seq {seq})", self.name)
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, SHM_LEN);
        }
    }
}

// ---------- 离屏渲染输出端(渲染会话用) ----------

use osu_replay_render::draw::{Atlas, DrawList};
use osu_replay_render::render::Renderer;

/// 离屏渲染 + 共享内存发布:`SurfaceRenderer` 在 KDE 模式下的替身。
/// 场景照旧按 16:9 内部尺寸渲染;QML 控件负责 letterbox 到实际桌面比例。
pub struct ShmSink {
    renderer: Renderer,
    publisher: Publisher,
    buf: Vec<u8>,
    /// 首帧立即回读(之后走一帧流水线,避免每帧等 GPU)。
    started: bool,
    /// dmabuf 零拷贝导出(可用 ARIA_DMABUF=0 关掉;失败自动退回拷贝路径)。
    dmabuf: Option<crate::dmabuf::Exporter>,
}

impl Drop for ShmSink {
    fn drop(&mut self) {
        // 必须在 renderer/设备销毁前释放导出纹理,否则 drop 回调里的
        // vkDestroyImage 会拿到悬空设备。
        self.dmabuf = None;
    }
}

impl ShmSink {
    pub fn new(width: u32, height: u32, atlas: &Atlas) -> Result<ShmSink, String> {
        let publisher = Publisher::connect()?;
        let renderer = Renderer::new(width, height, atlas);
        log::info!("[kde] 共享内存发布端就绪: {}", publisher.debug_info());
        let dmabuf = if std::env::var("ARIA_DMABUF").map(|v| v == "0").unwrap_or(false) {
            None
        } else {
            match crate::dmabuf::Exporter::new(&renderer) {
                Ok(e) => Some(e),
                Err(e) => {
                    log::warn!("[kde] dmabuf 不可用,退回共享内存拷贝: {e}");
                    None
                }
            }
        };
        Ok(ShmSink { renderer, publisher, buf: Vec::new(), started: false, dmabuf })
    }

    pub fn scene_size(&self) -> (u32, u32) {
        (self.renderer.width, self.renderer.height)
    }

    pub fn set_atlas(&mut self, atlas: &Atlas) {
        self.renderer.set_atlas(atlas);
    }

    /// 场景尺寸在构造时固定(共享内存按场景最大尺寸预留),窗口尺寸变化
    /// 不需要重配 —— QML 侧按 item 大小缩放。
    pub fn resize(&mut self, _width: u32, _height: u32) {}

    pub fn device(&self) -> &wgpu::Device {
        self.renderer.device()
    }

    pub fn queue(&self) -> &wgpu::Queue {
        self.renderer.queue()
    }

    pub fn renderer_mut(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// 提交一帧并发布。返回是否发布成功。
    pub fn render(&mut self, list: &DrawList, clear: [f64; 4]) -> bool {
        if self.dmabuf.is_some() {
            // 零拷贝:场景渲进 target → wgpu 把 target 拷进 dmabuf 图像
            // (纯 GPU)→ 序号递增,plasmashell 直接采样。没有 CPU 像素拷贝。
            // 不走 render_deferred:它会额外把 target 拷进回读环形缓冲
            // (CPU 路径用的),dmabuf 模式下会把环塞满并 panic。
            // 直接 encode + submit,再让 exporter 把 target 拷进 dmabuf 图像。
            let enc = self.renderer.encode_scene(list, clear);
            self.renderer.queue().submit(Some(enc.finish()));
            let Some(exp) = &mut self.dmabuf else { return false };
            let Ok(idx) = exp.copy_frame(&self.renderer) else { return false };
            let (w, h) = exp.size();
            let stride = exp.slot_stride(idx);
            self.publisher.publish_dmabuf(w, h, stride);
            return true;
        }
        self.renderer.render_deferred(list, clear);
        let pending = self.renderer.pending_len();
        // 首帧立即取(让画面尽快出现);之后保持 1 帧在 GPU 里,取更旧的
        // 那帧 —— 回读不阻塞提交,帧率更稳。
        if pending == 0 || (self.started && pending < 2) {
            return false;
        }
        self.started = true;
        self.renderer.read_oldest_into(&mut self.buf);
        let (w, h, stride) =
            (self.renderer.width, self.renderer.height, self.renderer.padded_row);
        self.publisher.publish(w, h, stride, &self.buf);
        true
    }
}