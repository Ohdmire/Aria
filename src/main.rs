//! Aria(ATRI rhythm interactive animation)—— osu! storyboard 桌面动态壁纸播放器。
//!
//! 单个可执行文件、两种模式:
//! - 默认:Tauri 设置窗口 + 系统托盘,spawn 自身 `--wallpaper` 渲染子进程;
//! - `--wallpaper`:winit + wgpu 渲染进程,把 storyboard 画到桌面
//!   WorkerW 层(Wallpaper Engine 同款),stdin/stdout JSON-lines 受控。
//! `--hidden` 启动则静默到托盘(开机自启用)。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod ctl;
mod ipc;
mod lazer;
mod loader;
mod logging;
mod pathcache;
mod playlist;
mod settings;
mod soundtouch;
mod stable;
mod wall;
mod win;

use ipc::{Command, Event};
use playlist::{PlayMode, Track};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WindowEvent};

fn main() {
    // 壁纸渲染子进程模式:不初始化 Tauri/WebView2(自己的日志在 wall::main 里)
    if std::env::args().any(|a| a == "--wallpaper") {
        std::process::exit(wall::main());
    }

    // 父进程 stderr 日志 + 可选文件记录(设置界面"日志记录"开关;
    // release 无控制台时 stderr 写往空处,无害)。必须在 --wallpaper
    // 分发之后,子进程在 wall::main 里自行安装同一实现。
    let _ = logging::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(setup)
        .on_window_event(|window, event| {
            // 关闭按钮:按保存的行为执行(托盘/退出);未保存则拦下并询问
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let app = window.app_handle();
                    let action = app
                        .state::<ctl::WallState>()
                        .settings
                        .lock()
                        .unwrap()
                        .close_action
                        .clone();
                    match action.as_deref() {
                        Some("exit") => quit_app_inner(app),
                        // 关到托盘 = 轻量模式:销毁窗口释放 webview 内存,
                        // 播放子进程继续;托盘单击/打开设置时按原配置重建。
                        Some("tray") => {
                            let _ = window.destroy();
                            ctl::release_caches(app);
                            trim_working_set();
                        }
                        _ => {
                            let _ = app.emit("wall://close-ask", ());
                        }
                    }
                }
            }
        })
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app);
        }))
        .invoke_handler(tauri::generate_handler![
            get_settings,
            pause,
            resume,
            toggle_play,
            seek,
            set_speed,
            set_fail,
            set_loop,
            unload,
            restart_wallpaper,
            get_status,
            set_autostart,
            set_log_enabled,
            export_log,
            close_window,
            set_close_action,
            set_dblclick_add,
            set_quick_add,
            set_quick_mods,
            quit_app,
            library_refresh,
            source_set,
            queue,
            current_track,
            playlist_add,
            playlist_reorder,
            playlist_add_batch,
            playlist_remove,
            playlist_clear,
            queue_set_entry,
            queue_set_mods,
            queue_set_diff,
            library_play,
            playlist_set,
            skip_next,
            skip_prev,
            set_volume,
            set_master_volume,
            set_audio_offset,
            set_hitsound,
            set_hits_volume,
            set_hud,
            set_pp,
            set_reduce_anim,
            set_break_lighten,
            set_cursor,
            set_cursor_size,
            set_fade_audio,
            set_gameplay_hidden,
            set_render_mode,
            set_bg_opacity,
            set_hidden,
            set_storyboard,
            set_beatmap_hitsounds,
            dir_status,
            pick_data_dir,
            set_data_dir,
            skin_list,
            set_skin,
            set_force_skin_colours,
            set_fps,
            set_cover,
            set_target_star,
            playing_track,
            set_star_min,
            open_url,
            set_ffmpeg_bins,
            ffmpeg_bin_status
        ])
        .build(tauri::generate_context!())
        .expect("构建 Tauri 应用失败")
        .run(|app, event| match event {
            // 轻量模式:最后一个窗口被销毁时 Tauri 默认会退出应用——
            // 这里拦下"窗口关闭触发的退出请求"(code = None),托盘与
            // 壁纸子进程继续存活;程序化退出(exit(0), code = Some)放行,
            // 退出前确保壁纸子进程不残留。
            tauri::RunEvent::ExitRequested { code, api, .. } => {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
            tauri::RunEvent::Exit => {
                if let Some(mut c) = app.state::<ctl::WallState>().ctl.lock().unwrap().take() {
                    let _ = c.send(&Command::Quit);
                    let _ = c.child.kill();
                    let _ = c.child.wait();
                }
            }
            _ => {}
        });
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let mut st = settings::load(app.handle());
    st.autostart = settings::autostart_enabled(); // 以注册表实际状态为准
    // 应用手动指定的数据目录(校验失败静默回自动检测)
    if let Some(dir) = &st.lazer_dir {
        let _ = lazer::set_custom_data_dir(Some(std::path::Path::new(dir)));
    }
    if let Some(dir) = &st.stable_dir {
        let _ = stable::set_custom_stable_dir(Some(std::path::Path::new(dir)));
    }
    // 上次保存的日志记录开关(子进程侧在 spawn 后由 SetLog 下发)
    if st.log_enabled {
        logging::set_enabled(true);
    }
    app.manage(ctl::WallState::new(st));

    build_tray(app)?;

    if !std::env::args().any(|a| a == "--hidden") {
        show_main(app.handle());
    }

    // 启动即恢复上次的壁纸(稍等 UI 加载完成再发,避免错过事件)
    let handle = app.handle().clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        if let Err(e) = ctl::reload_saved(&handle) {
            eprintln!("恢复壁纸失败: {e}");
        }
    });
    Ok(())
}

fn build_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "打开设置", true, None::<&str>)?;
    let prev = MenuItem::with_id(app, "prev", "上一首", true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, "toggle", "暂停 / 继续", true, None::<&str>)?;
    let next = MenuItem::with_id(app, "next", "下一首", true, None::<&str>)?;
    let sep = MenuItem::with_id(app, "sep-1", "-", true, None::<&str>)?;
    let restart = MenuItem::with_id(app, "restart", "重启壁纸", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &prev, &toggle, &next, &restart, &quit])?;

    let mut builder = TrayIconBuilder::with_id("aria")
        .tooltip("Aria — osu! storyboard 动态壁纸")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id().as_ref() {
            "open" => show_main(app),
            "prev" => {
                if let Err(e) = ctl::playlist_prev(app) {
                    eprintln!("上一首失败: {e}");
                }
            }
            "toggle" => {
                let cmd = if ctl::is_playing(app) { Command::Pause } else { Command::Resume };
                let _ = ctl::send_cmd(app, cmd);
            }
            "next" => {
                if let Err(e) = ctl::playlist_next(app) {
                    eprintln!("下一首失败: {e}");
                }
            }
            "restart" => {
                if let Err(e) = ctl::restart(app) {
                    eprintln!("重启壁纸失败: {e}");
                }
            }
            "quit" => quit_app_inner(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    let tray = builder.build(app)?;
    app.manage(tray); // 保活,否则托盘图标会被回收
    Ok(())
}

/// 把当前进程工作集压到最小:free() 之后 Windows 不会立即回收页面,
/// 不主动裁剪的话任务管理器里内存数字纹丝不动(用户观感 = "没释放")。
fn trim_working_set() {
    #[cfg(windows)]
    unsafe {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> isize;
            fn SetProcessWorkingSetSize(hProcess: isize, dwMinimumWorkingSetSize: isize, dwMaximumWorkingSetSize: isize) -> i32;
        }
        SetProcessWorkingSetSize(GetCurrentProcess(), -1, -1);
    }
}

