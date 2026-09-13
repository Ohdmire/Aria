//! 父进程(Tauri UI)侧的壁纸子进程管理:
//! spawn 自身 `--wallpaper`,stdin 写 [`Command`],stdout 读 [`Event`]
//! 并以 Tauri 事件 `wall://event` 转发给前端;子进程退出时一并通知。
//! 曲库(osu!lazer)解析缓存与播放列表推进也在这里。

use crate::ipc::{Command, Event};
use crate::lazer::{self, LazerLibrary};
use crate::playlist::{PlayMode, Playlist, Track};
use crate::settings::Settings;
use crate::stable;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// 托管到 Tauri 的全局状态。
pub struct WallState {
    pub ctl: std::sync::Mutex<Option<ChildCtl>>,
    pub settings: std::sync::Mutex<Settings>,
    pub last_event: std::sync::Mutex<Option<Event>>,
    /// 曲库解析缓存(键 = 源 "lazer"/"stable")。
    pub lib_cache: std::sync::Mutex<Option<(String, Arc<LazerLibrary>)>>,
    pub playlist: std::sync::Mutex<Playlist>,
    /// 当前播放是否来自曲库(决定 Ended 后是否自动切歌)。
    pub from_library: AtomicBool,
    /// 谱面集封面 data URL 缓存(None = 无封面,避免重复读盘)。
    pub cover_cache: std::sync::Mutex<std::collections::HashMap<String, Option<String>>>,
}

