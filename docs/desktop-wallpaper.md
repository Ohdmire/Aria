# 桌面壁纸挂接

壁纸窗口的挂接在 `src/win.rs`，播放器在 `src/wall.rs` 的 `create_window` 里创建窗口并调用它。探测程序 `examples/sp_probe.rs` 走同一条创建路径，用来在别的机器上核对桌面结构。本文记录 26200 上实测过的行为，不沿用「先 `SetParent` 再补 `WS_EX_LAYERED`」的旧假设。

## 两条桌面

用 `Progman` 的扩展样式区分。`WS_EX_NOREDIRECTIONBITMAP` 是 `0x200000`。

**Raised desktop**（本机 Windows 10 Pro 25H2，build 26200.9457）。`Progman` 扩展样式含 `0x200000`，例如 `0x200080`。直接子窗口从上到下是：

1. `SHELLDLL_DefView`：图标层，自身带 `WS_EX_LAYERED`（`0x80000`），进程是 `explorer.exe`。
2. 壁纸窗口：我们的子窗口，夹在图标和系统壁纸之间。
3. `WorkerW`：shell 用来画静态壁纸的子窗口，应压在最底。

**经典桌面**（Windows 10 Home China 22H2，build 19045 上见过）。`Progman` 扩展样式只有 `0x80`，一开始可以没有子窗口，`DefView` 句柄是 0。需要发 `0x052C`（`wParam=0xD`，`lParam=0x1`）让 shell 整理层次，再找「直接子级是 `SHELLDLL_DefView`」的那个顶层窗口，取它 z 序后面、进程属于 explorer 的第一个 `WorkerW`。找不到就不挂，不能退回挂到空的 `Progman` 上。`0x052C` 五秒内只发一次；显示之后补样式不能再发，否则 shell 会重排桌面。

winit 窗口的类名是 `Window Class`。它出现在 `Progman` 子窗口列表里，只说明那是我们自己的窗口，不是 `WorkerW`。

## 26200 上 SetParent 不会改父窗口

在本机对已经创建好的顶层窗口调用 `SetParent(Progman)`：

- 返回值是 `NULL`。窗口原来没有父窗口，`windows-rs` 把这次调用包装成 `Err(操作成功完成)`，`GetLastError` 仍是 0。
- 调用之后 `GetParent` 和 `GetAncestor(GA_PARENT)` 都还是桌面（`#32769`，例如 `0x1000c`），不是 `Progman`。
- 对 `Progman` 下那个 `WorkerW` 再调一次，结果相同。
- 调用线程和窗口线程相同，`Progman` 在 explorer 的线程上。这是正常的跨线程挂接，不是线程用错。
- 先写上 `WS_CHILD` 和 `WS_EX_LAYERED` 再 `SetParent`，父窗口同样不变。

所以不能用 `SetParent` 的返回值判断挂上没有，必须看挂完之后的父窗口句柄。`GWLP_HWNDPARENT` 在同一次试验里也没有把父窗口改成 `Progman`。

能挂上的做法是创建时就把 `hwndParent` 设为 `Progman`。播放器通过 winit 的 `WindowAttributes::with_parent_window` 做这件事（`win::progman_raw_parent`）。窗口生下来就是 `Progman` 的子窗口，`attach` 发现父窗口已经是 `Progman` 就不再调用 `SetParent`。

## 分层样式写不上去

子窗口建好之后，`SetWindowLongPtr(GWL_EXSTYLE)` 加上 `WS_EX_LAYERED`（`0x80000`），立刻再读仍然是原来的值。`SetLayeredWindowAttributes` 返回 `0x80070057`（参数错误）。winit 随后按自己的 `WindowFlags` 刷新样式时，也会把这个位清掉。

因此不要依赖事后补 `WS_EX_LAYERED`。创建时用 `WindowAttributesExtWindows::with_no_redirection_bitmap(true)`，让 winit 在 `CreateWindowEx` 里带上 `WS_EX_NOREDIRECTIONBITMAP`（`0x200000`）。这个位和 `Progman` 自己的扩展样式同类，刷新之后还在。本机探针里最终扩展样式是 `0x200010`（`WS_EX_NOREDIRECTIONBITMAP | WS_EX_ACCEPTFILES`）。

`WS_EX_NOREDIRECTIONBITMAP` 让窗口没有 GDI 重定向位图，DXGI 交换链可以直接被桌面合成。播放器的渲染子进程使用 DX12（未设置 `WGPU_BACKEND` 时，`wall::main` 会设成 `dx12`）。交换链格式与 `osu-replay-render` 的表面一致：`Bgra8Unorm`、`CompositeAlphaMode::Opaque`、`PresentMode::Fifo`。

## 播放器创建顺序

`wall::main` 在建任何窗口之前调用 `win::enable_per_monitor_dpi`（`DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`），客户区按物理像素计算。然后：

1. `with_no_redirection_bitmap(true)`，并且 `with_parent_window(Progman)`。
2. `win::attach`：raised 桌面上只摆位置和 z 序，把窗口放在 `DefView` 之下、`WorkerW` 之上，并把 `WorkerW` 压到最底。
3. `set_visible(true)`。winit 会按自己的 flags 重写样式；`WS_EX_NOREDIRECTIONBITMAP` 因为在 flags 里，会留下来。
4. `win::reattach` 再摆一次 z 序。这里不再发 `0x052C`。
5. `ShowWindow(SW_SHOWNOACTIVATE)`，然后 `set_cursor_hittest(false)`，让图标继续接收鼠标。

GPU 表面在载入谱面时才创建，用的是这个已经挂好的窗口。

## 退出不会拆掉壳层

退出时丢掉壁纸窗口，也就是销毁我们自己的 `HWND`。不发送 `0x052C`，也不改 `DefView` 的父窗口。本机在多次探针退出之后，`Progman` 的子窗口仍是 `DefView` 和 `WorkerW`。

下一次若再新建顶层窗口然后 `SetParent`，会再次挂不进去。所以每次都要在创建时指定父窗口，而不是退出之后再补挂。

## 探测程序

```powershell
cargo build --release --example sp_probe
```

产物是 `target/release/examples/sp_probe.exe`。把 exe 拷到要测的机器上运行。背景优先用 exe 旁边的 `BG.jpg`，没有则写出内嵌的那张。日志写到 exe 同目录的 `wall_probe.log`（UTF-8 BOM）。本仓库默认 release 目录下的日志是：

`target/release/examples/wall_probe.log`

日志包含系统产品名、版本、build，以及 `Progman` 每个直接子窗口的类名、`HWND`、是否可见、矩形、`style`、`ex`、进程路径。用这份日志判断挂上没有：父窗口应是 `Progman`，扩展样式应含 `0x200000`，子窗口顺序应是 `DefView`、壁纸窗口、`WorkerW`。