fn show_main(app: &AppHandle) {
    match app.get_webview_window("main") {
        Some(w) => {
            let _ = w.show();
            let _ = w.unminimize();
            let _ = w.set_focus();
        }
        None => recreate_main(app),
    }
}



fn recreate_main(app: &AppHandle) {
    let _ = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App("index.html".into()))
        .title("Aria")
        .inner_size(1180.0, 760.0)
        .min_inner_size(960.0, 600.0)
        .resizable(true)
        .center()
        .visible(true)
        .build();
}

fn quit_app_inner(app: &AppHandle) {
    let _ = ctl::send_cmd(app, Command::Quit);
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        handle.exit(0);
    });
}

// ---------- Tauri commands(前端 invoke) ----------

#[tauri::command]
fn get_settings(app: AppHandle) -> settings::Settings {
    app.state::<ctl::WallState>().settings.lock().unwrap().clone()
}

#[tauri::command]
fn pause(app: AppHandle) -> Result<(), String> {
    ctl::send_cmd(&app, Command::Pause)
}

#[tauri::command]
fn resume(app: AppHandle) -> Result<(), String> {
    ctl::send_cmd(&app, Command::Resume)
}

#[tauri::command]
fn toggle_play(app: AppHandle) -> Result<(), String> {
    let cmd = if ctl::is_playing(&app) { Command::Pause } else { Command::Resume };
    ctl::send_cmd(&app, cmd)
}

#[tauri::command]
fn seek(app: AppHandle, ms: f32) -> Result<(), String> {
    ctl::send_cmd(&app, Command::Seek { ms })
}

#[tauri::command]
fn set_speed(app: AppHandle, x: f32) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.speed = x.clamp(0.05, 16.0);
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetSpeed { x })
}

#[tauri::command]
fn set_fail(app: AppHandle, on: bool) -> Result<(), String> {
    ctl::send_cmd(&app, Command::SetFail { on })
}

/// 单曲循环(歌单播完自动回绕循环,恒定开启,不是选项)。
#[tauri::command]
fn set_loop(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.loop_playback = on;
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetLoop { on })
}

#[tauri::command]
fn unload(app: AppHandle) -> Result<(), String> {
    ctl::send_cmd(&app, Command::Unload)
}

#[tauri::command]
fn restart_wallpaper(app: AppHandle) -> Result<(), String> {
    ctl::restart(&app)
}

#[tauri::command]
fn get_status(app: AppHandle) -> Option<Event> {
    app.state::<ctl::WallState>().last_event.lock().unwrap().clone()
}

/// 当前播放的播放列表条目(设置窗口销毁重建后恢复标题/封面/难度用:
/// `loaded` 事件只有 blob 路径,曲库未就绪时会退化显示成哈希)。
#[tauri::command]
fn current_track(app: AppHandle) -> Option<crate::playlist::Track> {
    app.state::<ctl::WallState>().playlist.lock().unwrap().current()
}

#[tauri::command]
fn set_autostart(app: AppHandle, on: bool) -> Result<(), String> {
    settings::set_autostart(on)?;
    let state = app.state::<ctl::WallState>();
    let mut s = state.settings.lock().unwrap();
    s.autostart = on;
    settings::save(&app, &s);
    Ok(())
}

/// 日志记录开关:本进程立即生效并持久化,壁纸子进程随命令生效
/// (未运行则忽略,下次 spawn 时由启动批次下发)。
#[tauri::command]
fn set_log_enabled(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.log_enabled = on;
        settings::save(&app, &s);
    }
    logging::set_enabled(on);
    let _ = ctl::send_cmd(&app, Command::SetLog { on });
    Ok(())
}

/// 导出日志:保存对话框选位置,把数据目录 log/aria.log 的当前内容
/// 写过去;返回所选路径(None = 用户取消)。
#[tauri::command]
async fn export_log(app: AppHandle) -> Result<Option<String>, String> {
    let content = logging::read_log()
        .ok_or_else(|| "日志文件尚未生成(先打开日志记录)".to_string())?;
    if content.trim().is_empty() {
        return Err("日志为空".into());
    }
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        use tauri_plugin_dialog::DialogExt;
        let picked = app2
            .dialog()
            .file()
            .add_filter("日志", &["log", "txt"])
            .set_file_name("aria-log.log")
            .blocking_save_file();
        let Some(f) = picked else { return Ok(None) };
        let path = f.into_path().map_err(|e| e.to_string())?;
        std::fs::write(&path, content.as_bytes()).map_err(|e| e.to_string())?;
        Ok(Some(path.to_string_lossy().into_owned()))
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------- 曲库(lazer / stable) ----------
// 曲库命令全部 async + spawn_blocking:
// 同步命令在 Tauri v2 里跑在主线程,realm/osu!.db 解析、blob 复制、封面
// base64 都在秒级,主线程一动整个窗口就冻住 —— 表现为"曲库不可用"。
// 单飞锁(ctl::library)会让后来者在锁上等待,更不能在主线程上等。

/// library_refresh 命令的返回载荷。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryPayload {
    pub source: String,
    /// 当前源的数据文件路径(展示用)。
    pub realm_path: Option<String>,
    /// 各源是否可用(前端切换开关)。
    pub lazer_available: bool,
    pub stable_available: bool,
    pub sets: Vec<lazer::LazerSet>,
    pub collections: Vec<lazer::LazerCollection>,
}

#[tauri::command]
async fn library_refresh(
    app: AppHandle,
    refresh: Option<bool>,
    source: Option<String>,
) -> Result<LibraryPayload, String> {
    let source = source.unwrap_or_else(|| {
        app.state::<ctl::WallState>().settings.lock().unwrap().source.clone()
    });
    let refresh = refresh.unwrap_or(false);
    let src = source.clone();
    let lib = tauri::async_runtime::spawn_blocking(move || ctl::library(&app, &src, refresh))
        .await
        .map_err(|e| e.to_string())??;
    let realm_path = match source.as_str() {
        "stable" => stable::stable_root().map(|p| p.join("osu!.db").display().to_string()),
        _ => lazer::realm_path().map(|p| p.display().to_string()),
    };
    Ok(LibraryPayload {
        source,
        realm_path,
        lazer_available: lazer::realm_path().is_some(),
        stable_available: stable::stable_root().is_some(),
        sets: lib.sets.clone(),
        collections: lib.collections.clone(),
    })
}

