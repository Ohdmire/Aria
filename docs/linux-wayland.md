# Linux / Wayland 支持

只支持 **Wayland** 会话(Cargo 里 winit 已关掉 x11 后端),桌面壁纸走
**KDE Plasma 壁纸插件**;X11 与其它桌面环境不在支持范围内。

## 为什么是 Plasma 壁纸插件

KWin 把 `wlr-layer-shell` 的 background/bottom 面放在 plasmashell 桌面
窗口**之上** —— 直接挂 layer-shell 会盖住桌面图标(实测 KWin 6.8)。
Plasma 的壁纸本身是桌面 containment 里的一个 QML item,图标由同一个
窗口画在它上面,所以图标天然可见。做法与 wallpaper-engine-kde-plugin /
Smart Video Wallpaper Reborn 一致:安装一个 `Plasma/Wallpaper` KPackage。

## 画面链路

```
aria --wallpaper (wgpu 离屏渲染, 音频/时钟/控制在它手里)
        │  每帧 GPU 拷进 dmabuf(三缓冲),序号写进 POSIX 共享内存
        ▼
plasmashell ── 壁纸 QML item
        │  contents/ui/aria/libariaplugin.so (QQuickItem, 4ms 轮询帧号)
        ▼
桌面背景(桌面图标、任务栏、窗口都在它上面)
```

- 共享内存:`/dev/shm/aria-wallpaper-<uid>.shm`(不删除,~45MB tmpfs),
  只放头部元数据 + 序号;像素在 dmabuf 里。
- dmabuf fd 通过 `$XDG_RUNTIME_DIR/aria-wallpaper-dmabuf.sock` 交给插件。
- 三缓冲 + 抄完核对序号:写者连续推进 3 帧才会绕回读者正在读的那块,
  实际不可能撕裂。
- 不经过合成器、不需要屏幕捕获权限;渲染进程退出后桌面保留最后一帧。

## 构建与安装

应用启动时会自动把 `kde/wallpaper` 装到
`~/.local/share/plasma/wallpapers/com.ohdmire.aria/`,并把当前桌面壁纸
切换成 Aria(退出时还原)。原生控件必须**先构建好**:

```bash
# Arch: qt6-base qt6-declarative;Debian/Ubuntu: qt6-base-dev qt6-declarative-dev
scripts/build-kde-plugin.sh --install   # 构建 libariaplugin.so 并安装插件包
cargo build --release
```

打包(`pnpm tauri build`)会把 `kde/wallpaper/**` 作为 resources 带上,
但 `libariaplugin.so` 需要在本机构建后才会被包含。

## 屏与帧率

- 显示器枚举走 `wl_output` + `xdg-output`(逻辑坐标、缩放、刷新率)。
  设置里的“屏幕”只在 Wayland 侧用于选目标输出;Plasma 每个屏幕各有一个
  壁纸 item,都会显示同一份画面。
- 帧率设置(`0 = 跟随屏幕刷新率`)原样生效:实测 30/60/120/240 分别发布
  30.0/59.8/118.8/206.7 帧/秒,`0` 在 165Hz 屏上发布 ~164 帧/秒。
- **KDE 侧没有把壁纸限制在 60fps**:KWin 合成器跑在 165Hz
  (`supportInformation` 的 Refresh Rate / `kscreen-doctor` 均显示
  2560x1440@165,VRR=Never,`kwinrc` 里没有 MaxFPS/LatencyPolicy 覆盖),
  普通 Qt Quick 窗口实测 165fps。
- 高刷屏不必降场景分辨率:默认的 dmabuf 零拷贝路径下,1440p 壁纸实测
  绘制 122–142fps(165Hz 设置)、plasmashell 只占 ~35–39% CPU。
## 零拷贝(dmabuf)

Windows 上壁纸进程自己拥有 WorkerW 子窗口,`present` 直接进 DWM;KDE
下壁纸只能是 plasmashell 里的 QML item,画面必须跨进程。默认走
**dmabuf**(Linux 版的 D3D11 shared texture):

1. 渲染进程在 wgpu 的 Vulkan 设备上分配 3 个可导出的线性 `VkImage`
   (`VK_EXT_external_memory_dma_buf` + `VK_KHR_external_memory_fd`);
2. 每帧用 wgpu 自己的 encoder 把场景 target 拷进 dmabuf 图像(纯 GPU),
   等这一帧写完再递增共享内存里的序号;
