//! Linux dmabuf 零拷贝输出。
//!
//! Windows 上壁纸进程自己拥有 WorkerW 子窗口,`present` 直接进 DWM;KDE
//! Wayland 下壁纸只能是 plasmashell 桌面 containment 里的 QML item,画面
//! 必须跨进程交给 plasmashell。这条链路之前是"GPU 回读 → CPU 拷贝 →
//! QML 纹理上传",1440p 下 plasmashell 每帧要烧 ~12ms。
//!
//! 这里把渲染目标导出成 **dmabuf**(Linux 版的 D3D11 shared texture):
//!
//! 1. 在 wgpu 的 Vulkan 设备上建一个可导出内存的 `VkImage`
//!    (`VK_EXT_external_memory_dma_buf` + `VK_KHR_external_memory_fd`);
//! 2. 用 wgpu 自己的 encoder 把场景 target 拷进这个图像
//!    (`create_texture_from_hal` 把外部图像包成 wgpu 纹理,走 wgpu 的
//!    队列/屏障,不和 wgpu 抢 `vkQueueSubmit`);
//! 3. `vkGetMemoryFdKHR` 拿到 fd,通过 unix socket 一次性交给 plasmashell
//!    的壁纸控件(SCM_RIGHTS);之后每帧只更新共享内存里的序号。
//!
//! 消费端(插件)把 fd 用 EGL 导入成 GL 纹理直接采样,不再有 CPU 像素
//! 拷贝和纹理上传。