/// 切换曲库源(lazer / stable):清缓存、清封面缓存;播放列表只清理当前
/// 源已不存在的曲目,**不自动重建/填充**(列表内容由用户显式添加)。
#[tauri::command]
async fn source_set(app: AppHandle, source: String) -> Result<(), String> {
    if !matches!(source.as_str(), "lazer" | "stable") {
        return Err(format!("未知曲库源：{source}"));
    }
    if source == "stable" && stable::stable_root().is_none() {
        return Err("未找到 osu!stable 安装目录(需要 osu!.db)".into());
    }
    {
        let state = app.state::<ctl::WallState>();
        state.lib_cache.lock().unwrap().take();
        state.cover_cache.lock().unwrap().clear();
        let mut s = state.settings.lock().unwrap();
        s.source = source.clone();
        s.lazer_set = None; // 源变了,上次曲目不可恢复
        s.lazer_sha2 = None;
        settings::save(&app, &s);
    }
    // 清理新源中已不存在的曲目(跨源残留点播必失败)
    let src = source.clone();
    let app2 = app.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || {
        if let Ok(lib) = ctl::library(&app2, &src, false) {
            app2.state::<ctl::WallState>().playlist.lock().unwrap().prune_missing(&lib);
            ctl::save_playlist(&app2);
        }
    })
    .await;
    let _ = app.emit("wall://source", &source);
    Ok(())
}

/// 播放队列(恒按入列顺序;随机只影响实际播放顺序,不改列表显示)。
/// **不等曲库解析**:有缓存则精化(难度星级/最新标题),没有就用条目
/// 自带的显示快照 —— 启动/托盘态秒出列表,曲库就绪后前端再拉一次。
#[tauri::command]
async fn queue(app: AppHandle) -> Result<Vec<QueueItem>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<ctl::WallState>();
        let target = state.settings.lock().unwrap().target_star;
        let lib = ctl::library_cached(&app);
        let playlist = state.playlist.lock().unwrap();
        let current = playlist.current();
        let by_id: Option<std::collections::HashMap<&str, &lazer::LazerSet>> = lib
            .as_ref()
            .map(|lib| lib.sets.iter().map(|s| (s.id.as_str(), s)).collect());
        let mut items = Vec::with_capacity(playlist.queue().len());
        for track in playlist.queue() {
            let current_flag = current.as_ref().is_some_and(|c| c.qid == track.qid);
            let Some(by_id) = &by_id else {
                // 快照模式:曲库未解析,条目元数据直接上(旧存档无快照
                // 则为空,前端显示占位;入列/播放后永久补齐)
                items.push(QueueItem {
                    qid: track.qid,
                    set_id: track.set_id.clone(),
                    sha2: track.sha2.clone().unwrap_or_default(),
                    mods: track.mods,
                    locked: track.sha2.is_some(),
                    title: track.title.clone(),
                    artist: track.artist.clone(),
                    diff: String::new(),
                    star: 0.0,
                    length_ms: track.length_ms,
                    current: current_flag,
                });
                continue;
            };
            let Some(set) = by_id.get(track.set_id.as_str()) else { continue };
            // 显示难度:指定难度 > 目标星级解析(与播放时实际解析一致)
            let diff = track
                .sha2
                .as_ref()
                .and_then(|s| set.beatmaps.iter().find(|b| b.sha2 == *s))
                .unwrap_or_else(|| playlist::pick_diff(set, playlist::policy_of(target)));
            items.push(QueueItem {
                qid: track.qid,
                set_id: track.set_id.clone(),
                sha2: diff.sha2.clone(),
                mods: track.mods,
                locked: track.sha2.is_some(),
                title: if set.title_unicode.is_empty() { set.title.clone() } else { set.title_unicode.clone() },
                artist: if set.artist_unicode.is_empty() { set.artist.clone() } else { set.artist_unicode.clone() },
                diff: diff.name.clone(),
                star: diff.star_rating,
                length_ms: diff.length_ms,
                current: current_flag,
            });
        }
        Ok(items)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub qid: u64,
    pub set_id: String,
    pub sha2: String,
    /// osu! legacy mod 位(0 = 无;右键可改)。
    pub mods: u32,
    /// 锁定谱面(以难度入列):难度选择器对它不生效,恒播该难度。
    pub locked: bool,
    pub title: String,
    pub artist: String,
    pub diff: String,
    pub star: f64,
    pub length_ms: f64,
    pub current: bool,
}

/// 批量入列项:{setId, sha2?, mods?, lockDiff?}。`sha2` 显式指定时
/// 优先;`lockDiff = true` 时按当前目标星级策略解析一档难度并锁定
/// (导入即静态化,后续星级策略变化不影响);否则谱面集级动态解析。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchAdd {
    pub set_id: String,
    pub sha2: Option<String>,
    #[serde(default)]
    pub mods: u32,
    /// 难度策略(导入即锁定):None=最难,Some(-1)=最简单,Some(x>0)=
    /// 星级最接近 x(与 queue_set_diff / target_star 同编码)。
    #[serde(default)]
    pub target: Option<f64>,
}

/// 入列条目的显示快照(标题/艺术家按 Unicode 优先;时长取指定难度,
/// 谱面集级条目按目标星级解析 —— 与播放/队列显示同一口径)。
fn track_display_meta(set: &lazer::LazerSet, sha2: Option<&str>, target: Option<f64>) -> (String, String, f64) {
    let title = if set.title_unicode.is_empty() { set.title.clone() } else { set.title_unicode.clone() };
    let artist = if set.artist_unicode.is_empty() { set.artist.clone() } else { set.artist_unicode.clone() };
    let diff = sha2
        .and_then(|s| set.beatmaps.iter().find(|b| b.sha2 == s))
        .unwrap_or_else(|| playlist::pick_diff(set, playlist::policy_of(target)));
    (title, artist, diff.length_ms)
}