impl WallState {
    pub fn new(settings: Settings) -> WallState {
        WallState {
            ctl: std::sync::Mutex::new(None),
            settings: std::sync::Mutex::new(settings),
            last_event: std::sync::Mutex::new(None),
            lib_cache: std::sync::Mutex::new(None),
            playlist: std::sync::Mutex::new(Playlist::default()),
            from_library: AtomicBool::new(false),
            cover_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

/// 取曲库(带缓存,键 = 源);`refresh` 强制重新解析。
/// 缓存互斥锁**贯穿整个解析**(单飞):启动恢复与 UI 刷新并发到达时,
/// 后来者在锁上等待,拿到前者刚写入的结果 —— 否则两次解析会同时
/// 复制/映射同一个快照临时文件,产生 os 1224/10049 偶发错误。
pub fn library(app: &AppHandle, source: &str, refresh: bool) -> Result<Arc<LazerLibrary>, String> {
    let state = app.state::<WallState>();
    let mut guard = state.lib_cache.lock().unwrap();
    if refresh || guard.as_ref().is_some_and(|(s, _)| s != source) {
        *guard = None;
    }
    if let Some((_, lib)) = guard.clone() {
        return Ok(lib);
    }
    let lib = std::panic::catch_unwind(|| match source {
        "stable" => {
            let root = stable::stable_root()
                .ok_or_else(|| "未找到 osu!stable 安装目录(需要 osu!.db)".to_string())?;
            stable::library(&root)
        }
        _ => {
            let realm = lazer::realm_path()
                .ok_or_else(|| "未找到 osu!lazer 数据目录(client.realm)".to_string())?;
            lazer::parse(&realm)
        }
    })
    .unwrap_or_else(|_| Err("读取曲库时发生内部错误".into()))
    .map(Arc::new)?;
    *guard = Some((source.to_string(), lib.clone()));
    drop(guard);
    // 数据库刚解析:顺手清理播放列表里已不存在的曲目(启动快路径跳过
    // 的 prune 延迟到这里;跨源残留同样在此清除)。锁序:本函数的调用
    // 方都不持 playlist 锁,安全。
    {
        let state = app.state::<WallState>();
        state.playlist.lock().unwrap().prune_missing(&lib);
    }
    save_playlist(app);
    Ok(lib)
}

pub struct ChildCtl {
    pub child: Child,
    stdin: std::sync::Mutex<ChildStdin>,
}

impl ChildCtl {
    pub fn send(&self, cmd: &Command) -> Result<(), String> {
        let mut line = serde_json::to_string(cmd).map_err(|e| e.to_string())?;
        line.push('\n');
        let mut w = self.stdin.lock().map_err(|e| e.to_string())?;
        w.write_all(line.as_bytes()).map_err(|e| e.to_string())
    }

    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

/// 确保壁纸子进程在运行;不在/已死则(重新)拉起。
pub fn ensure_child(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<WallState>();
    let mut ctl = state.ctl.lock().unwrap();
    if let Some(c) = ctl.as_mut() {
        if c.alive() {
            return Ok(());
        }
    }
    if let Some(mut old) = ctl.take() {
        let _ = old.child.kill();
        let _ = old.child.wait();
    }
    let child = spawn_child(app).map_err(|e| e.to_string())?;
    *ctl = Some(child);
    Ok(())
}

/// 内置 ffmpeg/ffprobe 完整路径(NSIS 资源目录 bin/ 下;不带内置的
/// 构建变体里不存在)。解析优先级:手动指定 > 内置 > PATH。
pub fn bundled_bin(app: &AppHandle, name: &str) -> Option<String> {
    use tauri::Manager;
    let dir = app.path().resource_dir().ok()?;
    let p = dir.join("bin").join(name);
    p.is_file().then(|| p.to_string_lossy().into_owned())
}

/// 皮肤身份值 → 实际皮肤目录(realm 皮肤按需幂等挂载;其余按普通
/// 目录路径,如 stable Skins/)。目录不存在 = None(回默认皮肤)。
pub fn resolve_skin_dir(app: &AppHandle, stored: &Option<String>) -> Option<std::path::PathBuf> {
    use tauri::Manager;
    let s = stored.as_deref()?;
    if let Some(id) = s.strip_prefix("realm:skin:") {
        let root = crate::lazer::data_root()?;
        let source = app.state::<WallState>().settings.lock().unwrap().source.clone();
        let lib = library(app, &source, false).ok()?;
        let skin = lib.skins.iter().find(|s| s.id == id)?;
        let cache = app.path().app_data_dir().ok()?.join("skin-cache");
        return crate::lazer::mount_skin(&root, skin, &cache).ok();
    }
    let p = std::path::PathBuf::from(s);
    p.is_dir().then_some(p)
}

fn spawn_child(app: &AppHandle) -> anyhow::Result<ChildCtl> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--wallpaper")
        // DX12 后端:省掉 Vulkan 驱动栈与第三方 hook 层(OBS 等)的常驻
        // 内存(实测比 Vulkan 少 ~57MB)。注意 osu-replay-render 侧需
        // Backends::from_env() 才生效 —— InstanceDescriptor::default()
        // 会无视环境变量。
        .env("WGPU_BACKEND", "dx12")
        // MSAA 恒定关闭(1080p 拉伸下肉眼无差,省 4× 片元开销与显存)
        .env("NO_MSAA", "1")
        // 壁纸端从不回读像素:跳过 3×帧大小的 MAP_READ 暂存堆
        //(2560×1440 下约 44MB 提交内存)
        .env("NO_READBACK", "1")
        // storyboard CPU 解码缓存预算调低(上游缺省 384MB;壁纸 24-60fps
        // 消费,192MB 足够,超限只是偶发重解码)
        .env("SB_CACHE_MB", "192")
        // storyboard GPU 贴图预算放大(上游缺省 512MB):超预算 LRU 淘汰后
        // 贴图集中回归的那一帧要整批重新上线,是"突然卡一下再顺畅"的
        // 来源;1024MB 让绝大多数谱面永不淘汰,用显存换流畅
        .env("SB_GPU_MB", "1024");
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");

    // 先按保存的音量/HUD/渲染模式/背景亮度/总音量/日志设置子进程,再开始转发事件
    let (bgm_vol, hits_on, hits_vol, hud, pp, hit_anim, brk_lighten, cursor_on, cursor_sz, render_mode, bg_opacity, master, offset, ghidden, fade, ffmpeg, ffprobe, log_on) = {
        let state = app.state::<WallState>();
        let s = state.settings.lock().unwrap();
        (
            s.volume,
            s.hitsound,
            s.hits_volume,
            s.hud,
            s.pp,
            !s.reduce_anim,
            s.break_lighten,
            s.cursor,
            s.cursor_size,
            s.render_mode.clone(),
            s.bg_opacity,
            s.master_volume,
            s.audio_offset_ms,
            s.gameplay_hidden,
            s.fade_audio,
            s.ffmpeg.clone().or_else(|| bundled_bin(app, "ffmpeg.exe")),
            s.ffprobe.clone().or_else(|| bundled_bin(app, "ffprobe.exe")),
            s.log_enabled,
        )
    };
    {
        use std::io::Write as _;
        let vol_cmd = serde_json::to_string(&Command::SetVolume { v: bgm_vol }).unwrap();
        let hits_cmd = serde_json::to_string(&Command::SetHitsVolume {
            v: if hits_on { hits_vol } else { 0.0 },
        })
        .unwrap();
        let hud_cmd = serde_json::to_string(&Command::SetHud { on: hud }).unwrap();
        let pp_cmd = serde_json::to_string(&Command::SetPp { on: pp }).unwrap();
        let ha_cmd = serde_json::to_string(&Command::SetHitAnimations { on: hit_anim }).unwrap();
        let bl_cmd = serde_json::to_string(&Command::SetBreakLighten { on: brk_lighten }).unwrap();
        let cur_cmd = serde_json::to_string(&Command::SetCursor { on: cursor_on }).unwrap();
        let curs_cmd = serde_json::to_string(&Command::SetCursorSize { x: cursor_sz }).unwrap();
        let mode_cmd = serde_json::to_string(&Command::SetRenderMode { mode: render_mode }).unwrap();
        let bg_cmd = serde_json::to_string(&Command::SetBgOpacity { v: bg_opacity }).unwrap();
        let master_cmd = serde_json::to_string(&Command::SetMaster { v: master }).unwrap();
        let offset_cmd = serde_json::to_string(&Command::SetOffset { ms: offset }).unwrap();
        let gh_cmd = serde_json::to_string(&Command::SetGameplayHidden { on: ghidden }).unwrap();
        let fade_cmd = serde_json::to_string(&Command::SetFade { on: fade }).unwrap();
        let bins_cmd = serde_json::to_string(&Command::SetFfmpegBins { ffmpeg, ffprobe }).unwrap();
        let log_cmd = serde_json::to_string(&Command::SetLog { on: log_on }).unwrap();
        let _ = (&stdin).write_all(
            format!("{vol_cmd}\n{hits_cmd}\n{hud_cmd}\n{pp_cmd}\n{ha_cmd}\n{bl_cmd}\n{cur_cmd}\n{curs_cmd}\n{mode_cmd}\n{bg_cmd}\n{master_cmd}\n{offset_cmd}\n{gh_cmd}\n{fade_cmd}\n{bins_cmd}\n{log_cmd}\n").as_bytes(),
        );
    }

    // 读事件线程:JSON-lines → Tauri 事件;缓存最新状态供 UI 拉取
    let app2 = app.clone();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let Ok(ev) = serde_json::from_str::<Event>(&line) else { continue };
            if matches!(
                ev,
                Event::Status { .. } | Event::Loaded { .. } | Event::Unloaded | Event::Exited
            ) {
                *app2.state::<WallState>().last_event.lock().unwrap() = Some(ev.clone());
            }
            // 曲库播放模式:曲目结束自动切下一首
            if matches!(ev, Event::Ended) {
                on_ended(&app2);
            }
            let _ = app2.emit("wall://event", &ev);
            if matches!(ev, Event::Exited) {
                break;
            }
        }
        // 进程死亡(而非优雅退出)同样通知前端;Exited 去重交给前端
        let _ = app2.emit("wall://event", &Event::Exited);
    });

