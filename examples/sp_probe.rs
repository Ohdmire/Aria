//! 与正式壁纸进程同一条窗口路径,不另写一套 Win32。
//!
//! `src/wall.rs` `create_window` + `src/win.rs` `attach` / `reattach` /
//! `show_no_activate`,然后按 `osu_replay_render` 的 `SurfaceRenderer`
//! 建 wgpu 交换链(Fifo、Bgra8Unorm、Opaque),把 BG.jpg 画上去,3 秒后退出。
//!
//! 日志: exe 同目录 wall_probe.log(含 win.rs 自己的 log 行)
//! 编译: cargo build --release --example sp_probe

use std::fs::OpenOptions;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowAttributes};

const HOLD: Duration = Duration::from_secs(3);
const EMBEDDED_BG: &[u8] = include_bytes!("BG.jpg");

struct FileLog {
    file: Mutex<std::fs::File>,
}

fn keep_log(target: &str) -> bool {
    target == "sp_probe" || target.starts_with("aria")
}

impl log::Log for FileLog {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        keep_log(meta.target())
    }
    fn log(&self, record: &log::Record) {
        if !keep_log(record.target()) {
            return;
        }
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let row = format!("[{ms}] {} {}\n", record.target(), record.args());
        print!("{row}");
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(row.as_bytes());
            let _ = f.flush();
        }
    }
    fn flush(&self) {}
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn init_log(dir: &Path) {
    let path = dir.join("wall_probe.log");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap_or_else(|e| panic!("无法写日志 {}: {e}", path.display()));
    let _ = file.write_all(&[0xEF, 0xBB, 0xBF]);
    let logger = Box::leak(Box::new(FileLog { file: Mutex::new(file) }));
    let _ = log::set_logger(logger);
    log::set_max_level(log::LevelFilter::Info);
    log::info!("日志: {}", path.display());
}