/// 曲库加入播放列表。默认**谱面集级**入列(播放时按目标星级解析
/// 难度);难度 chip 传入 `sha2` 固定难度;`lockDiff = true` 且未传
/// `sha2` 时按当前目标星级策略解析一档并锁定(与批量导入同口径)。
#[tauri::command]
async fn playlist_add(
    app: AppHandle,
    set_id: String,
    sha2: Option<String>,
    mods: u32,
    target: Option<f64>,
) -> Result<bool, String> {
    let app2 = app.clone();
    // 返回 (是否新插入, 被替换的当前播放条目): 同谱面集重复导入 =
    // 原位替换参数(位置/qid 不变),命中当前条目时原位重载立即生效
    let (added, current_hit) = tauri::async_runtime::spawn_blocking(move || {
        let state = app2.state::<ctl::WallState>();
        let source = state.settings.lock().unwrap().source.clone();
        let lib = ctl::library(&app2, &source, false)?;
        let target = state.settings.lock().unwrap().target_star;
        let Some(set) = lib.sets.iter().find(|s| s.id == set_id) else {
            return Err::<_, String>("谱面集不在曲库中".into());
        };
        // 显式 sha2(难度 chip 右键)优先;否则按策略挑最相近锁定
        let sha2 = sha2
            .or_else(|| Some(playlist::pick_diff(set, playlist::policy_of(target)).sha2.clone()));
        let (title, artist, length_ms) = track_display_meta(set, sha2.as_deref(), target);
        let mut playlist = state.playlist.lock().unwrap();
        let added = playlist.add(set_id.clone(), sha2, mods, title, artist, length_ms);
        let current_hit =
            if added { None } else { playlist.current().filter(|c| c.set_id == set_id) };
        Ok((added, current_hit))
    })
    .await
    .map_err(|e| e.to_string())??;
    ctl::save_playlist(&app);
    if let Some(track) = current_hit {
        reload_current(&app, track).await?;
    }
    Ok(added)
}

/// 批量加入播放列表(曲库过滤结果的"添加到播放列表"按钮):一次锁、
/// 一次曲库访问,全部按谱面集级入列(播放时解析难度)。返回入列数。
#[tauri::command]
async fn playlist_add_batch(app: AppHandle, items: Vec<BatchAdd>) -> Result<usize, String> {
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app2.state::<ctl::WallState>();
        let source = state.settings.lock().unwrap().source.clone();
        let lib = ctl::library(&app2, &source, false)?;
        let target = state.settings.lock().unwrap().target_star;
        let mut added = 0usize;
        {
            let mut playlist = state.playlist.lock().unwrap();
            for item in &items {
                let Some(set) = lib.sets.iter().find(|s| s.id == item.set_id) else {
                    continue;
                };
                // 显式 sha2(chip)> 按条目策略挑最相近锁定
                let sha2 = item.sha2.clone().or_else(|| {
                    Some(playlist::pick_diff(set, playlist::policy_of(item.target)).sha2.clone())
                });
                let (title, artist, length_ms) =
                    track_display_meta(set, sha2.as_deref(), target);
                playlist.add(item.set_id.clone(), sha2, item.mods, title, artist, length_ms);
                added += 1;
            }
        }
        if added > 0 {
            ctl::save_playlist(&app2);
        }
        Ok(added)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 编辑类命令的公共尾巴:被改条目含当前播放曲目时原位重载(回到当前
/// 进度),让 mods/难度立即生效。
async fn reload_current(app: &AppHandle, track: Track) -> Result<(), String> {
    let t = match &*app.state::<ctl::WallState>().last_event.lock().unwrap() {
        Some(crate::ipc::Event::Status { t_ms, .. }) => *t_ms,
        _ => 0.0,
    };
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || ctl::load_track_at(&app2, &track, t))
        .await
        .map_err(|e| e.to_string())?
}

/// 编辑播放列表条目(右键菜单):覆盖 mods 与难度锁定方式。正在播放的
/// 条目原位重载(回到当前进度),其余条目下次播放生效。
#[tauri::command]
async fn queue_set_entry(
    app: AppHandle,
    qid: u64,
    mods: u32,
    sha2: Option<String>,
) -> Result<(), String> {
    let (edited, is_current) = {
        let state = app.state::<ctl::WallState>();
        let mut playlist = state.playlist.lock().unwrap();
        let is_current =
            playlist.current().is_some_and(|c| c.qid == qid);
        (playlist.set_entry(qid, mods, sha2), is_current)
    };
    let Some(track) = edited else { return Err("条目不存在".into()) };
    ctl::save_playlist(&app);
    if is_current {
        // 原位重载:回到当前播放位置(mods/难度立即生效)
        reload_current(&app, track).await?;
    }
    Ok(())
}

/// 批量设置条目 mods(多选右键「修改 mods」):难度锁定不动(跨谱面集
/// 难度不可统一)。含当前播放条目时原位重载。
#[tauri::command]
async fn queue_set_mods(app: AppHandle, qids: Vec<u64>, mods: u32) -> Result<(), String> {
    if qids.is_empty() {
        return Ok(());
    }
    let current_hit = {
        let state = app.state::<ctl::WallState>();
        state.playlist.lock().unwrap().set_mods_batch(&qids, mods)
    };
    ctl::save_playlist(&app);
    if let Some(track) = current_hit {
        reload_current(&app, track).await?;
    }
    Ok(())
}

/// 批量设置条目难度(多选右键「设置难度」):按策略解析每个谱面集的
/// 一档难度并锁定(mods 不动)。`target` 与 target_star 同编码:
/// None = 最难,Some(-1) = 最简单,Some(x>0) = 星级最接近 x。
/// 含当前播放条目时原位重载。
#[tauri::command]
async fn queue_set_diff(app: AppHandle, qids: Vec<u64>, target: Option<f64>) -> Result<(), String> {
    if qids.is_empty() {
        return Ok(());
    }
    let app2 = app.clone();
    let (current_hit, resolved) = tauri::async_runtime::spawn_blocking(move || {
        let state = app2.state::<ctl::WallState>();
        let source = state.settings.lock().unwrap().source.clone();
        let Ok(lib) = ctl::library(&app2, &source, false) else {
            return Err::<_, String>("曲库未就绪,稍后再试".into());
        };
        let policy = playlist::policy_of(target);
        let mut current_hit = None;
        let mut resolved = 0usize;
        let mut playlist = state.playlist.lock().unwrap();
        let cur_qid = playlist.current().map(|t| t.qid);
        for qid in &qids {
            let Some(track) = playlist.queue().into_iter().find(|t| t.qid == *qid) else { continue };
            let Some(set) = lib.sets.iter().find(|s| s.id == track.set_id) else { continue };
            let diff = playlist::pick_diff(set, policy);
            let mods = track.mods;
            if playlist.set_entry(*qid, mods, Some(diff.sha2.clone())).is_some() {
                resolved += 1;
                if cur_qid == Some(*qid) {
                    current_hit = playlist.current().filter(|c| c.qid == *qid);
                }
            }
        }
        Ok((current_hit, resolved))
    })
    .await
    .map_err(|e| e.to_string())??;
    if resolved == 0 {
        return Err("没有可设置的条目(曲库中不存在?)".into());
    }
    ctl::save_playlist(&app);
    if let Some(track) = current_hit {
        reload_current(&app, track).await?;
    }
    Ok(())
}

/// 从播放列表移除一首(按队列 qid)。
#[tauri::command]
async fn playlist_remove(app: AppHandle, qid: u64) -> Result<(), String> {
    app.state::<ctl::WallState>().playlist.lock().unwrap().remove(qid);
    ctl::save_playlist(&app);
    Ok(())
}

/// 清空播放列表。
#[tauri::command]
async fn playlist_clear(app: AppHandle) -> Result<(), String> {
    app.state::<ctl::WallState>().playlist.lock().unwrap().clear();
    ctl::save_playlist(&app);
    Ok(())
}

/// 播放曲库谱面。`sha2 = None` = 谱面集级播放(加载时按目标星级解析
/// 难度);指定难度 = 固定难度(播放条难度选择器切换)。曲目在播放列表
/// 中则对齐播放位置;不在列表 = 临时播放,**不自动入列**。
#[tauri::command]
async fn library_play(app: AppHandle, set_id: String, sha2: Option<String>) -> Result<(), String> {
    // 曲库访问(可能撞上正在进行的解析,单飞锁上等待)与 blob 校验都在
    // 阻塞线程池做,不占 async 运行时线程。
    let app2 = app.clone();
    let mut track = tauri::async_runtime::spawn_blocking(move || -> Result<Track, String> {
        let source = app2.state::<ctl::WallState>().settings.lock().unwrap().source.clone();
        let lib = ctl::library(&app2, &source, false)?;
        if !lib.sets.iter().any(|s| s.id == set_id) {
            return Err("谱面集不在曲库中".into());
        }
        Ok(Track { set_id, sha2, qid: 0, ..Default::default() })
    })
    .await
    .map_err(|e| e.to_string())??;
    {
        let state = app.state::<ctl::WallState>();
        let mut playlist = state.playlist.lock().unwrap();
        if playlist.align_current(track.set_id.clone(), track.sha2.clone()) {
            // 点播的是列表内条目:条目的 mods 与锁定难度权威(右键设置的
            // 设置跟着条目走,双击/难度切换不会把它丢掉)
            if let Some(cur) = playlist.current() {
                track.mods = cur.mods;
                if cur.sha2.is_some() {
                    track.sha2 = cur.sha2;
                }
            }
        }
    }
    ctl::save_playlist(&app);
    // 当前曲目记忆在 load_track 内统一维护(所有点播/切歌路径同源)
    tauri::async_runtime::spawn_blocking(move || ctl::load_track(&app, &track))
        .await
        .map_err(|e| e.to_string())?
}

/// 设置播放模式。曲目列表保持不变(随机模式对现有曲目重洗),收藏夹
/// 现在只是曲库前端的过滤器,不再驱动播放列表重建。
#[tauri::command]
async fn playlist_set(
    app: AppHandle,
    mode: String,
    collection: Option<String>,
) -> Result<(), String> {
    let mode = PlayMode::parse(&mode).ok_or_else(|| format!("未知播放模式：{mode}"))?;
    app.state::<ctl::WallState>().playlist.lock().unwrap().set_mode(mode);
    let state = app.state::<ctl::WallState>();
    let mut s = state.settings.lock().unwrap();
    s.play_mode = mode;
    s.collection = collection;
    settings::save(&app, &s);
    Ok(())
}

/// 手动排序(顺序模式拖动列表行):from → to(入列顺序下标)。
#[tauri::command]
async fn playlist_reorder(app: AppHandle, from: usize, to: usize) -> Result<(), String> {
    app.state::<ctl::WallState>().playlist.lock().unwrap().reorder(from, to);
    ctl::save_playlist(&app);
    Ok(())
}

#[tauri::command]
async fn skip_next(app: AppHandle) -> Result<(), String> {
    if !app.state::<ctl::WallState>().from_library.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    let next = app.state::<ctl::WallState>().playlist.lock().unwrap().advance();
    if let Some(track) = next {
        return tauri::async_runtime::spawn_blocking(move || ctl::load_track(&app, &track))
            .await
            .map_err(|e| e.to_string())?;
    }
    // 单曲循环/空列表:重播当前
    ctl::send_cmd(&app, Command::Seek { ms: 0.0 })?;
    ctl::send_cmd(&app, Command::Resume)
}

#[tauri::command]
async fn skip_prev(app: AppHandle) -> Result<(), String> {
    if !app.state::<ctl::WallState>().from_library.load(std::sync::atomic::Ordering::Relaxed) {
        return ctl::send_cmd(&app, Command::Seek { ms: 0.0 });
    }
    let prev = app.state::<ctl::WallState>().playlist.lock().unwrap().rewind();
    if let Some(track) = prev {
        return tauri::async_runtime::spawn_blocking(move || ctl::load_track(&app, &track))
            .await
            .map_err(|e| e.to_string())?;
    }
    ctl::send_cmd(&app, Command::Seek { ms: 0.0 })
}

#[tauri::command]
fn set_volume(app: AppHandle, v: f32) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.volume = v.clamp(0.0, 1.0);
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetVolume { v: v.clamp(0.0, 1.0) })
}