    Ok(ChildCtl { child, stdin: std::sync::Mutex::new(stdin) })
}

/// 曲目结束:曲库源且非单曲循环 → advance 下一首并加载。
fn on_ended(app: &AppHandle) {
    if !app.state::<WallState>().from_library.load(Ordering::Relaxed) {
        return;
    }
    let next = app.state::<WallState>().playlist.lock().unwrap().advance();
    if let Some(track) = next {
        save_playlist(app);
        let _ = load_track(app, &track);
    }
}

/// 托盘/菜单:切到下一首(单曲循环模式也手动前进)。
pub fn playlist_next(app: &AppHandle) -> Result<(), String> {
    let next = {
        let state = app.state::<WallState>();
        state.playlist.lock().unwrap().advance_manual()
    };
    match next {
        Some(track) => {
            save_playlist(app);
            load_track(app, &track)
        }
        None => Ok(()),
    }
}

/// 轻量模式:释放可再生的界面侧胖缓存(封面 base64 等)。webview 已随
/// 窗口销毁释放,壁纸子进程的渲染资源不在本进程,不受影响。
/// 曲库解析缓存(lib_cache)**保留**:realm 重解析 + 全量 blob 存在性
/// stat 是秒级操作,托盘切歌/自动连播/恢复全走它——清掉就是"托盘点
/// 下一首要等很久"。它是纯元数据(无 blob/封面),几个 MB,留着不亏。
pub fn release_caches(app: &AppHandle) {
    let state = app.state::<WallState>();
    state.cover_cache.lock().unwrap().clear();
}