fn block_on<F: std::future::Future>(mut fut: F) -> F::Output {
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    unsafe fn clone(p: *const ()) -> RawWaker {
        RawWaker::new(p, &VTABLE)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake, drop);
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    loop {
        match unsafe { std::pin::Pin::new_unchecked(&mut fut) }.poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

struct Image {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
}

fn log_os() {
    let key = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let Ok(key) = key else {
        log::info!("系统: 读注册表失败");
        return;
    };
    let product: String = key.get_value("ProductName").unwrap_or_default();
    let display: String = key.get_value("DisplayVersion").unwrap_or_default();
    let build: String = key.get_value("CurrentBuild").unwrap_or_default();
    let ubr: u32 = key.get_value("UBR").unwrap_or(0);
    log::info!("系统: {product} {display} build {build}.{ubr}");
}

fn log_diag(ours: windows::Win32::Foundation::HWND) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowExW, FindWindowW, GWL_EXSTYLE, GWL_STYLE, GetAncestor, GetClassNameW, GetClientRect,
        GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible, GA_PARENT,
    };
    use windows::core::{PCWSTR, w};

    unsafe fn class_of(h: HWND) -> String {
        let mut buf = [0u16; 64];
        let n = GetClassNameW(h, &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
    unsafe fn owner(h: HWND) -> String {
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == 0 {
            return "pid=?".into();
        }
        let name = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok().and_then(|p| {
            let mut buf = [0u16; 260];
            let mut len = buf.len() as u32;
            QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len)
                .ok()
                .map(|_| String::from_utf16_lossy(&buf[..len as usize]))
        });
        match name {
            Some(n) => format!("pid={pid} {n}"),
            None => format!("pid={pid}"),
        }
    }
    unsafe fn line(i: u32, h: HWND) -> String {
        let mut wr = windows::Win32::Foundation::RECT::default();
        let _ = GetWindowRect(h, &mut wr);
        let mut cr = windows::Win32::Foundation::RECT::default();
        let _ = GetClientRect(h, &mut cr);
        let vis = IsWindowVisible(h).as_bool();
        format!(
            "  #{i} {} {:#x} vis={vis} win={}x{} @({},{}) client={}x{} style={:#x} ex={:#x} {}",
            class_of(h),
            h.0 as usize,
            wr.right - wr.left,
            wr.bottom - wr.top,
            wr.left,
            wr.top,
            cr.right - cr.left,
            cr.bottom - cr.top,
            GetWindowLongPtrW(h, GWL_STYLE),
            GetWindowLongPtrW(h, GWL_EXSTYLE),
            owner(h)
        )
    }

    unsafe {
        let progman = FindWindowW(w!("Progman"), w!("Program Manager")).unwrap_or_default();
        if progman.is_invalid() {
            log::info!("层级: 未找到 Progman");
            return;
        }
        let parent = GetAncestor(ours, GA_PARENT);
        log::info!(
            "壁纸窗口 {:#x} 父={} {:#x}",
            ours.0 as usize,
            class_of(parent),
            parent.0 as usize
        );
        log::info!("{}", line(0, ours));
        log::info!("Progman 直接子窗口(从上到下):");
        let mut after = None;
        let mut n = 0u32;
        for _ in 0..32 {
            let c = FindWindowExW(Some(progman), after, None, PCWSTR::null()).unwrap_or_default();
            if c.is_invalid() {
                break;
            }
            n += 1;
            log::info!("{}", line(n, c));
            after = Some(c);
        }
    }
}

fn load_bg(dir: &Path) -> Image {
    let path = dir.join("BG.jpg");
    if !path.is_file() {
        std::fs::write(&path, EMBEDDED_BG).expect("写 BG.jpg");
    }
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    log::info!("背景文件: {} ({} 字节)", path.display(), len);
    let img = decode_jpg(&path);
    let nonzero = img.rgba.chunks_exact(4).filter(|p| (p[0] | p[1] | p[2]) != 0).count();
    log::info!("解码: {}x{} RGBA 非透明色像素 {nonzero}", img.w, img.h);
    if nonzero == 0 {
        panic!("BG.jpg 解码结果是空的");
    }
    img
}

fn decode_jpg(path: &Path) -> Image {
    // GDI+ 只负责把文件变成像素。窗口和交换链不经过这里。
    #[repr(C)]
    struct Startup {
        version: u32,
        cb: *mut core::ffi::c_void,
        bg: i32,
        ext: i32,
    }
    #[repr(C)]
    struct Rect {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    }
    #[repr(C)]
    struct Bits {
        w: u32,
        h: u32,
        stride: i32,
        fmt: i32,
        scan: *mut u8,
        reserved: usize,
    }
    #[link(name = "gdiplus")]
    unsafe extern "system" {
        fn GdiplusStartup(token: *mut usize, input: *const Startup, output: *mut core::ffi::c_void) -> i32;
        fn GdiplusShutdown(token: usize);
        fn GdipCreateBitmapFromFile(name: *const u16, bmp: *mut *mut core::ffi::c_void) -> i32;
        fn GdipGetImageWidth(img: *mut core::ffi::c_void, w: *mut u32) -> i32;
        fn GdipGetImageHeight(img: *mut core::ffi::c_void, h: *mut u32) -> i32;
        fn GdipBitmapLockBits(bmp: *mut core::ffi::c_void, r: *const Rect, flags: u32, fmt: i32, data: *mut Bits) -> i32;
        fn GdipBitmapUnlockBits(bmp: *mut core::ffi::c_void, data: *mut Bits) -> i32;
        fn GdipDisposeImage(img: *mut core::ffi::c_void) -> i32;
    }
    unsafe {
        let mut token = 0usize;
        let st = GdiplusStartup(
            &mut token,
            &Startup { version: 1, cb: std::ptr::null_mut(), bg: 0, ext: 0 },
            std::ptr::null_mut(),
        );
        assert_eq!(st, 0, "GdiplusStartup={st}");
        let name: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let mut bmp = std::ptr::null_mut();
        let st = GdipCreateBitmapFromFile(name.as_ptr(), &mut bmp);
        assert!(st == 0 && !bmp.is_null(), "读图失败 {st}");
        let mut w = 0u32;
        let mut h = 0u32;
        GdipGetImageWidth(bmp, &mut w);
        GdipGetImageHeight(bmp, &mut h);
        let mut data = Bits { w: 0, h: 0, stride: 0, fmt: 0, scan: std::ptr::null_mut(), reserved: 0 };
        let st = GdipBitmapLockBits(bmp, &Rect { x: 0, y: 0, w: w as i32, h: h as i32 }, 1, 0x26200A, &mut data);
        assert!(st == 0 && !data.scan.is_null() && w > 0 && h > 0, "LockBits={st}");
        let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
        for y in 0..h as usize {
            let src = data.scan.add(y * data.stride as usize);
            for x in 0..w as usize {
                let s = src.add(x * 4);
                let d = (y * w as usize + x) * 4;
                // GDI+ 32bpp ARGB 内存序是 B,G,R,A。wgpu 要 RGBA。
                rgba[d] = *s.add(2);
                rgba[d + 1] = *s.add(1);
                rgba[d + 2] = *s;
                rgba[d + 3] = 255;
            }
        }
        GdipBitmapUnlockBits(bmp, &mut data);
        GdipDisposeImage(bmp);
        GdiplusShutdown(token);
        Image { rgba, w, h }
    }
}

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind: wgpu::BindGroup,
}

const SHADER: &str = r#"
struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}
@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    var u = array<vec2<f32>, 3>(vec2(0.0, 1.0), vec2(2.0, 1.0), vec2(0.0, -1.0));
    var o: Out;
    o.pos = vec4<f32>(p[i], 0.0, 1.0);
    o.uv = u[i];
    return o;
}
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@fragment
fn fs(i: Out) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, i.uv);
}
"#;