use std::ffi::{c_void, CString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;

use ash::vk;
use osu_replay_render::render::Renderer;

/// DRM_FORMAT_ARGB8888(`AR24`):小端内存里就是 B,G,R,A,对应
/// 渲染目标 `Bgra8Unorm`。
pub const FOURCC_ARGB8888: u32 = 0x3432_5241;
/// 与 QML 控件约定的握手版本。
const PROTO_VERSION: u32 = 1;
/// 三缓冲:写者要连续推进 3 帧才会绕回读者正在采样的那块。
const SLOTS: usize = 3;

/// 一个导出槽位。image/memory 的所有权在 `DropCallback` 里,纹理 drop 时
/// 才 destroy/free,所以这里只留 fd / 行距 / 拷贝目标。
struct Slot {
    fd: OwnedFd,
    stride: u64,
    /// 包成 wgpu 纹理(拷贝目标)。
    texture: wgpu::Texture,
}

pub struct Exporter {
    width: u32,
    height: u32,
    slots: Vec<Slot>,
    next: usize,
    socket: Option<PathBuf>,
}

impl Exporter {
    /// 为给定渲染器创建 N 个 dmabuf 槽位,并在
    /// `$XDG_RUNTIME_DIR/aria-wallpaper.sock` 上等消费端来取 fd。
    pub fn new(renderer: &Renderer) -> Result<Exporter, String> {
        let (width, height) = (renderer.width, renderer.height);
        let device = renderer.device();
        // as_hal 的返回值借用期不能带出回调,整段创建都在回调里做完。
        let slots = unsafe {
            device.as_hal::<wgpu::hal::api::Vulkan, _, Result<Vec<Slot>, String>>(|d| {
                let hal_dev = d.ok_or("dmabuf 需要 wgpu 的 Vulkan 后端(WGPU_BACKEND=vulkan)")?;
                let raw_device = hal_dev.raw_device();
                let vk_device = raw_device.handle();
                let get_memory_fd = load_get_memory_fd(vk_device)?;
                let mut slots = Vec::with_capacity(SLOTS);
                for _ in 0..SLOTS {
                    slots.push(create_slot(
                        device,
                        hal_dev,
                        raw_device,
                        vk_device,
                        get_memory_fd,
                        width,
                        height,
                    )?);
                }
                Ok(slots)
            })?
        };
        log::info!(
            "[kde] dmabuf 零拷贝就绪:{}x{} × {} 槽(线性 tiling)",
            width,
            height,
            SLOTS
        );

        let mut exporter = Exporter {
            width,
            height,
            slots,
            next: 0,
            socket: None,
        };
        if let Err(e) = exporter.serve() {
            log::warn!("[kde] dmabuf socket 启动失败,退回共享内存拷贝: {e}");
            return Err(e);
        }
        Ok(exporter)
    }

    /// 把渲染目标拷进下一个 dmabuf 槽位,返回槽位下标。
    /// 用 wgpu 的 encoder 提交,提交后 `poll(Wait)` 确保 GPU 写完再谈发布。
    pub fn copy_frame(&mut self, renderer: &Renderer) -> Result<usize, String> {
        let idx = self.next % SLOTS;
        self.next = (self.next + 1) % SLOTS;
        let src = renderer.target_texture().as_image_copy();
        let dst = self.slots[idx].texture.as_image_copy();
        let extent = wgpu::Extent3d {
            width: self.width,
            height: self.height,
            depth_or_array_layers: 1,
        };
        let device = renderer.device();
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("aria dmabuf copy"),
        });
        enc.copy_texture_to_texture(src, dst, extent);
        renderer.queue().submit(Some(enc.finish()));
        // 等这一帧真正写完,消费端看到序号时图像一定是完整的。
        let _ = device.poll(wgpu::PollType::Wait);
        Ok(idx)
    }

    pub fn slot_stride(&self, idx: usize) -> u32 {
        self.slots[idx].stride as u32
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 在 unix socket 上把槽位 fd 交给消费端(每次连接发一遍)。
    fn serve(&mut self) -> Result<(), String> {
        let path = socket_path().ok_or("无法定位 XDG_RUNTIME_DIR")?;
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)
            .map_err(|e| format!("绑定 {} 失败:{e}", path.display()))?;
        let mut payload = Vec::with_capacity(40);
        payload.extend_from_slice(&0x4149_5241u32.to_le_bytes()); // "ARIA"
        payload.extend_from_slice(&PROTO_VERSION.to_le_bytes());
        payload.extend_from_slice(&self.width.to_le_bytes());
        payload.extend_from_slice(&self.height.to_le_bytes());
        payload.extend_from_slice(&(SLOTS as u32).to_le_bytes());
        payload.extend_from_slice(&FOURCC_ARGB8888.to_le_bytes());
        payload.extend_from_slice(&(self.slots[0].stride as u32).to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes()); // plane offset
        payload.extend_from_slice(&0u64.to_le_bytes()); // modifier(线性)
        payload.extend_from_slice(&0u32.to_le_bytes()); // reserved
        let fds: Vec<RawFd> = self.slots.iter().map(|s| s.fd.as_raw_fd()).collect();
        let fds = Arc::new(fds);
        let payload = Arc::new(payload);
        std::thread::Builder::new()
            .name("aria-dmabuf".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    let Ok(conn) = conn else { continue };
                    let mut dup: Vec<RawFd> = Vec::with_capacity(fds.len());
                    for fd in fds.iter() {
                        let d = unsafe { libc::dup(*fd) };
                        if d >= 0 {
                            dup.push(d);
                        }
                    }
                    if let Err(e) = send_fds(conn.as_raw_fd(), &payload, &dup) {
                        log::warn!("[kde] 发送 dmabuf fd 失败: {e}");
                    }
                    for fd in dup {
                        unsafe { libc::close(fd) };
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        self.socket = Some(path);
        Ok(())
    }
}

impl Drop for Exporter {
    fn drop(&mut self) {
        // 槽位的 wgpu 纹理先 drop(会触发 DropCallback 销毁 image/释放内存)
        self.slots.clear();
        if let Some(p) = &self.socket {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn socket_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| Some(PathBuf::from("/tmp")))?;
    Some(dir.join("aria-wallpaper-dmabuf.sock"))
}

/// `vkGetDeviceProcAddr(vk_device, "vkGetMemoryFdKHR")`。
/// 返回的函数指针在设备存活期间一直有效。
unsafe fn load_get_memory_fd(
    vk_device: vk::Device,
) -> Result<unsafe extern "system" fn(vk::Device, *const vk::MemoryGetFdInfoKHR<'_>, *mut RawFd) -> vk::Result, String>
{
    let name = CString::new("vkGetDeviceProcAddr").unwrap();
    // ash 用 RTLD_LOCAL 载入 libvulkan,它的符号不在 RTLD_DEFAULT 里;
    // 自己 dlopen 同一个 loader 再取符号(同一个实例,句柄通用)。句柄
    // 故意不 dlclose —— loader 活得比本进程的 Vulkan 设备久。
    let lib = CString::new("libvulkan.so.1").unwrap();
    let handle = unsafe { libc::dlopen(lib.as_ptr(), libc::RTLD_NOW) };
    let sym = if handle.is_null() {
        unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) }
    } else {
        unsafe { libc::dlsym(handle, name.as_ptr()) }
    };
    if sym.is_null() {
        return Err("libvulkan 未加载/找不到 vkGetDeviceProcAddr".into());
    }
    let get_proc: unsafe extern "system" fn(vk::Device, *const std::ffi::c_char) -> *const c_void =
        unsafe { std::mem::transmute(sym) };
    let fn_name = CString::new("vkGetMemoryFdKHR").unwrap();
    let p = unsafe { get_proc(vk_device, fn_name.as_ptr()) };
    if p.is_null() {
        return Err("VK_KHR_external_memory_fd 不可用".into());
    }
    Ok(unsafe { std::mem::transmute(p) })
}

fn create_slot(
    device: &wgpu::Device,
    hal_dev: &wgpu::hal::vulkan::Device,
    raw_device: &ash::Device,
    vk_device: vk::Device,
    get_memory_fd: unsafe extern "system" fn(
        vk::Device,
        *const vk::MemoryGetFdInfoKHR<'_>,
        *mut RawFd,
    ) -> vk::Result,
    width: u32,
    height: u32,
) -> Result<Slot, String> {
    let _ = vk_device;
    let mut ext_image = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::B8G8R8A8_UNORM)
        .extent(vk::Extent3D { width, height, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        // 线性:EGL 导入最稳(NVIDIA 对带 modifier 的 dmabuf 支持有限)
        .tiling(vk::ImageTiling::LINEAR)
        .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut ext_image);
    let image = unsafe { raw_device.create_image(&image_info, None) }
        .map_err(|e| format!("vkCreateImage 失败:{e}"))?;

    let req = unsafe { raw_device.get_image_memory_requirements(image) };
    let type_index = (0..32).find(|i| req.memory_type_bits & (1 << i) != 0).unwrap_or(0);
    let mut export_info = vk::ExportMemoryAllocateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(type_index)
        .push_next(&mut export_info)
        .push_next(&mut dedicated);
    let memory = unsafe { raw_device.allocate_memory(&alloc_info, None) }
        .map_err(|e| format!("vkAllocateMemory 失败:{e}"))?;
    unsafe { raw_device.bind_image_memory(image, memory, 0) }
        .map_err(|e| format!("vkBindImageMemory 失败:{e}"))?;

    let layout = unsafe {
        raw_device.get_image_subresource_layout(
            image,
            vk::ImageSubresource::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .mip_level(0)
                .array_layer(0),
        )
    };
    let fd_raw = unsafe {
        let mut fd: RawFd = -1;
        let info = vk::MemoryGetFdInfoKHR::default()
            .memory(memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        (get_memory_fd)(vk_device, &info, &mut fd).result().map_err(|e| format!("vkGetMemoryFdKHR 失败:{e}"))?;
        fd
    };
    if fd_raw < 0 {
        return Err("vkGetMemoryFdKHR 返回无效 fd".into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd_raw) };

    // 包成 wgpu 纹理:走 wgpu 自己的队列做拷贝,不和 wgpu 抢 vkQueueSubmit。
    let hal_desc = wgpu::hal::TextureDescriptor {
        label: Some("aria-dmabuf"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu_types::TextureUses::COPY_DST,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: vec![],
    };
    // DropCallback 要求 Send + Sync;用地址避免裸指针的 auto-trait 问题。
    let dev_addr = raw_device as *const ash::Device as usize;
    let drop_cb: wgpu::hal::DropCallback = {
        Box::new(move || unsafe {
            let dev = &*(dev_addr as *const ash::Device);
            dev.destroy_image(image, None);
            dev.free_memory(memory, None);
        })
    };
    let hal_tex = unsafe {
        wgpu::hal::vulkan::Device::texture_from_raw(image, &hal_desc, Some(drop_cb))
    };
    let desc = wgpu::TextureDescriptor {
        label: Some("aria dmabuf"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    };
    let _ = hal_dev;
    let texture = unsafe {
        device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(hal_tex, &desc)
    };
    Ok(Slot { fd, stride: layout.row_pitch, texture })
}

fn send_fds(sock: RawFd, payload: &[u8], fds: &[RawFd]) -> std::io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: payload.as_ptr() as *mut c_void,
        iov_len: payload.len(),
    };
    let fd_bytes = std::mem::size_of_val(fds);
    let mut cmsg_buf = vec![0u8; unsafe { libc::CMSG_SPACE(fd_bytes as u32) } as usize];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut c_void;
    msg.msg_controllen = cmsg_buf.len();
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(std::io::Error::other("cmsg 初始化失败"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as u32) as usize;
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(cmsg), fd_bytes);
    }
    let n = unsafe { libc::sendmsg(sock, &msg, 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}