/// 打击音效开关(关 = 音量 0)。
#[tauri::command]
fn set_hitsound(app: AppHandle, on: bool) -> Result<(), String> {
    let v = {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.hitsound = on;
        settings::save(&app, &s);
        if on { s.hits_volume } else { 0.0 }
    };
    ctl::send_cmd(&app, Command::SetHitsVolume { v })
}

/// 打击音效音量(仅在开关开启时生效)。
#[tauri::command]
fn set_hits_volume(app: AppHandle, v: f32) -> Result<(), String> {
    let send = {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.hits_volume = v.clamp(0.0, 1.0);
        settings::save(&app, &s);
        s.hitsound
    };
    if send {
        ctl::send_cmd(&app, Command::SetHitsVolume { v: v.clamp(0.0, 1.0) })?;
    }
    Ok(())
}

/// 音效偏移(ms):正值 = 提前,负值 = 延后;实时生效。
#[tauri::command]
fn set_audio_offset(app: AppHandle, ms: f32) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.audio_offset_ms = ms.clamp(-1000.0, 1000.0);
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetOffset { ms: ms.clamp(-1000.0, 1000.0) })
}

/// 总音量(主增益):同时作用于音乐与打击音效。
#[tauri::command]
fn set_master_volume(app: AppHandle, v: f32) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.master_volume = v.clamp(0.0, 1.0);
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetMaster { v: v.clamp(0.0, 1.0) })
}

/// 帧率上限(0 = 跟随屏幕刷新率;30/60/120/240/360)。
#[tauri::command]
fn set_fps(app: AppHandle, fps: u32) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.fps = fps;
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetFps { fps })
}