fn open_surface(window: &Window, width: u32, height: u32, image: &Image) -> Gpu {
    // 与 osu_replay_render::surface::SurfaceRenderer::new 相同的实例、
    // 适配器选择和交换链格式。
    let mut descriptor = wgpu::InstanceDescriptor::default();
    descriptor.backends = wgpu::Backends::from_env().unwrap_or(wgpu::Backends::all());
    let instance = wgpu::Instance::new(&descriptor);
    let raw_window = window.window_handle().unwrap().as_raw();
    let raw_display = window.display_handle().unwrap().as_raw();
    let surface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: raw_display,
            raw_window_handle: raw_window,
        })
    }
    .expect("create surface");
    let surface = surface;
    // 'static:窗口活得比 Gpu 久,探测进程退出时一起拆。
    let surface: wgpu::Surface<'static> = unsafe { std::mem::transmute(surface) };
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    }))
    .expect("没有 GPU 适配器");
    log::info!(
        "wgpu surface: backend={:?} adapter='{}'",
        adapter.get_info().backend,
        adapter.get_info().name
    );
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        .expect("request_device");
    let caps = surface.get_capabilities(&adapter);
    let format = if caps.formats.contains(&wgpu::TextureFormat::Bgra8Unorm) {
        wgpu::TextureFormat::Bgra8Unorm
    } else {
        caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(wgpu::TextureFormat::Bgra8Unorm)
    };
    let alpha = caps
        .alpha_modes
        .iter()
        .copied()
        .find(|m| *m == wgpu::CompositeAlphaMode::Opaque)
        .unwrap_or(wgpu::CompositeAlphaMode::Auto);
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: width.max(1),
        height: height.max(1),
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: alpha,
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    surface.configure(&device, &config);
    log::info!("交换链 {}x{} {:?} {:?}", config.width, config.height, format, alpha);

    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("bg"),
        size: wgpu::Extent3d { width: image.w, height: image.h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &image.rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(image.w * 4),
            rows_per_image: Some(image.h),
        },
        wgpu::Extent3d { width: image.w, height: image.h, depth_or_array_layers: 1 },
    );
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    let samp = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("bg"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("bg"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&samp) },
        ],
    });
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("bg"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("bg"),
        bind_group_layouts: &[&layout],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("bg"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs"), buffers: &[], compilation_options: Default::default() },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });
    Gpu { device, queue, surface, config, pipeline, bind }
}

impl Gpu {
    fn draw(&self) {
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(e) => {
                log::warn!("get_current_texture: {e:?}");
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("bg"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(std::iter::once(enc.finish()));
        frame.present();
    }
}

struct App {
    dir: PathBuf,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    t0: Option<Instant>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // wall.rs create_window 的窗口属性。
        let attrs = WindowAttributes::default()
            .with_title("aria wallpaper")
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_inner_size(PhysicalSize::new(2560, 1440));
        let window = Arc::new(event_loop.create_window(attrs).expect("创建窗口"));
        let hwnd = aria::win::window_hwnd(window.as_ref()).expect("hwnd");
        let host = aria::win::attach(hwnd, None);
        if host.width == 0 || host.height == 0 {
            log::error!("attach 失败");
            event_loop.exit();
            return;
        }
        // set_visible 会清掉 WS_CHILD / WS_EX_LAYERED,必须再封一次。
        window.set_visible(true);
        let host = {
            let again = aria::win::reattach(hwnd, None);
            if again.width > 0 { again } else { host }
        };
        aria::win::show_no_activate(hwnd);
        log::info!("窗口就绪,已附加桌面: {}×{}", host.width, host.height);
        log_diag(hwnd);
        let image = load_bg(&self.dir);
        let gpu = open_surface(window.as_ref(), host.width, host.height, &image);
        self.t0 = Some(Instant::now());
        self.gpu = Some(gpu);
        self.window = Some(window);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        if let WindowEvent::RedrawRequested = event {
            if let Some(gpu) = &self.gpu {
                gpu.draw();
            }
            if self.t0.map(|t| t.elapsed() >= HOLD).unwrap_or(false) {
                log::info!("3 秒到,退出");
                event_loop.exit();
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

fn main() {
    // 与壁纸进程同一入口:物理像素 + DX12。
    aria::win::enable_per_monitor_dpi();
    unsafe { std::env::set_var("WGPU_BACKEND", "dx12"); }
    let dir = exe_dir();
    init_log(&dir);
    log::info!("== sp_probe: 调用 aria::win + 播放器同款 wgpu 交换链 ==");
    log_os();
    let event_loop = EventLoop::new().expect("event loop");
    let mut app = App { dir, window: None, gpu: None, t0: None };
    event_loop.run_app(&mut app).expect("run");
}