/// 托盘/菜单:切到上一首(单曲循环模式为空操作)。
pub fn playlist_prev(app: &AppHandle) -> Result<(), String> {
    let prev = {
        let state = app.state::<WallState>();
        state.playlist.lock().unwrap().rewind()
    };
    match prev {
        Some(track) => {
            save_playlist(app);
            load_track(app, &track)
        }
        None => Ok(()),
    }
}

/// 播放列表持久化(曲目 + 当前位置)。
pub fn save_playlist(app: &AppHandle) {
    let state = app.state::<WallState>();
    let (tracks, pos) = state.playlist.lock().unwrap().snapshot();
    let mut s = state.settings.lock().unwrap();
    s.playlist = tracks
        .into_iter()
        .map(|t| crate::settings::SavedTrack {
            set_id: t.set_id,
            sha2: t.sha2,
            mods: t.mods,
            title: t.title,
            artist: t.artist,
            length_ms: t.length_ms,
        })
        .collect();
    s.playlist_pos = pos;
    crate::settings::save(app, &s);
}

/// 已解析曲库的缓存视图(不做解析):播放列表秒显路径用 —— 没有缓存
/// 时返回 None,调用方退回曲目自带的显示快照。
pub fn library_cached(app: &AppHandle) -> Option<std::sync::Arc<lazer::LazerLibrary>> {
    let state = app.state::<WallState>();
    state
        .lib_cache
        .lock()
        .unwrap()
        .clone()
        .map(|(_, lib)| lib)
}

/// 让壁纸子进程加载该曲目。难度解析发生在**播放时**:谱面集级条目
/// (sha2 = None)按当前目标星级挑选,指定难度的条目按指定播。全程零拷贝:
/// - stable:谱面集目录本身就是解包形态,直接传完整 .osu 路径;
/// - lazer:.osu 直接传 files/ blob 路径,谱面集文件名 → blob 路径的映射
///   表(manifest)随命令下发,渲染端按名解析音频/背景/storyboard/视频,
///   不再把谱面集复制物化成普通目录(消除"复制失败 os error 3"一类错误)。
pub fn load_track(app: &AppHandle, track: &Track) -> Result<(), String> {
    load_track_at(app, track, 0.0)
}