/// 谱面集封面(data URL,父进程缓存;无封面返回 None)。读 blob +
/// base64 编码是 MB 级 IO,挪到阻塞线程池。
#[tauri::command]
async fn set_cover(app: AppHandle, set_id: String) -> Option<String> {
    {
        let state = app.state::<ctl::WallState>();
        let cache = state.cover_cache.lock().unwrap();
        if let Some(hit) = cache.get(&set_id) {
            return hit.clone();
        }
    }
    let id = set_id.clone();
    let app2 = app.clone();
    let url = tauri::async_runtime::spawn_blocking(move || {
        (|| {
            let source = app2.state::<ctl::WallState>().settings.lock().unwrap().source.clone();
            let lib = ctl::library(&app2, &source, false).ok()?;
            let set = lib.sets.iter().find(|s| s.id == id)?;
            match &set.root {
                // stable:谱面集目录直接取图
                Some(_) => stable::cover_data_url(set),
                None => {
                    let root = lazer::data_root()?;
                    lazer::cover_data_url(&root, set)
                }
            }
        })()
    })
    .await
    .ok()
    .flatten();
    let state = app.state::<ctl::WallState>();
    let mut cache = state.cover_cache.lock().unwrap();
    if cache.len() >= 80 {
        cache.clear(); // 简单上限:超过即整体清空(封面可随时重读)
    }
    cache.insert(set_id, url.clone());
    url
}

/// 点播难度目标星数(None = 最难)。
#[tauri::command]
fn set_target_star(app: AppHandle, star: Option<f64>) -> Result<(), String> {
    let state = app.state::<ctl::WallState>();
    let mut s = state.settings.lock().unwrap();
    s.target_star = star;
    settings::save(&app, &s);
    Ok(())
}

/// 实际正在播放的曲库曲目(含手动选定难度;谱面集级/策略挑选 = sha2 None)。
/// `load_track` 每次载入都同步 lazer_set/lazer_sha2,这里即播放真相:
/// 前端 loaded 事件以此对齐,难度下拉跟随真实播放(手动改难度只作用
/// 当前曲目,队列自动切歌后回到「自动」)。
#[tauri::command]
fn playing_track(app: AppHandle) -> Option<crate::playlist::Track> {
    let s = app.state::<ctl::WallState>().settings.lock().unwrap().clone();
    s.lazer_set.map(|set_id| crate::playlist::Track { set_id, sha2: s.lazer_sha2, qid: 0, ..Default::default() })
}

/// 曲库全局星级下限(0 = 不过滤)。
#[tauri::command]
fn set_star_min(app: AppHandle, min: f64) -> Result<(), String> {
    let state = app.state::<ctl::WallState>();
    let mut s = state.settings.lock().unwrap();
    s.star_min = min;
    settings::save(&app, &s);
    Ok(())
}

/// 用系统默认浏览器打开链接(关于页的开源库 / 感谢名单跳转)。
/// webview 内不允许直接导航到外部地址,统一走这里。
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(format!("仅支持 http(s) 链接:{url}"));
    }
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("cmd")
        .args(["/C", "start", "", &url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开浏览器失败:{e}"))
}

/// 手动指定 ffmpeg/ffprobe 完整路径(None = 恢复 PATH 查找)。
/// 视频层解码与视频探测 / BGM 时长用;下次载入生效。
#[tauri::command]
fn set_ffmpeg_bins(
    app: AppHandle,
    ffmpeg: Option<String>,
    ffprobe: Option<String>,
) -> Result<(), String> {
    for (what, p) in [("ffmpeg", ffmpeg.as_deref()), ("ffprobe", ffprobe.as_deref())] {
        if let Some(p) = p {
            if !std::path::Path::new(p).is_file() {
                return Err(format!("{what}:文件不存在 {p}"));
            }
        }
    }
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.ffmpeg = ffmpeg.clone();
        s.ffprobe = ffprobe.clone();
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetFfmpegBins { ffmpeg, ffprobe })
}

/// 在 PATH 各目录中查找可执行文件(与子进程的隐式查找一致)。
fn find_on_path(name: &str) -> Option<std::path::PathBuf> {
    for dir in std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
    {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// ffmpeg/ffprobe 当前实际生效的完整路径与来源:手动指定 > 内置 >
/// PATH(都未找到为 null/"none",UI 显示"未找到")。
#[tauri::command]
fn ffmpeg_bin_status(app: AppHandle) -> Result<serde_json::Value, String> {
    let state = app.state::<ctl::WallState>();
    let s = state.settings.lock().unwrap();
    let resolve = |manual: &Option<String>, name: &str| -> (Option<String>, &'static str) {
        if let Some(p) = manual {
            return (Some(p.clone()), "manual");
        }
        if let Some(p) = ctl::bundled_bin(&app, name) {
            return (Some(p), "bundled");
        }
        if let Some(p) = find_on_path(name) {
            return (Some(p.to_string_lossy().into_owned()), "path");
        }
        (None, "none")
    };
    let (ffmpeg, ffmpeg_src) = resolve(&s.ffmpeg, "ffmpeg.exe");
    let (ffprobe, ffprobe_src) = resolve(&s.ffprobe, "ffprobe.exe");
    Ok(serde_json::json!({
        "ffmpeg": ffmpeg,
        "ffprobe": ffprobe,
        "ffmpegSrc": ffmpeg_src,
        "ffprobeSrc": ffprobe_src,
    }))
}

/// HUD 开关(壁纸默认关)。
#[tauri::command]
fn set_hud(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.hud = on;
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetHud { on })
}

/// PP 计数器开关(HUD 的子项,默认开)。实时生效:只控制显示;PP 时间
/// 线在加载期随 HUD 一起计算,与该开关无关。
#[tauri::command]
fn set_pp(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.pp = on;
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetPp { on })
}

/// 减少打击动画开关(默认关 = 完整动画)。开 = lazer "hit animations"
/// 关闭态(#38371):命中圆圈 60ms 整体淡出。实时生效(纯渲染旗标)。
#[tauri::command]
fn set_reduce_anim(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.reduce_anim = on;
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetHitAnimations { on: !on })
}

/// 休息段背景变亮开关(默认关):break 期间背景图暗度亮起 0.3,
/// 800ms OutQuint 淡变(前奏/曲末同算 break)。实时生效。
#[tauri::command]
fn set_break_lighten(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.break_lighten = on;
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetBreakLighten { on })
}

/// 光标渲染开关(默认开):关 = 光标与拖尾都不画。实时生效。
#[tauri::command]
fn set_cursor(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.cursor = on;
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetCursor { on })
}

/// 光标大小倍率(0.1–2.0,实时生效;光标与拖尾同步缩放)。
#[tauri::command]
fn set_cursor_size(app: AppHandle, x: f32) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.cursor_size = x.clamp(0.1, 2.0);
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetCursorSize { x: x.clamp(0.1, 2.0) })
}

/// BGM 淡入淡出开关(换曲淡出旧曲、起播淡入;只作用于 BGM)。
#[tauri::command]
fn set_fade_audio(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.fade_audio = on;
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetFade { on })
}