3. fd 通过 `$XDG_RUNTIME_DIR/aria-wallpaper-dmabuf.sock` 一次性交给插件
   (`SCM_RIGHTS`),之后每帧只传序号;
4. 插件把 fd 用 EGL 导入成 GL 纹理直接采样,没有 CPU 像素拷贝、也没有
   纹理上传。

NVIDIA 的 EGL 只允许把 dmabuf 绑到 `GL_TEXTURE_EXTERNAL_OES`(绑
`GL_TEXTURE_2D` 会 `GL_INVALID_OPERATION`),而 Qt 场景图只认
`sampler2D`;所以插件多一步 **GPU blit**(external → 2D FBO,1440p 约
1–2ms)。Mesa 等能直接绑 2D 的驱动会自动跳过这一步。

实测(2560×1440,165Hz,桌面可见):

| 路径 | 发布 | 壁纸实际绘制 | plasmashell CPU |
| --- | --- | --- | --- |
| 共享内存拷贝 fps=60 | 60/s | 60/s | ~60% |
| 共享内存拷贝 fps=0 | 164/s | ~80/s | ~138% |
| dmabuf fps=60 | 60/s | 60/s | ~17% |
| dmabuf fps=120 | 119/s | 107/s | ~31% |
| dmabuf fps=0 | 164/s | 122–142/s | ~35–39% |

驱动不支持/排查问题时可 `ARIA_DMABUF=0 aria` 退回共享内存拷贝路径。
壁纸包里的 `libariaplugin.so` 需要和主程序一起更新(插件通过 socket
握手版本号判断兼容性)。

- KWin 在桌面窗口被其它窗口**完全遮挡**时不再合成它(省电),壁纸
  绘制会随之中止、桌面重新露出后立即恢复 —— 这不是帧率限制。

## 与 Windows 的差异

| 能力 | Windows | Linux/Wayland |
| --- | --- | --- |
| 壁纸层 | WorkerW 子窗口直渲 | Plasma 壁纸插件(dmabuf 零拷贝交帧) |
| 桌面图标 | 正常 | 正常(壁纸在 containment 内) |
| 全屏遮挡自动暂停 | `desktop_covered` | 无(KWin 不提供等价查询),仅靠 Plasma 自身停止重绘 |
| 全局快捷键 | `RegisterHotKey` | 不支持(Wayland 无全局快捷键协议,KDE 未实现 GlobalShortcuts portal) |
| 开机自启 | 注册表 Run | `~/.config/autostart/com.ohdmire.aria.desktop` |
| 多屏 | 可跨屏铺满 | 每屏同一个画面(每屏独立 surface 未做) |

## 数据与曲库位置

- 设置/播放列表/路径缓存:`~/.local/share/com.ohdmire.aria/`
- 日志(开启“日志记录”后):`~/.local/share/com.ohdmire.aria/log/aria.log`
- osu!lazer:`~/.local/share/osu`(Flatpak:`~/.var/app/sh.ppy.osu/data/osu`)
- osu!stable(Wine):自动探测 `$WINEPREFIX` / `~/.wine` /
  `~/.local/share/osu-wine/osu!` / `~/.local/share/wineprefixes/*` 下的
  `drive_c/.../osu!`;也可在设置里手动指定目录。

## 退出与还原

切换壁纸前会把原来的壁纸插件列表落盘到
`~/.local/share/com.ohdmire.aria/kde-wallpaper.json`,还原时:

- 正常退出(托盘“退出”/窗口关闭选“退出”)→ 还原后清掉备份;
- `SIGTERM` / `SIGINT`(注销、`pkill aria`)→ 先还原再退出;
- 如果当前壁纸已经不是 Aria(用户中途自己换了)→ 不覆盖用户的选择;
- 备份里只有 Aria 自己/缺失时 → 退回 `org.kde.image`,不会“还原”成
  自己而停在最后一帧;
- `kill -9` 拦不住:桌面会停在最后一帧,**下次启动会自动按备份还原再
  接管**;不想启动应用时用救急命令:

```bash
aria --restore-wallpaper
```

## 已知限制

- 只支持 KDE Plasma 6;GNOME/sway 等没有 Plasma containment,会退回
  调试窗口播放并提示。
- 首次启用需要 Plasma 加载插件;应用已在运行时升级 `libariaplugin.so`
  不会热生效——重启 plasmashell 或重新切换壁纸类型即可。
- 全局快捷键不可用;壁纸插件自身不持有时钟,渲染进程退出后画面停在
  最后一帧(见上节的还原机制)。