/// 统一的 Load 派发尾部:设置快照 + 记住当前曲目 + 回填显示快照 +
/// 下发命令 + 广播。快/慢路径共用。
#[allow(clippy::too_many_arguments)]
fn dispatch_load(
    app: &AppHandle,
    track: &Track,
    osu_path: String,
    manifest: Option<Vec<crate::ipc::VFile>>,
    sha2: String,
    title: String,
    artist: String,
    length_ms: f64,
    start: f32,
) -> Result<(), String> {
    // 视频/故事板单一开关(lazer ShowStoryboard:关 = 精灵与视频一起关)
    let (fail, speed, loop_playback, skin_stored, force_colours, hidden, storyboard, beatmap_hitsounds, upscale) = {
        let state = app.state::<WallState>();
        let s = state.settings.lock().unwrap();
        (
            s.fail,
            s.speed,
            s.loop_playback,
            s.skin.clone(),
            s.force_skin_colours,
            s.hidden,
            s.storyboard,
            s.beatmap_hitsounds,
            s.upscale.clone(),
        )
    };
    // 皮肤身份值 → 实际目录(realm 挂载/缓存定位;失效回默认皮肤)
    let skin = resolve_skin_dir(app, &skin_stored).map(|p| p.to_string_lossy().into_owned());
    app.state::<WallState>().from_library.store(true, Ordering::Relaxed);
    // 单一真相源:每次实际载入都记住当前曲目(点播/托盘切歌/自动连播/
    // 设置重载统一走这里)。
    {
        let state = app.state::<WallState>();
        let mut s = state.settings.lock().unwrap();
        s.lazer_set = Some(track.set_id.clone());
        s.lazer_sha2 = track.sha2.clone();
        crate::settings::save(app, &s);
    }
    // 回填条目显示快照(播放解析出的实际难度时长),列表秒显信息保鲜
    {
        let probe = Track { set_id: track.set_id.clone(), sha2: Some(sha2.clone()), ..Default::default() };
        let state = app.state::<WallState>();
        state.playlist.lock().unwrap().backfill_meta(&probe, title, artist, length_ms);
        drop(state);
        save_playlist(app);
    }
    send_cmd(
        app,
        Command::Load {
            path: osu_path,
            diff: None,
            fail,
            speed,
            start: start.max(0.0),
            loop_playback,
            mods: track.mods,
            manifest,
            skin,
            force_colours,
            hidden,
            storyboard,
            video: storyboard,
            beatmap_hitsounds,
            upscale,
        },
    )?;
    // 广播**已解析**难度(前端难度选择器/标题据此刷新)
    let _ = app.emit(
        "wall://track",
        &Track { set_id: track.set_id.clone(), sha2: Some(sha2), qid: track.qid, ..Default::default() },
    );
    Ok(())
}