/// 隐藏游玩画面模式(只渲染背景 + storyboard,隐藏 gameplay 元素;音频照常)。
#[tauri::command]
fn set_gameplay_hidden(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.gameplay_hidden = on;
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetGameplayHidden { on })
}

// ---------- 皮肤(已安装列表解析 + lazer 归档挂载;不提供手动导入) ----------

/// 应用皮肤(None = 回内置 Argon)并保存;立即热切换生效 —— 不重载
/// 曲目、不打断音频,渲染端只重建皮肤相关资源(图集/皮肤缓存/combo
/// 色/音效采样)。未在播放时下一曲按保存的设置走。
/// `stored` 为皮肤身份值:realm:skin:<id> / stable 皮肤目录路径。
fn apply_skin_stored(app: &AppHandle, stored: Option<String>) -> Result<(), String> {
    let force = {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.skin = stored.clone();
        settings::save(app, &s);
        s.force_skin_colours
    };
    let dir = ctl::resolve_skin_dir(app, &stored).map(|p| p.to_string_lossy().into_owned());
    ctl::send_cmd(app, Command::SetSkin { skin: dir, force_colours: force })
}

/// 数据目录状态:手动设置值 + 实际生效路径(自动检测的也给出,供展示)。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirStatus {
    pub lazer_manual: Option<String>,
    pub lazer_effective: Option<String>,
    pub stable_manual: Option<String>,
    pub stable_effective: Option<String>,
}

#[tauri::command]
fn dir_status() -> DirStatus {
    DirStatus {
        lazer_manual: lazer::custom_data_dir().map(|p| p.display().to_string()),
        lazer_effective: lazer::realm_path().map(|p| p.display().to_string()),
        stable_manual: stable::custom_stable_dir().map(|p| p.display().to_string()),
        stable_effective: stable::stable_root().map(|p| p.display().to_string()),
    }
}

/// 选择数据目录(kind = "lazer" / "stable")的文件夹对话框。
#[tauri::command]
async fn pick_data_dir(app: AppHandle, kind: String) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let picked = app2.dialog().file().blocking_pick_folder();
        picked
            .and_then(|f| f.into_path().ok())
            .map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| e.to_string())
}

/// 设置(或清除,None)手动数据目录:校验、写入 override 与设置、清空
/// 曲库/封面缓存(下一轮按新目录重新解析)。
#[tauri::command]
async fn set_data_dir(app: AppHandle, kind: String, path: Option<String>) -> Result<(), String> {
    if !matches!(kind.as_str(), "lazer" | "stable") {
        return Err(format!("未知数据目录类型：{kind}"));
    }
    let dir = path.clone().map(std::path::PathBuf::from);
    // 校验(失败不落库)
    match kind.as_str() {
        "lazer" => lazer::set_custom_data_dir(dir.as_deref())?,
        _ => stable::set_custom_stable_dir(dir.as_deref())?,
    }
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        if kind == "lazer" {
            s.lazer_dir = path.clone();
        } else {
            s.stable_dir = path.clone();
        }
        settings::save(&app, &s);
        state.lib_cache.lock().unwrap().take();
        state.cover_cache.lock().unwrap().clear();
    }
    // 当前源若受影响:清理失效曲目并广播(前端重读曲库)
    let source = app.state::<ctl::WallState>().settings.lock().unwrap().source.clone();
    let src = source.clone();
    let app2 = app.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || {
        if let Ok(lib) = ctl::library(&app2, &src, false) {
            app2.state::<ctl::WallState>().playlist.lock().unwrap().prune_missing(&lib);
            ctl::save_playlist(&app2);
        }
    })
    .await;
    let _ = app.emit("wall://source", &source);
    Ok(())
}

/// 枚举可用皮肤:lazer(`<数据目录>/skins`)与 stable(`<osu!>/Skins`)
/// 的已安装皮肤(子目录即皮肤),供下拉选择。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkinChoice {
    pub name: String,
    pub path: String,
    /// "lazer" / "stable"。
    pub source: String,
}

fn list_skin_dir(root: &std::path::Path, source: &str, out: &mut Vec<SkinChoice>) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let file_name = e.file_name().to_string_lossy().into_owned();
        if file_name.is_empty() {
            continue;
        }
        if p.is_dir() {
            // stable / 解包形态:子目录即皮肤
            out.push(SkinChoice {
                name: file_name,
                path: p.to_string_lossy().into_owned(),
                source: source.into(),
            });
        } else {
            // lazer 布局:skins/<名称>.osk 归档,选中时才挂载(解包到缓存)。
            // 文件名可带 .osk_N 版本后缀,显示名取 .osk 之前的部分。
            let lower = file_name.to_lowercase();
            if let Some(idx) = lower.find(".osk") {
                let name = file_name[..idx].to_string();
                if !name.is_empty() {
                    out.push(SkinChoice {
                        name,
                        path: p.to_string_lossy().into_owned(),
                        source: source.into(),
                    });
                }
            }
        }
    }
}

#[tauri::command]
async fn skin_list(app: AppHandle) -> Result<Vec<SkinChoice>, String> {
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut skins = Vec::new();
        // lazer:已安装皮肤解析自 client.realm(class_Skin)——
        // realm 里的文件清单指向 files/ blob,选中时才物化挂载
        let source = app2.state::<ctl::WallState>().settings.lock().unwrap().source.clone();
        if source != "stable" {
            if let Ok(lib) = ctl::library(&app2, "lazer", false) {
                for skin in &lib.skins {
                    skins.push(SkinChoice {
                        name: if skin.creator.is_empty() {
                            skin.name.clone()
                        } else {
                            format!("{} ({})", skin.name, skin.creator)
                        },
                        path: format!("realm:skin:{}", skin.id),
                        source: "lazer".into(),
                    });
                }
            }
        }
        // 磁盘形态:stable Skins/ 目录与 .osk 归档;lazer skins/ 若存在
        // (导出布局)也一并给出
        if let Some(root) = lazer::data_root() {
            list_skin_dir(&root.join("skins"), "lazer", &mut skins);
        }
        if let Some(root) = stable::stable_root() {
            list_skin_dir(&root.join("Skins"), "stable", &mut skins);
        }
        skins.sort_by(|a, b| a.source.cmp(&b.source).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(skins)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 选择皮肤(None = 内置 Argon);立即重载当前曲目生效。
/// 只允许 osu! 自带来源:lazer 已安装皮肤(`realm:skin:<id>`,播放时
/// 按需幂等挂载)与 stable Skins/ 目录;不存在外部皮肤导入。
/// 存档持久化该身份值,重启后与 skin_list 选项精确匹配、正确恢复。
#[tauri::command]
async fn set_skin(app: AppHandle, path: Option<String>) -> Result<(), String> {
    let app2 = app.clone();
    let owned = path.map(std::path::PathBuf::from);
    tauri::async_runtime::spawn_blocking(move || {
        let stored = match owned {
            None => None,
            Some(p) => {
                let s = p.to_string_lossy().into_owned();
                if let Some(id) = s.strip_prefix("realm:skin:") {
                    // lazer 已安装皮肤:校验 realm 内存在该 id
                    if ctl::resolve_skin_dir(&app2, &Some(s.clone())).is_none() {
                        return Err(format!("皮肤不可用:realm:skin:{id}"));
                    }
                    Some(s)
                } else if p.is_dir() {
                    // stable Skins/ 下的皮肤目录
                    Some(s)
                } else {
                    return Err(format!("皮肤不存在:{}", p.display()));
                }
            }
        };
        apply_skin_stored(&app2, stored)
    })
    .await
    .map_err(|e| format!("{e}"))?
}

/// 强制使用皮肤的 combo 颜色(覆盖谱面 [Colours]);加载期生效,
/// 变更后重载当前曲目。
#[tauri::command]
fn set_force_skin_colours(app: AppHandle, on: bool) -> Result<(), String> {
    let dir = {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.force_skin_colours = on;
        settings::save(&app, &s);
        ctl::resolve_skin_dir(&app, &s.skin).map(|p| p.to_string_lossy().into_owned())
    };
    ctl::send_cmd(&app, Command::SetSkin { skin: dir, force_colours: on })
}

/// 渲染模式:"always" 始终渲染 / "autopause" 全屏遮挡时不渲染画面(默认)/
/// "fs_pause" 全屏时暂停播放(壁纸保留)/ "fs_sleep" 全屏时暂停并休眠
/// (拆除壁纸会话释放内存)/ "off" 播放器模式(不渲染,只播音乐 + 音效)。
/// 切换无缝:不重载曲目、不打断音频,仅拆/建渲染会话与暂停/恢复播放。
#[tauri::command]
fn set_render_mode(app: AppHandle, mode: String) -> Result<(), String> {
    if !matches!(mode.as_str(), "always" | "autopause" | "fs_pause" | "fs_sleep" | "off") {
        return Err(format!("未知渲染模式：{mode}"));
    }
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.render_mode = mode.clone();
        // 同步旧字段,保证下次读取的迁移逻辑推出相同模式
        s.pure_audio = mode == "off";
        s.auto_pause_render = mode != "always";
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetRenderMode { mode })
}

/// 背景亮度(0.0–1.0),实时生效。
#[tauri::command]
fn set_bg_opacity(app: AppHandle, v: f32) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.bg_opacity = v.clamp(0.0, 1.0);
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetBgOpacity { v: v.clamp(0.0, 1.0) })
}

/// 加载期开关(HD/storyboard/视频)变更后:正在播曲库曲目则立即重载
/// 当首,让设置当场可见。
fn reload_current_track(app: &AppHandle) {
    if !app.state::<ctl::WallState>().from_library.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // 原位重载:记住当前播放位置,不回到开头
    let t = match &*app.state::<ctl::WallState>().last_event.lock().unwrap() {
        Some(ipc::Event::Status { t_ms, .. }) => t_ms.max(0.0),
        _ => 0.0,
    };
    let state = app.state::<ctl::WallState>();
    let s = state.settings.lock().unwrap();
    if let Some(set_id) = s.lazer_set.clone() {
        drop(s);
        let _ = ctl::load_track_at(app, &playlist::Track { set_id, sha2: None, qid: 0, ..Default::default() }, t);
    }
}

/// HD(Hidden)视觉开关:纯渲染旗标,实时生效,不重载不打断。
#[tauri::command]
fn set_hidden(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.hidden = on;
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetHidden { on })
}

/// storyboard 渲染开关(关 = 完全跳过解析与渲染,无论谱面是否存在;
/// 巨型 SB 可省数秒加载)。加载期生效,变更后重载当前曲目。
#[tauri::command]
fn set_storyboard(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.storyboard = on;
        settings::save(&app, &s);
    }
    reload_current_track(&app);
    Ok(())
}

/// 谱面自带音效开关(lazer "Beatmap hitsounds",默认开):谱面集内的
/// 采样文件按槽位优先于皮肤(custom sample bank 索引参与时)。运行期
/// **无缝热切换**:壁纸端只重建采样表,音乐/画面/进度/游标都不动,
/// 不重载不打断;下次载入曲目按设置走对应层。
#[tauri::command]
fn set_beatmap_hitsounds(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.beatmap_hitsounds = on;
        settings::save(&app, &s);
    }
    ctl::send_cmd(&app, Command::SetBeatmapHitsounds { on })
}

/// 关闭主窗口的执行(询问弹窗选择后调用;remember = 记住该选择)。
#[tauri::command]
fn close_window(app: AppHandle, window: tauri::WebviewWindow, mode: String, remember: bool) {
    if remember && matches!(mode.as_str(), "tray" | "exit") {
        let state = app.state::<ctl::WallState>();
        let mut s = state.settings.lock().unwrap();
        s.close_action = Some(mode.clone());
        settings::save(&app, &s);
    }
    if mode == "exit" {
        quit_app_inner(&app);
    } else {
        let _ = window.hide();
    }
}

/// 设置关闭按钮行为:None = 每次询问;"tray" / "exit"。
#[tauri::command]
fn set_close_action(app: AppHandle, action: Option<String>) -> Result<(), String> {
    if let Some(a) = &action {
        if !matches!(a.as_str(), "tray" | "exit") {
            return Err(format!("未知关闭行为：{a}"));
        }
    }
    let state = app.state::<ctl::WallState>();
    let mut s = state.settings.lock().unwrap();
    s.close_action = action;
    settings::save(&app, &s);
    Ok(())
}

/// 双击曲库谱面行加入播放列表(默认关 = 双击立即播放)。纯 UI 行为,
/// 壁纸端无关。
#[tauri::command]
fn set_quick_add(app: AppHandle, on: bool) -> Result<(), String> {
    let state = app.state::<ctl::WallState>();
    let mut s = state.settings.lock().unwrap();
    s.quick_add = on;
    settings::save(&app, &s);
    Ok(())
}

/// 快速导入应用的默认 mods(osu! legacy 位)。
#[tauri::command]
fn set_quick_mods(app: AppHandle, mods: u32) -> Result<(), String> {
    let state = app.state::<ctl::WallState>();
    let mut s = state.settings.lock().unwrap();
    s.quick_mods = mods;
    settings::save(&app, &s);
    Ok(())
}

#[tauri::command]
fn set_dblclick_add(app: AppHandle, on: bool) -> Result<(), String> {
    let state = app.state::<ctl::WallState>();
    let mut s = state.settings.lock().unwrap();
    s.dblclick_add = on;
    settings::save(&app, &s);
    Ok(())
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    quit_app_inner(&app);
}