/// [`load_track`] + 起始位置(设置开关后原位重载用:storyboard/皮肤等
/// 加载期选项变更不回到开头)。
pub fn load_track_at(app: &AppHandle, track: &Track, start: f32) -> Result<(), String> {
    // 播放时难度解析:指定难度 > 目标星级(改设置后重播/切歌即生效)
    let target = app.state::<WallState>().settings.lock().unwrap().target_star;

    // ---- 快路径:路径缓存命中 → 零数据库直接下发(托盘切歌/启动恢复
    // 秒切;数据库只在打开曲库时才解析)----
    if let Some(cached) = crate::pathcache::get(app, &track.set_id) {
        let picked = track
            .sha2
            .as_ref()
            .and_then(|s| cached.diffs.iter().find(|d| &d.sha2 == s))
            .or_else(|| crate::playlist::pick_cached_diff(&cached.diffs, crate::playlist::policy_of(target)));
        match picked {
            Some(diff) if std::path::Path::new(&diff.osu_path).is_file() => {
                let title = if track.title.is_empty() { diff.name.clone() } else { track.title.clone() };
                let length_ms = if track.length_ms > 0.0 { track.length_ms } else { diff.length_ms };
                return dispatch_load(
                    app,
                    track,
                    diff.osu_path.clone(),
                    cached.manifest.clone(),
                    diff.sha2.clone(),
                    title,
                    track.artist.clone(),
                    length_ms,
                    start,
                );
            }
            // 路径失效(地图被删/存储迁移):丢缓存走慢路径重建
            _ => crate::pathcache::remove(app, &track.set_id),
        }
    }

    // ---- 慢路径:解析曲库(现状逻辑),成功后回写路径缓存 ----
    let source = app.state::<WallState>().settings.lock().unwrap().source.clone();
    let lib = library(app, &source, false)?;
    let set = lib
        .sets
        .iter()
        .find(|s| s.id == track.set_id)
        .ok_or("谱面集不在曲库中(曲库可能已更新,请刷新)")?;
    let sha2 = track
        .sha2
        .clone()
        .unwrap_or_else(|| crate::playlist::pick_diff(set, crate::playlist::policy_of(target)).sha2.clone());
    let (path, manifest) = match &set.root {
        // stable:难度 .osu = sha2 携带的文件名,直接落目录
        Some(root) => (PathBuf::from(root).join(&sha2), None),
        None => {
            let root = lazer::data_root().ok_or("未找到 osu!lazer 数据目录")?;
            let osu_path = lazer::beatmap_blob(&root, &sha2);
            if !osu_path.is_file() {
                return Err(format!(
                    "难度 blob 缺失(曲库可能已更新):{}",
                    osu_path.display()
                ));
            }
            let files: Vec<crate::ipc::VFile> = set
                .files
                .iter()
                .map(|f| crate::ipc::VFile {
                    name: f.filename.clone(),
                    path: root
                        .join("files")
                        .join(lazer::blob_relative_path(&f.hash))
                        .to_string_lossy()
                        .into_owned(),
                })
                .collect();
            (osu_path, Some(files))
        }
    };
    if path.is_file() {
        // 回写路径缓存:每个难度的 .osu 路径 + 集内共享的 manifest
        let osu_of = |sha2: &str| -> String {
            match &set.root {
                Some(root) => PathBuf::from(root).join(sha2).to_string_lossy().into_owned(),
                None => lazer::data_root()
                    .map(|r| lazer::beatmap_blob(&r, sha2).to_string_lossy().into_owned())
                    .unwrap_or_default(),
            }
        };
        crate::pathcache::put(
            app,
            &track.set_id,
            crate::pathcache::CachedSet {
                diffs: set
                    .beatmaps
                    .iter()
                    .map(|b| crate::pathcache::CachedDiff {
                        sha2: b.sha2.clone(),
                        name: b.name.clone(),
                        star: b.star_rating,
                        length_ms: b.length_ms,
                        osu_path: osu_of(&b.sha2),
                    })
                    .collect(),
                manifest: manifest.clone(),
            },
        );
    }
    let title = if set.title_unicode.is_empty() { set.title.clone() } else { set.title_unicode.clone() };
    let artist = if set.artist_unicode.is_empty() { set.artist.clone() } else { set.artist_unicode.clone() };
    let length_ms = set.beatmaps.iter().find(|b| b.sha2 == sha2).map(|b| b.length_ms).unwrap_or(0.0);
    dispatch_load(
        app,
        track,
        path.to_string_lossy().into_owned(),
        manifest,
        sha2,
        title,
        artist,
        length_ms,
        start,
    )
}

/// 发命令给壁纸进程(必要时先拉起)。
pub fn send_cmd(app: &AppHandle, cmd: Command) -> Result<(), String> {
    ensure_child(app)?;
    let state = app.state::<WallState>();
    let ctl = state.ctl.lock().unwrap();
    match ctl.as_ref() {
        Some(c) => c.send(&cmd),
        None => Err("壁纸进程未运行".into()),
    }
}

/// 按已保存的设置重新应用壁纸(应用启动时 / 重启壁纸进程后)。
/// 只恢复持久化的播放列表与上次曲目;列表为空时不自动从曲库填充
/// (列表内容由用户显式添加)。曲库曲目优先于手动文件。
///
/// **零数据库优先**:播放列表恢复用自带快照(不解析),上次曲目经
/// 路径缓存直接下发(load_track 快路径)——缓存未命中时 load_track
/// 内部才解析数据库。prune(清理已删地图/跨源残留)延迟到数据库
/// 真正解析时(`library` 内)执行。
pub fn reload_saved(app: &AppHandle) -> Result<(), String> {
    let s = app.state::<WallState>().settings.lock().unwrap().clone();
    if s.lazer_set.is_some() || !s.playlist.is_empty() {
        // 恢复持久化列表(显示快照自带,不碰数据库),上次曲目把播放头
        // 对齐到列表内条目(不在列表 = 临时播放,对齐是空操作)
        {
            let state = app.state::<WallState>();
            let mut playlist = state.playlist.lock().unwrap();
            if !s.playlist.is_empty() {
                playlist.restore(&s.playlist, s.playlist_pos, s.play_mode);
                if s.play_mode == PlayMode::Random {
                    playlist.set_mode(PlayMode::Random);
                }
            }
            if let Some(set_id) = s.lazer_set.clone() {
                playlist.align_current(set_id, s.lazer_sha2.clone());
            }
        }
        // 上次曲目 → 列表当前位:load_track 快路径命中即完全不读数据库
        let current = app.state::<WallState>().playlist.lock().unwrap().current();
        let play = current.or_else(|| {
            s.lazer_set.clone().map(|set_id| Track {
                set_id,
                sha2: s.lazer_sha2.clone(),
                qid: 0,
                ..Default::default()
            })
        });
        if let Some(track) = play {
            if load_track(app, &track).is_ok() {
                return Ok(());
            }
        }
        // 首选失败(如地图已删:load_track 慢路径已解析数据库并 prune
        // 了列表)→ 重试一次新的列表当前位
        let retry = app.state::<WallState>().playlist.lock().unwrap().current();
        if let Some(track) = retry {
            if load_track(app, &track).is_ok() {
                return Ok(());
            }
        }
        // 上次曲目在当前源已不存在且列表空了:清掉残留,走手动文件回退
        let state = app.state::<WallState>();
        let mut st = state.settings.lock().unwrap();
        st.lazer_set = None;
        st.lazer_sha2 = None;
        crate::settings::save(app, &st);
    }
    if let Some(path) = s.path {
        app.state::<WallState>().from_library.store(false, Ordering::Relaxed);
        let stored = app.state::<WallState>().settings.lock().unwrap().skin.clone();
        let skin = resolve_skin_dir(app, &stored).map(|p| p.to_string_lossy().into_owned());
        send_cmd(
            app,
            Command::Load {
                path,
                diff: s.diff,
                fail: s.fail,
                speed: s.speed,
                start: 0.0,
                loop_playback: s.loop_playback,
                // 手动文件点播无 mods 概念
                mods: 0,
                manifest: None,
                skin,
                force_colours: s.force_skin_colours,
                hidden: s.hidden,
                storyboard: s.storyboard,
                video: s.storyboard,
                beatmap_hitsounds: s.beatmap_hitsounds,
                upscale: s.upscale.clone(),
            },
        )?;
    }
    Ok(())
}

/// 杀掉壁纸进程并按保存的设置重启(桌面层重挂 / 排障用)。
pub fn restart(app: &AppHandle) -> Result<(), String> {
    {
        let state = app.state::<WallState>();
        if let Some(c) = state.ctl.lock().unwrap().as_ref() {
            let _ = c.send(&Command::Quit);
        }
    }
    std::thread::sleep(Duration::from_millis(150));
    {
        let state = app.state::<WallState>();
        if let Some(mut old) = state.ctl.lock().unwrap().take() {
            for _ in 0..10 {
                if !old.alive() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = old.child.kill();
            let _ = old.child.wait();
        }
    }
    *app.state::<WallState>().last_event.lock().unwrap() = None;
    reload_saved(app)
}

/// 当前播放中(供暂停/继续切换判断)。
pub fn is_playing(app: &AppHandle) -> bool {
    matches!(
        &*app.state::<WallState>().last_event.lock().unwrap(),
        Some(Event::Status { playing: true, .. })
    )
}
