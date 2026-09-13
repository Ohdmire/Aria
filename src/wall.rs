//! `--wallpaper` 子进程:壁纸渲染端(osu! autoplay 游玩画面)。
//!
//! stdin 按行读 [`Command`],把 osu-replay-render 的完整场景(自动游玩
//! 光标 + 判定动画 + HUD + storyboard + 背景,全部 wgpu)渲到附加在桌面
//! WorkerW 上的 winit 窗口;stdout 按行回 [`Event`],日志走 stderr。
//!
//! 画面比例:场景内部按 16:9 渲染(尺寸贴合桌面、长边封顶 2560),
//! `SurfaceRenderer` 的 blit 自动把 16:9 内容 letterbox 到任意桌面比例
//! (2K 16:9 屏正好 1:1 铺满)。音频(rodio)的播放位置是主时钟,
//! 暂停/seek/倍速画面与声音天然同步;stdin EOF 即退出信号。

use crate::audio::AudioOut;
use crate::ipc::{Command, Event, VFile};
use crate::loader::{Library, LoadedWall};
use crate::win::{self, WallpaperHost};
use osu_replay_render::draw::{Atlas, DrawList};
use osu_replay_render::scene::{Assets, SceneState};
use osu_replay_render::storyboard::{self, Assets as SbAssets, StoryboardLayer};
use osu_replay_render::surface::SurfaceRenderer;
use osu_replay_render::{build_atlas, game, skin, CLEAR, Fonts, StoryboardSlots};
use std::collections::HashMap;
use std::io::{BufRead, BufWriter, Write};
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowAttributes};

/// 壁纸窗口被外部销毁后,重挂桌面的等待间隔(给 explorer 重启留时间)。
const REATTACH_DELAY: Duration = Duration::from_secs(3);
/// 纯净模式/渲染暂停(无窗口)下的播放推进周期:~60Hz 足够音效对齐与人眼进度条。
const PURE_TICK: Duration = Duration::from_millis(15);
/// 自动暂停迟滞:桌面连续被遮挡多久才暂停渲染(防全屏切换瞬态抖动)。
const COVER_HOLD: Duration = Duration::from_millis(1000);
/// 桌面重新可见多久才恢复渲染(重建会话有成本,避免闪烁)。
const UNCOVER_HOLD: Duration = Duration::from_millis(500);
/// 场景内部分辨率长边封顶(4K 桌面降载;2K 屏原生)。
const SCENE_MAX_LONG: u32 = 2560;

/// 渲染模式(设置 render_mode 的运行期形态)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderMode {
    /// 始终渲染。
    Always,
    /// 全屏遮挡时不渲染画面(音乐照播,会话拆除省电)。
    AutoPause,
    /// 全屏遮挡时暂停播放(壁纸会话保留,画面冻结在最后一帧)。
    FsPause,
    /// 全屏遮挡时暂停播放并拆除渲染会话(释放内存;恢复时重建续播)。
    FsSleep,
    /// 播放器模式:恒不渲染,只播音乐 + 音效。
    Off,
}

impl RenderMode {
    fn parse(s: &str) -> RenderMode {
        match s {
            "always" => RenderMode::Always,
            "fs_pause" => RenderMode::FsPause,
            "fs_sleep" => RenderMode::FsSleep,
            "off" => RenderMode::Off,
            _ => RenderMode::AutoPause,
        }
    }
    /// 播放器模式(恒不渲染)。
    fn pure(self) -> bool {
        self == RenderMode::Off
    }
    /// 遮挡时是否拆除渲染会话。
    fn drops_render_on_cover(self) -> bool {
        matches!(self, RenderMode::AutoPause | RenderMode::FsSleep)
    }
    /// 遮挡时是否暂停播放(音乐 + 音效一起停)。
    fn pauses_playback_on_cover(self) -> bool {
        matches!(self, RenderMode::FsPause | RenderMode::FsSleep)
    }
}

/// 路径取文件名(日志用)。
fn basename_of(p: &str) -> &str {
    p.rsplit(['\\', '/']).next().unwrap_or(p)
}

pub fn main() -> i32 {
    let _ = crate::logging::init();
    log::info!("壁纸渲染子进程启动 (pid {})", std::process::id());

    let event_loop = match EventLoop::with_user_event().build() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("创建事件循环失败: {e}");
            return 1;
        }
    };
    let proxy = event_loop.create_proxy();
    spawn_stdin_reader(proxy.clone());

    // 清理上次会话残留的预处理临时文件(崩溃/断电未及删除的)
    if let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) {
        for e in rd.flatten() {
            let name = e.file_name();
            if name.to_string_lossy().starts_with("aria-bgm-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let out = Arc::new(Out(Mutex::new(BufWriter::new(std::io::stdout()))));
    let audio = AudioOut::new(); // 无设备则 None,静音播放
    let mut app = WallApp::new(out, proxy, audio);
    match event_loop.run_app(&mut app) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("事件循环失败: {e}");
            1
        }
    }
}

/// stdout 按行写 JSON 事件(多线程共享)。
struct Out(Mutex<BufWriter<std::io::Stdout>>);

impl Out {
    fn send(&self, ev: &Event) {
        if let Ok(mut w) = self.0.lock() {
            if let Ok(s) = serde_json::to_string(ev) {
                let _ = writeln!(w, "{s}");
                let _ = w.flush();
            }
        }
    }
}

/// 后台线程:读 stdin 命令行 → 转发进事件循环;EOF 时请求退出。
/// 整曲 SoundTouch tempo 预处理:解码(kira/symphonia 全格式)→ 变速
/// 不变调 → 16bit PCM WAV 临时文件。一首 4 分钟歌 ≈ 1–2 秒。
fn render_tempo_wav(src: &Path, dst: &Path, speed: f64) -> Result<(), String> {
    let data = kira::sound::static_sound::StaticSoundData::from_file(src)
        .map_err(|e| format!("解码失败: {e:?}"))?;
    let mut interleaved = Vec::with_capacity(data.frames.len() * 2);
    for f in data.frames.iter() {
        interleaved.push(f.left);
        interleaved.push(f.right);
    }
    let stretched = crate::soundtouch::stretch_tempo(&interleaved, data.sample_rate, speed);
    write_wav_pcm16(dst, &stretched, data.sample_rate).map_err(|e| format!("写 WAV 失败: {e}"))
}

/// 极简 WAV 写出(16bit PCM 立体声,44 字节头)——避免为写侧引入编码库。
fn write_wav_pcm16(path: &Path, samples: &[f32], sample_rate: u32) -> std::io::Result<()> {
    use std::io::Write;
    let frames = samples.len() / 2;
    let data_len = (frames * 4) as u32;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?; // fmt 块长
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&2u16.to_le_bytes())?; // 立体声
    f.write_all(&sample_rate.to_le_bytes())?;
    f.write_all(&(sample_rate * 4).to_le_bytes())?; // 字节率
    f.write_all(&4u16.to_le_bytes())?; // 块对齐
    f.write_all(&16u16.to_le_bytes())?; // 位深
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    let mut bytes = Vec::with_capacity(data_len as usize);
    for s in samples {
        bytes.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    f.write_all(&bytes)?;
    Ok(())
}

fn spawn_stdin_reader(proxy: EventLoopProxy<Command>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Command>(line) {
                Ok(cmd) => {
                    if proxy.send_event(cmd).is_err() {
                        return;
                    }
                }
                Err(e) => log::warn!("无法解析命令: {e}"),
            }
        }
        // EOF:父进程退出或关闭了管道
        let _ = proxy.send_event(Command::Quit);
    });
}

/// ---- osu! legacy mod 位(壁纸可选子集;数值 = stable 官方位,
/// 与前端 main.js 的 MOD_BITS 一致。HR=1<<4 / EZ=1<<1 仅由渲染端按位
/// 解释,本文件不单列)----
const MOD_HD: u32 = 1 << 3;
const MOD_DT: u32 = 1 << 6;
const MOD_HT: u32 = 1 << 8;
const MOD_NC: u32 = 1 << 9;

/// 曲目速率 mod → 游戏倍速(DT/NC 1.5,HT 0.75,无 1.0)。
fn mods_rate(mods: u32) -> f32 {
    if mods & (MOD_DT | MOD_NC) != 0 {
        1.5
    } else if mods & MOD_HT != 0 {
        0.75
    } else {
        1.0
    }
}

#[derive(Clone)]
struct LoadParams {
    path: String,
    diff: Option<String>,
    speed: f32,
    start: f32,
    loop_playback: bool,
    /// lazer 谱面集虚拟文件表(零拷贝;None = 普通路径流)。
    manifest: Option<Vec<VFile>>,
    /// 皮肤目录(None = 内置 Argon)。
    skin: Option<String>,
    /// 皮肤 combo 颜色强制覆盖谱面 [Colours]。
    force_colours: bool,
    /// HD(Hidden)视觉,加载期生效。
    hidden: bool,
    /// osu! legacy mod 位(HD/HR/EZ/DT/HT/NC;见 [`MOD_*`] 常量)。
    mods: u32,
    /// storyboard / 视频层渲染开关(关 = 不解析不渲染)。
    storyboard: bool,
    video: bool,
    /// 谱面自带音效(lazer "Beatmap hitsounds",默认开):谱面集内的
    /// 采样文件按槽位优先于皮肤。加载期生效。
    beatmap_hitsounds: bool,
}

/// 一次加载的谱面来源:普通路径(.osz 解包 / stable 目录)或 lazer 虚拟
/// 文件表(零拷贝)。保留在会话里 —— 渲染暂停后恢复时直接重建,无需
/// 重读任何文件、更不打断音频。
#[derive(Clone)]
enum MapSource {
    Path { map_path: PathBuf },
    Virtual { text: String, files: VirtualFiles },
}

/// 一次构建好的完整渲染会话:storyboard 解析、图集、字体、场景、GPU
/// 资源(含 SurfaceRenderer)。**全部就绪后音频 + 音效 + 画面一起起播**,
/// 不会出现"BGM 先响、音效迟半拍"。
struct RenderSession {
    surf: SurfaceRenderer,
    sb_layer: Option<StoryboardLayer>,
    sb_samples: Vec<(f32, f32, kira::sound::static_sound::StaticSoundData)>,
    atlas: Atlas,
    fonts: Fonts,
    scene: SceneState,
    /// 场景内部分辨率。
    scene_size: (u32, u32),
}


/// lazer 谱面集的虚拟文件表:文件名(小写、`/` 分隔)→ blob 实际路径。
/// 点播时父进程把 realm 解析出的 文件名 → files/ blob 映射直接下发,
/// 渲染端按名解析(.osu / 音频 / 背景 / storyboard 素材 / 视频全程直读
/// blob,不复制)。
#[derive(Clone)]
struct VirtualFiles {
    by_name: HashMap<String, PathBuf>,
}

impl VirtualFiles {
    fn new(files: &[VFile]) -> VirtualFiles {
        let mut by_name = HashMap::with_capacity(files.len());
        for f in files {
            let norm = f.name.trim().trim_matches('"').replace('\\', "/").to_lowercase();
            by_name
                .entry(norm)
                .or_insert_with(|| PathBuf::from(&f.path));
        }
        VirtualFiles { by_name }
    }

    /// 相对文件名 → 实际路径。全名精确匹配(大小写/分隔符归一)优先,
    /// 回退仅文件名匹配(谱面声明带子目录前缀而文件表为平铺名时)。
    fn resolve(&self, name: &str) -> Option<PathBuf> {
        let norm = name.trim().trim_matches('"').replace('\\', "/").to_lowercase();
        if let Some(p) = self.by_name.get(&norm) {
            return Some(p.clone());
        }
        let base = norm.rsplit('/').next()?.to_string();
        self.by_name.get(&base).cloned()
    }

    /// 谱组共享 .osb(文件表内扩展名匹配,文件名稳定序第一个)的内容。
    fn osb_text(&self) -> Option<String> {
        let mut names: Vec<String> =
            self.by_name.keys().filter(|k| k.ends_with(".osb")).cloned().collect();
        names.sort();
        let first = names.first()?;
        std::fs::read_to_string(self.by_name.get(first)?).ok()
    }
}

/// 播放时钟:墙钟积分(平滑、逐帧递增,匹配屏幕刷新率)+ 音频位置锚定
/// (±80ms 限幅校正,消除解码位置的量化抖动)。音频缺位(暂停/播完/
/// 无设备)时自由计时。
///
/// **seek 在途保护**:kira 的 `seek_to` 是异步的,落地前 `position()` 仍报
/// 旧位置 —— 直接锚定会把时钟以 ±80ms/帧 的速度拽回旧位置,倒退 seek
/// 表现为时钟短暂前冲、密集补播打击音效。因此在途(`seek_target` 存在)
/// 时不追陈旧锚点,墙钟自由走;音频位置落到目标附近才硬对齐。
///
/// **阻塞恢复**:事件循环被同步加载阻塞数秒(巨型 storyboard 重建等)
/// 期间音频照播 —— 恢复后墙钟积分会把时间自然补回,与音频误差可能很小,
/// 但播放头停在了阻塞前。两类检测都置 `jumped` 标记:tick 间隔超过
/// `STALL_JUMP`(阻塞必现),或与音频误差超过 `ANCHOR_GATE`;调用方据此
/// 把打击音效播放头跳到当前时刻(期间事件不补播),任何"阻塞→追赶→
/// 密集爆发"的路径都被消除。
#[derive(Clone, Copy)]
struct Clock {
    t: f32,
    playing: bool,
    speed: f32,
    wall_t: f32,
    wall_at: Option<Instant>,
    /// seek 在途的目标位置(ms);音频位置接近它 = 落地,硬对齐并清除。
    seek_target: Option<f32>,
    /// 时钟发生硬跳(seek 落地 / 阻塞恢复对齐);调用方消费后应重置
    /// 打击音效播放头,避免跨段补播。
    jumped: bool,
}

/// 正常锚定门限:误差在其内做 ±80ms 限幅校正;超出视为 seek 在途的
/// 陈旧位置或阻塞后的大偏差,走硬对齐路径。
const ANCHOR_GATE: f32 = 250.0;
/// 墙钟积分的"长阻塞"门限:两个 tick 间隔超过它 = 事件循环被同步加载
/// 阻塞了(storyboard 重建等)。阻塞期间墙钟积分会把时间自然"补"回来,
/// 与音频位置误差依旧很小 —— 但打击音效播放头停在阻塞前,若不标记跳变
/// 会把期间的事件一口气全补播(全屏返回桌面的"爆发"根因)。
const STALL_JUMP: f32 = 500.0;

impl Clock {
    fn new(t: f32, speed: f32) -> Clock {
        Clock { t, playing: true, speed, wall_t: t, wall_at: None, seek_target: None, jumped: false }
    }

    fn step(&mut self, audio_pos: Option<f64>) {
        let now = Instant::now();
        if let Some(prev) = self.wall_at.replace(now) {
            let dt = now.duration_since(prev).as_secs_f32() * 1000.0;
            if self.playing {
                self.wall_t += dt * self.speed;
                // 长阻塞恢复:播放头将由调用方跳到当前时刻,期间事件不补播
                if dt > STALL_JUMP {
                    self.jumped = true;
                }
            }
        }
        if self.playing {
            if let Some(pos) = audio_pos {
                let err = pos as f32 - self.wall_t;
                if err.abs() <= ANCHOR_GATE {
                    // 常规锚定:小误差限幅校正
                    self.wall_t += err.clamp(-80.0, 80.0);
                    self.seek_target = None;
                } else if let Some(target) = self.seek_target {
                    // seek 在途:音频位置已接近目标 = 落地,硬对齐
                    //(不管落地多晚,一次对齐消除全部延迟偏差);
                    // 否则(仍报旧位置)忽略锚点,墙钟自由走
                    if (pos as f32 - target).abs() <= ANCHOR_GATE {
                        self.wall_t = pos as f32;
                        self.seek_target = None;
                        self.jumped = true;
                    }
                } else {
                    // 无 seek 在途的大偏差:事件循环阻塞后音频先行了 ——
                    // 音频是主时钟,直接硬对齐并置跳变标记(旧行为的
                    // ±80ms/帧 追赶会把跨越的事件密集补播,听感即
                    // "阻塞后爆发",全屏返回桌面重建会话正是此路径)
                    self.wall_t = pos as f32;
                    self.jumped = true;
                }
            }
        } else {
            self.wall_t = self.t; // 暂停时冻结
        }
        self.t = self.wall_t;
    }

    fn seek(&mut self, ms: f32) {
        self.t = ms;
        self.wall_t = ms;
        self.wall_at = None;
        self.seek_target = Some(ms);
    }

    /// 消费"时钟硬跳"标记(seek 落地 / 阻塞恢复对齐);调用方据此重置
    /// 打击音效播放头,跳过期间事件避免补播爆发。
    fn take_jump(&mut self) -> bool {
        std::mem::take(&mut self.jumped)
    }
}

#[cfg(test)]
mod clock_tests {
    use super::*;

    /// 倒退 seek 期间音频仍报旧(较大)位置:时钟不得前冲 —— 否则播放头
    /// 会跨过大量事件,打击音效密集补播。
    #[test]
    fn backward_seek_no_forward_spike_on_stale_audio() {
        let mut c = Clock::new(0.0, 1.0);
        // 推进到 60s(音频同步)
        for _ in 0..5 {
            c.step(Some(60_000.0));
        }
        // 倒退 seek 到 30s;kira 在途,音频继续报 ~60s
        c.seek(30_000.0);
        for _ in 0..60 {
            c.step(Some(59_999.0));
        }
        assert!(
            c.t < 32_000.0,
            "倒退 seek 在途时钟前冲(密集补播的根源): t={}ms",
            c.t
        );
        // 音频落到 30s 附近 → 硬对齐
        c.step(Some(30_050.0));
        assert!((c.t - 30_050.0).abs() < 1.0, "落地后应硬对齐: t={}ms", c.t);
        assert!(c.seek_target.is_none(), "落地后清除在途标记");
    }

    /// 前进 seek:同样不在途追旧(较小)位置,落地对齐。
    #[test]
    fn forward_seek_waits_for_landing() {
        let mut c = Clock::new(0.0, 1.0);
        for _ in 0..5 {
            c.step(Some(10_000.0));
        }
        c.seek(60_000.0);
        for _ in 0..60 {
            c.step(Some(10_001.0)); // 在途仍报 ~10s
        }
        assert!(c.t > 58_000.0 && c.t < 62_500.0, "前进 seek 时钟应留在目标附近: t={}ms", c.t);
        c.step(Some(60_030.0));
        assert!((c.t - 60_030.0).abs() < 1.0, "落地后应对齐音频: t={}ms", c.t);
    }

    /// 事件循环被同步加载阻塞数秒后恢复(全屏返回桌面重建会话):墙钟积分
    /// 会把时间补回、与音频误差很小(不会触发硬对齐),但播放头停在阻塞前
    /// —— 必须由"tick 间隔超限"置跳变标记,否则期间事件一口气全补播。
    #[test]
    fn stalled_loop_marks_jump_even_when_audio_matches() {
        let mut c = Clock::new(10_000.0, 1.0);
        c.wall_at = Some(Instant::now() - Duration::from_millis(3000)); // 上一 tick 在 3s 前
        c.step(Some(13_000.0)); // 音频期间照播 3s
        assert!((c.t - 13_000.0).abs() < 260.0, "时间应与音频对齐: t={}ms", c.t);
        assert!(c.take_jump(), "长阻塞恢复必须置跳变标记(播放头重置依据)");
        // 正常节奏(tick ~15ms)不置标记
        c.step(Some(13_015.0));
        assert!(!c.take_jump(), "正常 tick 不应置跳变");
    }

    /// 无 seek 在途的大偏差 = 事件循环阻塞后音频先行(全屏返回桌面触发
    /// 渲染会话同步重建正是此路径):应**硬对齐**到音频并置跳变标记,
    /// 调用方据此重置播放头 —— 旧的 ±80ms/帧 追赶会密集补播("阻塞爆发")。
    #[test]
    fn large_error_without_seek_snaps_and_marks_jump() {
        let mut c = Clock::new(0.0, 1.0);
        c.seek_target = None;
        c.wall_t = 10_000.0;
        assert!(!c.take_jump(), "初始无跳变");
        c.step(Some(60_000.0)); // 阻塞 50s,音频已先行
        assert!((c.t - 60_000.0).abs() < 1.0, "应硬对齐音频主时钟: t={}ms", c.t);
        assert!(c.take_jump(), "硬对齐应置跳变标记(播放头重置依据)");
        assert!(!c.take_jump(), "标记消费一次即清");
    }
}

/// 16:9 场景内部分辨率:贴合桌面(不超出),长边封顶 2560(4K 桌面降载;
/// 2K 16:9 屏正好原生)。blit 再 letterbox 到任意桌面比例。
fn scene_size(desk_w: u32, desk_h: u32) -> (u32, u32) {
    let mut h = desk_h.min(desk_w * 9 / 16).max(1);
    let mut w = h * 16 / 9;
    if w > SCENE_MAX_LONG {
        w = SCENE_MAX_LONG;
        h = w * 9 / 16;
    }
    (w & !1, h & !1)
}

struct WallApp {
    out: Arc<Out>,
    proxy: EventLoopProxy<Command>,
    library: Library,
    last_load: Option<LoadParams>,
    /// 当前曲目的音频文件(循环重播用)。
    audio_path: Option<PathBuf>,
    /// None = 本机无音频设备,静音播放。
    audio: Option<AudioOut>,
    /// 打击音效:事件表(按时间排序)+ 采样预载 + 播放头游标。
    hs_events: Vec<osu_replay_render::hitsound::HitsoundEvent>,
    hs_sounds: HashMap<osu_replay_render::hitsound::SampleSlot, kira::sound::static_sound::StaticSoundData>,
    /// SB 声音采样(lazer StoryboardSampleInfo):按播放头跨过触发。
    sb_samples: Vec<(f32, f32, kira::sound::static_sound::StaticSoundData)>,
    sb_sample_cursor: usize,
    /// 在播的 SB 采样句柄(seek/切歌时统一停止,避免语音拖尾)。
    sb_sample_handles: Vec<kira::sound::static_sound::StaticSoundHandle>,
    hs_cursor: usize,
    /// 循环音事件(滑条滑动/转盘旋转)与活动播放句柄 (事件下标, run 下标, 句柄)。
    loop_events: Vec<osu_replay_render::hitsound::LoopSoundEvent>,
    loop_playbacks: Vec<(usize, usize, kira::sound::static_sound::StaticSoundHandle)>,
    /// 本次会话的打击音效槽位表(含解析失败的槽位;热切换时按它整表
    /// 重解析 —— 仅看 hs_sounds 的 key 会漏掉"权威静音"的槽位)。
    hs_slots: Vec<osu_replay_render::hitsound::SampleSlot>,
    /// 谱面自带音效开关(运行期可热切换,仅重建采样表)。
    beatmap_hitsounds: bool,
    /// HUD(计分板等)开关,壁纸默认关闭。
    hud_visible: bool,
    /// PP 计数器开关(HUD 的子项;PP 时间线在加载期随 HUD 一起计算)。
    pp_display: bool,
    /// osu! 打击动画(true = 完整动画,默认;false = 减少模式:命中
    /// 圆圈 60ms 整体淡出)。实时生效(纯渲染旗标)。
    hit_animations: bool,
    /// 休息段背景变亮(break 期间背景图暗度 −0.3,800ms 淡变)。
    break_lighten: bool,
    /// 光标渲染开关(关 = 光标与拖尾都不画)。
    cursor_on: bool,
    /// 光标大小倍率(0.1–2.0)。
    cursor_size: f32,
    /// 隐藏游玩画面模式(只渲染背景 + storyboard;音频/判定照常)。
    gameplay_hidden: bool,
    /// 背景亮度(0.0–1.0;谱面背景存在时生效,实时可调)。
    bg_opacity: f32,
    /// storyboard / 视频层渲染开关(当前会话;由 Load 参数带入)。
    sb_enabled: bool,
    video_enabled: bool,
    /// BGM 淡入淡出(只作用于 BGM;换曲淡出旧曲、起播/重挂淡入)。
    fade_audio: bool,
    /// HD(Hidden)视觉(实时;场景渲染旗标)= 用户开关 | 曲目 HD mod。
    hd_on: bool,
    /// 曲目自带 HD mod 位(加载期;与用户 HD 开关取或得到 hd_on)。
    track_hd: bool,
    /// 用户倍速(播放条变速按钮;实际速度 = track_rate × user_speed)。
    user_speed: f32,
    /// 曲目速率 mod(DT/NC=1.5,HT=0.75,无=1.0)。
    track_rate: f32,
    /// 变速时是否保持音调(Nightcore 为 false = 升调)。
    pitch_preserve: bool,
    /// SoundTouch tempo 预处理任务序号(后台线程据此丢弃过期结果)。
    stretch_job: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// 当前在播的 tempo 预处理临时文件(换文件/退出时删除)。
    last_stretch: Option<PathBuf>,
    /// 用户手动暂停(与遮挡自动暂停区分):遮挡恢复/渲染模式切换等
    /// 一切自动恢复路径都不得越过它续播。
    user_paused: bool,
    /// 加载起播在等预处理完成:时钟冻结(画面停在起始帧、音效不
    /// 触发),AudioReady 到达后音乐+音效+画面同帧开始。变速路径
    /// (SetSpeed)不置此标志——旧 BGM 在播,时钟照常锚定。
    audio_pending: bool,
    /// 采样解码缓存((皮肤/谱面身份, 槽位) → wav 字节;None = 该槽位
    /// 静音)。解析链含谱面文件层,同谱面(重播/重载)零重解;换谱面按
    /// 需重解 —— mp3/ogg 走进程内 symphonia,每槽位毫秒级。
    sample_cache: HashMap<(String, String), Option<Vec<u8>>>,
    /// 进度条/状态上报的曲目时长(ms):优先 BGM 音频真实时长(谱面最后
    /// 物件时间通常比歌短一截),探测失败回退物件时长。
    track_duration_ms: f32,
    /// 音效偏移(ms):正值 = 提前,负值 = 延后(相对 BGM)。
    audio_offset_ms: f32,
    /// BGM 时长探测缓存((路径, 大小) → ms;None = 探测失败)。
    dur_cache: HashMap<(PathBuf, u64), Option<f64>>,
    /// 渲染模式(运行期单一真相;派生 pure/遮挡行为)。
    render_mode: RenderMode,
    /// 遮挡触发的播放暂停(FsPause/FsSleep):可见恢复时自动续播;
    /// 手动暂停/恢复不受此标记驱动。
    fs_paused: bool,
    /// 手动指定的 ffmpeg/ffprobe 完整路径(None = PATH 查找):
    /// 视频层解码与视频探测 / BGM 时长用,下次载入生效。
    ffmpeg_bin: Option<PathBuf>,
    ffprobe_bin: Option<PathBuf>,
    /// 最近一次遮挡检测结果(含迟滞确认时间)。
    covered: bool,
    covered_at: Instant,
    /// 壁纸窗口 HWND(遮挡检测用;渲染暂停、窗口销毁后仍保留,
    /// 新窗口创建时更新)。
    wall_hwnd: Option<windows::Win32::Foundation::HWND>,
    /// 当前曲目的谱面来源(渲染会话重建用;暂停恢复零重读)。
    source: Option<MapSource>,
    /// 帧率上限(0 = 跟随屏幕刷新率,不节流)。
    fps: u32,
    frame_at: Option<Instant>,
    /// 实际渲染帧率统计(每秒结算一次)。
    frame_count: u32,
    measured_fps: u32,
    fps_measure_at: Instant,
    // 壁纸窗口(空 = 桌面空闲);GPU 会话在 Load 时按需构建
    window: Option<Arc<Window>>,
    host: Option<WallpaperHost>,
    desk: (u32, u32),
    // autoplay 会话(Arc:构建期间与字段借用解耦)
    game: Option<Arc<game::GameData>>,
    surf: Option<SurfaceRenderer>,
    atlas: Option<Atlas>,
    fonts: Option<Fonts>,
    skin: Option<skin::ResolvedSkin>,
    sb_layer: Option<StoryboardLayer>,
    scene: Option<SceneState>,
    scene_size: (u32, u32),
    list: DrawList,
    clock: Clock,
    looping: bool,
    ended_sent: bool,
    poll_at: Instant,
    status_at: Instant,
}

impl WallApp {
    fn new(out: Arc<Out>, proxy: EventLoopProxy<Command>, audio: Option<AudioOut>) -> WallApp {
        WallApp {
            out,
            proxy,
            library: Library::new(),
            last_load: None,
            audio_path: None,
            hs_events: Vec::new(),
            hs_sounds: HashMap::new(),
            sb_samples: Vec::new(),
            sb_sample_cursor: 0,
            sb_sample_handles: Vec::new(),
            hs_cursor: 0,
            loop_events: Vec::new(),
            loop_playbacks: Vec::new(),
            hs_slots: Vec::new(),
            beatmap_hitsounds: true,
            fps: 0,
            frame_at: None,
            frame_count: 0,
            measured_fps: 0,
            fps_measure_at: Instant::now(),
            audio,
            window: None,
            host: None,
            desk: (0, 0),
            surf: None,
            game: None,
            atlas: None,
            fonts: None,
            skin: None,
            sb_layer: None,
            scene: None,
            scene_size: (1920, 1080),
            list: DrawList::new(),
            hud_visible: false,
            pp_display: false,
            hit_animations: true,
            break_lighten: false, // 壁纸特设:与上游默认相反
            cursor_on: true,
            cursor_size: 1.0,
            gameplay_hidden: false,
            bg_opacity: 0.3,
            sb_enabled: true,
            video_enabled: true,
            fade_audio: true,
            hd_on: false,
            track_hd: false,
            user_speed: 1.0,
            track_rate: 1.0,
            pitch_preserve: true,
            stretch_job: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_stretch: None,
            user_paused: false,
            audio_pending: false,
            sample_cache: HashMap::new(),
            track_duration_ms: 0.0,
            audio_offset_ms: 0.0,
            dur_cache: HashMap::new(),
            render_mode: RenderMode::AutoPause,
            fs_paused: false,
            ffmpeg_bin: None,
            ffprobe_bin: None,
            covered: false,
            covered_at: Instant::now(),
            wall_hwnd: None,
            source: None,
            clock: Clock { t: 0.0, playing: false, speed: 1.0, wall_t: 0.0, wall_at: None, seek_target: None, jumped: false },
            looping: true,
            ended_sent: false,
            poll_at: Instant::now(),
            status_at: Instant::now(),
        }
    }

    /// 曲目时长(引擎最后快照 + 1s 收尾)。
    fn limit(&self) -> f32 {
        self.game
            .as_ref()
            .and_then(|g| g.snapshots.last().map(|s| s.time as f32 + 1000.0))
            .unwrap_or(f32::MAX)
    }

    fn apply_load(&mut self, event_loop: &ActiveEventLoop, p: LoadParams) {
        let mut p = p;
        p.start = p.start.max(0.0);
        self.user_paused = false; // 换曲/重载 = 新的播放意图
        self.last_load = Some(p.clone());
        // 淡出旧曲 BGM(只动 BGM,音效不参与;与下方载入耗时重叠,
        // 换曲/重载过渡无硬切)
        if self.fade_audio {
            if let Some(out) = &mut self.audio {
                out.fade_out(800.0);
            }
        }

        // ---- 谱面来源:虚拟文件表(lazer 零拷贝,直读 blob)或普通路径 ----
        // 加载路径上的任何 panic 都转成 Error 事件,避免壁纸进程整个崩溃;
        // 失败保留旧会话,不出现"点了没反应"。
        let source = if let Some(manifest) = p.manifest.clone() {
            let files = VirtualFiles::new(&manifest);
            let map_path = PathBuf::from(&p.path);
            match std::fs::read_to_string(&map_path) {
                Ok(text) => MapSource::Virtual { text, files },
                Err(e) => {
                    self.out.send(&Event::Error {
                        message: format!("读取谱面失败 {}: {e}", map_path.display()),
                    });
                    return;
                }
            }
        } else {
            let loaded: anyhow::Result<LoadedWall> =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.library.load(&p.path, p.diff.as_deref())
                }))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("加载谱面时发生内部错误")));
            match loaded {
                Ok(l) => MapSource::Path { map_path: l.map_path },
                Err(e) => {
                    self.out.send(&Event::Error { message: format!("{e:#}") });
                    return;
                }
            }
        };

        // ---- autoplay 会话 ----
        let map_str = p.path.clone();
        log::info!("[load] 解析谱面: {}", basename_of(&map_str));
        // HUD 关闭时不算 PP/星级(rosu-pp 全程计算可观,壁纸 HUD 默认关)
        let with_pp = self.hud_visible;
        let game_result = match &source {
            MapSource::Path { map_path, .. } => {
                game::load_autoplay(&map_path.to_string_lossy(), p.mods, p.hidden, with_pp)
            }
            MapSource::Virtual { text, .. } => {
                game::load_autoplay_content(text, p.mods, p.hidden, with_pp)
            }
        };
        let mut game_data = match game_result {
            Ok(g) => g,
            Err(e) => {
                self.out.send(&Event::Error { message: format!("解析谱面失败: {e}") });
                return;
            }
        };
        log::info!("[load] 音频/背景/storyboard 解析");

        // 相对名 → 实际路径:虚拟表按名解析(大小写不敏感);普通路径流
        // 落谱面所在目录(NTFS 大小写不敏感)。
        let resolve = |name: &str| -> Option<PathBuf> {
            match &source {
                MapSource::Path { map_path, .. } => {
                    let cand = map_path
                        .parent()
                        .map(|d| d.join(name))
                        .unwrap_or_else(|| PathBuf::from(name));
                    cand.is_file().then_some(cand)
                }
                MapSource::Virtual { files, .. } => files.resolve(name),
            }
        };

        // 音频:谱面自己声明的 AudioFilename(虚拟流 = blob 路径,kira 按
        // 内容嗅探解码,与扩展名无关)
        self.audio_path = game_data
            .map_audio
            .as_ref()
            .and_then(|name| resolve(name))
            .filter(|cand| {
                if cand.is_file() {
                    true
                } else {
                    log::warn!("音频文件缺失: {}", cand.display());
                    false
                }
            });

        // 皮肤目录(lazer 挂载 / stable;None = 内置 Argon)。
        // 纯净模式同样加载:打击音效采样解析走皮肤。
        let resolved_skin = match skin::load_skin(p.skin.as_deref().map(std::path::Path::new)) {
            Ok(s) => s,
            Err(e) => {
                self.out.send(&Event::Error { message: format!("加载皮肤失败: {e}") });
                return;
            }
        };

        // 打击音效:事件表 + 循环音事件,采样预解码;播放头
        // 跨过事件即触发 —— 与画面共用同一时钟,天然无偏移。纯净模式与
        // 完整模式共用这一套。采样解析按 lazer 层级:谱面自带文件
        // (LegacyBeatmapSkin,开关控制)→ 皮肤 → 内置默认。
        let hs_events =
            osu_replay_render::hitsound::collect_events(&game_data, &game_data.sample_data);
        let loop_events =
            osu_replay_render::hitsound::collect_loop_events(&game_data, &game_data.sample_data);
        let beatmap_store: Option<Box<dyn osu_replay_render::hitsound::BeatmapSampleStore>> =
            if p.beatmap_hitsounds {
                match &source {
                    MapSource::Path { map_path } => map_path.parent().map(|dir| {
                        Box::new(osu_replay_render::hitsound::DirectorySampleStore::new(dir))
                            as Box<dyn osu_replay_render::hitsound::BeatmapSampleStore>
                    }),
                    MapSource::Virtual { files, .. } => Some(Box::new(VirtualSampleStore {
                        files: files.clone(),
                    })),
                }
            } else {
                None
            };
        let mut slots: Vec<osu_replay_render::hitsound::SampleSlot> = hs_events
            .iter()
            .map(|e| e.slot())
            .chain(loop_events.iter().map(|e| e.slot()))
            .collect();
        slots.sort_by_key(|s| format!("{s:?}"));
        slots.dedup();
        // 留存槽位表:谱面音效运行期热切换按它整表重解析(含解析失败的
        // 槽位,开关换层后结论可能不同)
        self.hs_slots = slots.clone();
        let mut hs_sounds = HashMap::new();
        for slot in slots {
            let bytes = {
                let mut cache = std::mem::take(&mut self.sample_cache);
                // 谱面身份入 key:采样解析现在依赖谱面文件(同槽位不同
                // 谱面的文件不同),跨谱面不能共用缓存项
                let r = resolve_slot_sample(
                    &mut cache,
                    p.skin.as_deref(),
                    p.path.as_str(),
                    &slot,
                    beatmap_store.as_deref(),
                    &resolved_skin,
                );
                self.sample_cache = cache;
                r
            };
            if let Some(bytes) = bytes {
                if let Ok(data) = kira::sound::static_sound::StaticSoundData::from_cursor(
                    std::io::Cursor::new(bytes),
                ) {
                    hs_sounds.insert(slot, data);
                }
            }
        }
        log::info!(
            "打击音效: {} 事件 / {} 循环音 / {} 采样",
            hs_events.len(),
            loop_events.len(),
            hs_sounds.len()
        );

        // ---- 渲染会话先就绪,然后一切同时起播 ----
        // storyboard 解析/图集/GPU 构建可达数秒("execute" 一类巨型 SB),
        // 全部完成之前**不起播音频** —— 音乐 + 打击音效 + 画面同帧开始,
        // 不会出现"BGM 先响、音效迟半拍"。构建失败降级纯音频继续播。
        game::apply_skin_combo_colours(&mut game_data, &resolved_skin, p.force_colours);
        self.clear_old_render();
        self.game = Some(Arc::new(game_data));
        self.skin = Some(resolved_skin);
        self.source = Some(source);
        self.beatmap_hitsounds = p.beatmap_hitsounds;
        self.sb_enabled = p.storyboard;
        self.video_enabled = p.video;
        // mods:HD 并入视觉开关;速率 mod 与用户倍速相乘得到会话速度;
        // NC 不做音调补偿(升调即 nightcore 听感)
        self.track_hd = p.mods & MOD_HD != 0;
        self.hd_on = p.hidden || self.track_hd;
        self.track_rate = mods_rate(p.mods);
        self.pitch_preserve = p.mods & MOD_NC == 0;
        if self.render_desired() {
            let game = self.game.clone().unwrap();
            let mut skin = self.skin.take().unwrap();
            let result = self.build_render_session(event_loop, &game, &mut skin);
            self.skin = Some(skin);
            match result {
                Ok(session) => self.adopt_render_session(session),
                Err(e) => {
                    if !e.is_empty() {
                        self.out.send(&Event::Error { message: e });
                    }
                }
            }
        }

        // ---- 核心会话接管,同时起播 ----
        self.track_duration_ms = match &self.audio_path {
            Some(path) => {
                let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                // 按路径+大小缓存:同一文件重复播放/切回不再起 ffprobe
                let dur = self
                    .dur_cache
                    .get(&(path.clone(), len))
                    .copied()
                    .unwrap_or_else(|| {
                        let d = audio_duration_ms(path, self.ffprobe_bin.as_deref());
                        self.dur_cache.insert((path.clone(), len), d);
                        d
                    });
                let limit = self.limit() as f64;
                dur.filter(|d| *d > limit && *d < 36_000_000.0)
                    .map(|d| d as f32)
                    .unwrap_or(limit as f32)
            }
            None => self.limit(),
        };
        self.looping = p.loop_playback;
        self.clock = Clock::new(p.start, self.track_rate * p.speed.clamp(0.05, 16.0));
        self.ended_sent = false;
        self.hs_events = hs_events;
        self.loop_events = loop_events;
        self.hs_sounds = hs_sounds;
        self.hs_cursor = self
            .hs_events
            .partition_point(|e| e.time <= (p.start + self.audio_offset_ms) as f64);
        // SB 采样游标同样按起始时刻定位:原位重载(start 在曲中)时若归
        // 零,首个 tick 会把开头到当前位置的 storyboard 语音一口气补播
        //(adopt_render_session 的归零只适合从头起播)。
        self.sb_sample_cursor = self.sb_samples.partition_point(|(t0, _, _)| {
            *t0 as f64 <= (p.start + self.audio_offset_ms) as f64
        });
        self.stop_loops();

        let duration = self.track_duration_ms;
        let fade_in = self.fade_in_ms();
        self.drop_stretch();
        let eff = self.clock.speed;
        let has_audio = match (&mut self.audio, &self.audio_path) {
            (Some(out), Some(path)) => {
                if self.pitch_preserve && (eff - 1.0).abs() > 1e-3 {
                    // 变速不变调:后台 SoundTouch tempo 预处理(整曲一次性,
                    // 完成后 AudioReady 起播,播放链路零效果器零延迟)。
                    // 等待期间时钟冻结(画面停在起始帧、音效不触发),
                    // 处理完成音乐+音效+画面同帧开始。
                    self.audio_pending = true;
                    self.spawn_stretch(eff, fade_in);
                    true
                } else {
                    // 1× 直通 / Nightcore 升调:原文件直接播
                    out.play(path, p.start, eff, 1.0, fade_in)
                }
            }
            (Some(out), None) => {
                out.stop();
                false
            }
            (None, _) => false,
        };
        self.out.send(&Event::Loaded {
            path: p.path.clone(),
            diff: None,
            duration_ms: duration,
            widescreen: true,
            has_audio,
            mods: p.mods,
        });
        if let Some(w) = &self.window {
            w.request_redraw();
        }

        // 加载路径的瞬态(字体光栅 × 5 套、背景 + 模糊副本、皮肤贴图解码、
        // 快照重排)已死,修剪把堆保留页还给 OS
        win::trim_working_set();
    }

    /// 曲终(BGM 流已终结)后的 seek 重建:按当前会话形态重启流式声 ——
    /// 直通/升调 = 原文件 × 有效速率;变速不变调 = 预处理临时文件
    /// (素材时间换算),预处理不可用回退原文件变调。与
    /// [`Self::apply_load`] 的起播分支同构。
    fn revive_bgm_at(&mut self, ms: f32) {
        if self.audio_pending {
            return;
        }
        let eff = self.clock.speed;
        let fade = self.fade_in_ms();
        let Some(out) = &mut self.audio else { return };
        let played = if self.pitch_preserve && (eff - 1.0).abs() > 1e-3 {
            match self.last_stretch.clone() {
                Some(tmp) => out.play(&tmp, ms, 1.0, eff as f64, fade),
                None => match self.audio_path.clone() {
                    Some(src) => out.play(&src, ms, eff, 1.0, fade),
                    None => false,
                },
            }
        } else {
            match self.audio_path.clone() {
                Some(src) => out.play(&src, ms, eff, 1.0, fade),
                None => false,
            }
        };
        if !played {
            log::warn!("曲终重播失败:无法在 {:.0}ms 重建 BGM 流", ms);
        }
    }

    /// 旧渲染会话退场:先渲染一帧清屏色(不留上一曲残影),再拆 GPU 会话。
    /// 窗口/桌面挂接保留复用。
    fn clear_old_render(&mut self) {
        if let Some(surf) = &mut self.surf {
            self.list.clear();
            self.list.finish();
            surf.render(&self.list, CLEAR);
        }
        self.sb_layer = None;
        self.surf = None;
        self.atlas = None;
        self.fonts = None;
        self.scene = None;
    }

    /// 当前是否应渲染:播放器模式恒不渲染;AutoPause/FsSleep 在桌面被
    /// 完全遮挡(迟滞确认)时拆除渲染会话。FsPause 保留会话(画面冻结)。
    fn render_desired(&self) -> bool {
        !self.render_mode.pure() && !(self.render_mode.drops_render_on_cover() && self.covered)
    }

    /// 按期望状态无缝切换渲染(音频/时钟/曲目全程不动):
    /// - 需要:同步重建(storyboard/图集/GPU,可能数秒;恢复场景音频
    ///   一直在播,构建期间画面保持清屏,完成后接管);
    /// - 不需要:拆会话省电。
    fn sync_render(&mut self, event_loop: &ActiveEventLoop) {
        if self.game.is_none() {
            return;
        }
        if self.render_desired() {
            if self.surf.is_none() {
                self.rebuild_render(event_loop);
            }
        } else if self.surf.is_some() || self.window.is_some() {
            self.drop_render_session();
        }
    }

    /// 暂停淡出的统一时长(Wallpaper Engine 式快速过渡:
    /// 仅防爆音,不拖泥带水;受"淡入淡出"设置控制;循环音同)。
    fn fade_swap_ms(&self) -> f64 {
        if self.fade_audio { 500.0 } else { 0.0 }
    }

    /// 恢复/起播的淡入时长:比淡出短——回来要干脆,防爆音 250ms 足够
    /// (受"淡入淡出"设置控制;循环音同)。
    fn fade_in_ms(&self) -> f64 {
        if self.fade_audio { 250.0 } else { 0.0 }
    }

    /// 遮挡触发的播放暂停(与手动 Pause 同一动作;游戏全屏场景)。
    fn cover_pause_playback(&mut self) {
        log::info!("[render] 遮挡暂停播放");
        self.clock.playing = false;
        let fade = self.fade_swap_ms();
        if let Some(out) = &mut self.audio {
            out.pause(fade);
        }
        let tween = kira::Tween {
            duration: std::time::Duration::from_secs_f64(fade.max(0.0) / 1000.0),
            ..kira::Tween::default()
        };
        for (_, _, h) in self.loop_playbacks.iter_mut() {
            h.pause(tween);
        }
    }

    /// 遮挡恢复:FsSleep 先重建渲染会话(数秒),就绪后再续播 ——
    /// 画面与音乐同时回来,没有"黑屏放歌"的窗口期。
    fn cover_resume_playback(&mut self, event_loop: &ActiveEventLoop) {
        if self.render_mode.drops_render_on_cover() {
            self.sync_render(event_loop);
        }
        // 用户手动暂停:一切自动恢复不得越过(渲染会话照常重建,画面
        // 停在暂停帧,音乐/音效保持静默)
        if self.user_paused {
            log::info!("[render] 遮挡恢复:用户已手动暂停,不续播");
            return;
        }
        log::info!("[render] 遮挡恢复播放");
        if self.game.is_some() {
            self.clock.playing = true;
            self.clock.wall_at = None;
            let fade = self.fade_in_ms();
            if let Some(out) = &mut self.audio {
                out.resume(fade);
            }
            let tween = kira::Tween {
                duration: std::time::Duration::from_secs_f64(fade.max(0.0) / 1000.0),
                ..kira::Tween::default()
            };
            for (_, _, h) in self.loop_playbacks.iter_mut() {
                h.resume(tween);
            }
        }
    }

    /// 拆除渲染会话(窗口 + GPU),保留曲目/音频/时钟/音效 —— 暂停渲染
    /// 省电的核心。窗口先置 None 再丢弃 Arc:Destroyed 事件到达时看到
    /// window 已空,不会触发"被外部销毁"的重挂流程。
    fn drop_render_session(&mut self) {
        self.sb_layer = None;
        self.surf = None;
        self.atlas = None;
        self.fonts = None;
        self.scene = None;
        self.window = None;
        self.host = None;
        self.measured_fps = 0;
        self.frame_count = 0;
        log::info!("[render] 暂停(音乐 + 音效继续)");
        // GPU 会话拆掉后堆里全是死页(图集/字体/GPU staging 的高水位残留),
        // 修剪工作集把 RAM 还给系统(实测 266MB → 个位数 MB)
        win::trim_working_set();
    }

    /// 同步重建渲染会话(暂停/纯净恢复路径):复用保留的谱面来源,
    /// 零文件重读;音频/时钟全程不动,构建期间画面保持清屏。
    /// 失败保持无渲染态(音频继续)。
    fn rebuild_render(&mut self, event_loop: &ActiveEventLoop) {
        let Some(game) = self.game.clone() else { return };
        let Some(mut skin) = self.skin.take() else { return };
        let result = self.build_render_session(event_loop, &game, &mut skin);
        self.skin = Some(skin);
        match result {
            Ok(session) => {
                self.adopt_render_session(session);
                log::info!("[render] 会话就绪(播放未中断)");
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Err(e) => {
                if !e.is_empty() {
                    log::warn!("[render] 会话重建失败(渲染保持停用,音频继续): {e}");
                }
            }
        }
        win::trim_working_set();
    }

    /// 构建渲染会话(窗口 + storyboard + 图集 + GPU)。调用前需已清掉旧
    /// GPU 会话(clear_old_render)。纯音频模式下不应调用。
    fn build_render_session(
        &mut self,
        event_loop: &ActiveEventLoop,
        game: &game::GameData,
        skin: &mut skin::ResolvedSkin,
    ) -> Result<RenderSession, String> {
        if self.window.is_none() && self.create_window(event_loop).is_err() {
            return Err(String::new()); // 错误事件已在 create_window 内上报
        }
        let Some(window) = self.window.clone() else { return Err("无壁纸窗口".into()) };
        use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
        let raw_display = window
            .display_handle()
            .map_err(|e| format!("display handle: {e}"))?
            .as_raw();
        let raw_window = window
            .window_handle()
            .map_err(|e| format!("window handle: {e}"))?
            .as_raw();
        let Some(source) = self.source.as_ref() else { return Err("会话缺谱面来源".into()) };
        prep_render_session(
            game,
            skin,
            source,
            raw_display,
            raw_window,
            self.desk,
            self.hud_visible,
            self.pp_display,
            self.hit_animations,
            self.break_lighten,
            self.cursor_on,
            self.cursor_size,
            self.gameplay_hidden,
            self.bg_opacity,
            self.sb_enabled,
            self.video_enabled,
            self.hd_on,
            self.ffmpeg_bin.as_deref(),
            self.ffprobe_bin.as_deref(),
        )
    }

    /// 渲染会话接管(字段整体替换,丢旧的会释放其 GPU 资源)。
    fn adopt_render_session(&mut self, mut session: RenderSession) {
        session.surf.resize(self.desk.0, self.desk.1);
        self.surf = Some(session.surf);
        self.sb_layer = session.sb_layer;
        self.sb_samples = session.sb_samples;
        self.stop_sb_samples();
        self.sb_sample_cursor = 0;
        self.atlas = Some(session.atlas);
        self.fonts = Some(session.fonts);
        self.scene = Some(session.scene);
        self.scene_size = session.scene_size;
    }

    /// 创建壁纸窗口(隐藏)→ 附加桌面(attach 内部完成布局探测/撑开/定位)。
    /// GPU 会话在 Load 时创建。
    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let attrs = WindowAttributes::default()
            .with_title("aria wallpaper")
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_inner_size(PhysicalSize::new(2560, 1440)); // 占位,attach 会重设
        let window = event_loop
            .create_window(attrs)
            .map_err(|e| format!("创建壁纸窗口失败: {e}"))?;
        let window = Arc::new(window);
        let hwnd = win::window_hwnd(&window).map_err(|e| format!("{e}"))?;
        let host = win::attach(hwnd);
        if host.width == 0 || host.height == 0 {
            self.out.send(&Event::Error { message: "桌面壁纸层不可用".into() });
            return Err(String::new());
        }

        self.host = Some(host);
        self.window = Some(window);
        self.wall_hwnd = Some(hwnd);
        self.desk = (host.width, host.height);
        self.window.as_ref().unwrap().set_visible(true);
        log::info!("壁纸窗口已附加桌面: {}×{}", self.desk.0, self.desk.1);
        self.out.send(&Event::Ready {
            width: self.desk.0,
            height: self.desk.1,
            refresh_hz: win::monitor_refresh_hz(),
        });
        Ok(())
    }

    /// 丢弃窗口与 GPU 会话,回到桌面空闲态。
    fn destroy_state(&mut self) {
        self.sb_layer = None;
        self.surf = None;
        self.scene = None;
        self.game = None;
        self.atlas = None;
        self.fonts = None;
        self.skin = None;
        self.source = None;
        self.window = None;
        self.host = None;
        self.audio_path = None;
        self.hs_events = Vec::new();
        self.hs_sounds = HashMap::new();
        self.hs_cursor = 0;
        self.stop_sb_samples();
        self.sb_sample_cursor = 0;
        self.loop_events = Vec::new();
        self.hs_slots = Vec::new();
        self.stop_loops();
        if let Some(out) = &mut self.audio {
            out.stop();
        }
    }

    /// 音效时间轴位置(时钟 + 偏移):所有音效触发/循环音/播放头重置共用,
    /// 保证偏移全局一致。
    fn hs_time(&self) -> f64 {
        self.clock.t as f64 + self.audio_offset_ms as f64
    }

    /// 停掉所有在播 SB 采样句柄(seek/切歌;语音不该拖到新位置)。
    fn stop_sb_samples(&mut self) {
        for mut h in self.sb_sample_handles.drain(..) {
            let _ = h.stop(kira::Tween::default());
        }
    }

    /// 打击音效推进:播放头跨过的事件即触发,随后更新循环音。
    fn fire_hitsounds(&mut self) {
        let t = self.hs_time();
        if let Some(out) = &mut self.audio {
            loop {
                let Some(event) = self.hs_events.get(self.hs_cursor).cloned() else { break };
                if event.time > t {
                    break;
                }
                out.fire(&self.hs_sounds, &event);
                self.hs_cursor += 1;
            }
        }
        // SB 采样(tutorial 语音):同一时钟跨过即触发;先回收已播完句柄
        self.sb_sample_handles.retain(|h| {
            !matches!(h.state(), kira::sound::PlaybackState::Stopped)
        });
        if let Some(out) = &mut self.audio {
            loop {
                let Some(&(time, volume, _)) = self.sb_samples.get(self.sb_sample_cursor) else { break };
                if time as f64 > t {
                    break;
                }
                let v = (volume / 100.0).clamp(0.0, 1.0) * out.hits_volume();
                if v > 0.0 {
                    let mut d = self.sb_samples[self.sb_sample_cursor].2.clone();
                    d.settings = kira::sound::static_sound::StaticSoundSettings::new()
                        .volume(crate::audio::amplitude_to_decibels(v));
                    if let Some(h) = out.play_effect(d) {
                        self.sb_sample_handles.push(h);
                    }
                }
                self.sb_sample_cursor += 1;
            }
        } else {
            self.sb_sample_cursor = self.sb_sample_cursor.max(
                self.sb_samples.partition_point(|(t0, _, _)| *t0 as f64 <= t),
            );
        }
        if self.audio.is_none() {
            // 无音频设备也要推进游标,恢复后不至于补播爆音
            self.hs_cursor = self.hs_cursor.max(
                self.hs_events.partition_point(|e| e.time <= t),
            );
        }
        self.update_loops();
    }

    /// 循环音推进:进 run 起播、区间内随参数
    /// 调整、离 run 停播。kira 声音归混音器所有,必须显式 stop。
    fn update_loops(&mut self) {
        if self.loop_events.is_empty() {
            return;
        }
        let t = self.hs_time();
        let hits_on = self.audio.as_ref().map(|a| a.hits_volume() > 0.0).unwrap_or(false);
        if !hits_on {
            self.stop_loops();
            return;
        }
        let mut active: Vec<(usize, usize)> = Vec::new();
        for (i, event) in self.loop_events.iter().enumerate() {
            if let Some(run) = event.run_at(t) {
                active.push((i, run));
            }
        }
        let mut stale: Vec<usize> = Vec::new();
        for (idx, (i, run, handle)) in self.loop_playbacks.iter_mut().enumerate() {
            match active.iter().find(|(ai, _)| *ai == *i) {
                Some((_, arun)) if *arun == *run => {
                    // 参数微更新
                    let event = &self.loop_events[*i];
                    let Some(game) = self.game.as_ref() else { continue };
                    let (rate, amp, pan_x) = event.params_at(*run, t, true, game);
                    let vol = self.audio.as_ref().map(|a| a.hits_volume()).unwrap_or(0.8);
                    let tween = kira::Tween::default();
                    handle.set_playback_rate(kira::PlaybackRate(rate), tween);
                    handle.set_volume(
                        crate::audio::amplitude_to_decibels(amp as f32 * vol),
                        tween,
                    );
                    handle.set_panning(kira::Panning(crate::audio::osu_panning(pan_x)), tween);
                }
                _ => {
                    handle.stop(kira::Tween::default());
                    stale.push(idx);
                }
            }
        }
        for idx in stale.into_iter().rev() {
            self.loop_playbacks.swap_remove(idx);
        }
        // 新 run 起播
        let Some(out) = &mut self.audio else { return };
        let vol = out.hits_volume();
        for (i, run) in active {
            if self.loop_playbacks.iter().any(|(pi, prun, _)| *pi == i && *prun == run) {
                continue;
            }
            let event = &self.loop_events[i];
            let Some(game) = self.game.as_ref() else { break };
            let (rate, amp, pan_x) = event.params_at(run, t, true, game);
            let Some(data) = self.hs_sounds.get(&event.slot()) else { continue };
            let mut data = data.clone();
            // 0 帧采样(ArgonPro 滑条循环是空文件)配 loop_region 会让
            // kira 音频线程死循环,跳过
            if data.frames.is_empty() {
                continue;
            }
            data.settings = kira::sound::static_sound::StaticSoundSettings::new()
                .loop_region(..)
                .playback_rate(kira::PlaybackRate(rate))
                .volume(crate::audio::amplitude_to_decibels(amp as f32 * vol))
                .panning(kira::Panning(crate::audio::osu_panning(pan_x)));
            if let Some(h) = out.play_effect(data) {
                self.loop_playbacks.push((i, run, h));
            }
        }
    }

    /// 皮肤热切换:只重建皮肤相关的资源 —— 重解皮肤目录、重打包图集
    /// (背景图重新解码,storyboard 槽位原尺寸重开;SB 层的 GPU 资源与
    /// 解析结果不动,每帧按新图集槽位区域合成)、`set_atlas` 热换 GPU
    /// 纹理、丢弃场景/HUD 的皮肤缓存、`Arc::get_mut` 原地重涂 combo 色、
    /// 音效采样表热换。谱面/判定/storyboard/音频/时钟全部保留:切换
    /// 只是那一帧晚到一次图集重打包,音乐不断。无渲染会话(纯音频/
    /// 未加载)时跳过 GPU 部分,只换皮肤与后续加载参数。
    /// 失败语义:皮肤目录解析失败、或此前有背景现在却解码失败时报错
    /// 并完全保持现状(热换前不做任何变更)。
    fn reswap_skin(&mut self, skin_path: Option<String>, force_colours: bool) {
        // ---- 前置解析(失败即返回,零副作用) ----
        let mut resolved = match skin::load_skin(skin_path.as_deref().map(std::path::Path::new)) {
            Ok(s) => s,
            Err(e) => {
                self.out.send(&Event::Error { message: format!("加载皮肤失败: {e}") });
                return;
            }
        };
        // 背景图:storyboard 接管时与原加载一致地跳过;否则重新解码。
        // 原会话有背景而现在解码失败 → 放弃热换(避免图集缺背景)。
        let sb_replaces = self.scene.as_ref().is_some_and(|s| s.sb_replaces_bg);
        let bg_name = self.game.as_ref().and_then(|g| g.map_background.clone());
        let want_bg = !sb_replaces && bg_name.is_some();
        let source = self.source.clone();
        let bg_image = if want_bg {
            bg_name.as_deref().and_then(|name| match source.as_ref() {
                Some(MapSource::Path { map_path }) => {
                    let cand = map_path
                        .parent()
                        .map(|d| d.join(name))
                        .unwrap_or_else(|| PathBuf::from(name));
                    cand.is_file().then_some(cand)
                }
                Some(MapSource::Virtual { files, .. }) => files.resolve(name),
                None => None,
            })
            .and_then(|cand| osu_replay_render::decode_image_file(&cand).ok())
        } else {
            None
        };
        if want_bg && bg_image.is_none() && self.scene.as_ref().is_some_and(|s| s.has_bg) {
            self.out.send(&Event::Error { message: "皮肤热切换中止:背景图重新解码失败".into() });
            return;
        }

        // ---- 以下开始变更 ----
        // last_load 先行:音效重换与后续重挂/重载都按新皮肤走。
        if let Some(load) = &mut self.last_load {
            load.skin = skin_path;
            load.force_colours = force_colours;
        }
        // combo 色:命令处理期间 self.game 是唯一持有者 → get_mut 原地
        // 重涂(零拷贝;有渲染会话时 build 路径的克隆早已释放)。
        if let Some(g) = self.game.as_mut().and_then(std::sync::Arc::get_mut) {
            game::apply_skin_combo_colours(g, &resolved, force_colours);
        }
        // GPU 会话存在:重打包图集并热换。
        if let Some(surf) = &mut self.surf {
            let (w, h) = self.scene_size;
            let sb_slot = (w.min(1920).max(1) & !1, h.min(1080).max(1) & !1);
            let slots = self.sb_layer.is_some().then(|| StoryboardSlots {
                width: sb_slot.0,
                height: sb_slot.1,
                foreground: self.scene.as_ref().is_some_and(|s| s.storyboard_fg),
            });
            let max_dim = osu_replay_render::render::Renderer::probe_max_texture_dimension_2d();
            let (mut atlas, fonts) = build_atlas(
                bg_image,
                Some(w as f32 / h.max(1) as f32),
                None,
                &mut resolved,
                max_dim,
                slots,
            );
            surf.set_atlas(&atlas);
            atlas.release_cpu_copy();
            self.atlas = Some(atlas);
            self.fonts = Some(fonts);
            if let Some(scene) = &mut self.scene {
                scene.pro_skin = !resolved.is_legacy();
                scene.invalidate_skin_cache();
            }
        }
        self.skin = Some(resolved);
        self.reswap_hitsounds();
    }

    /// 谱面自带音效运行期热切换:按新开关状态重解析当前会话的全部槽位,
    /// 整表替换 `hs_sounds`。音乐/画面/进度/事件游标都不动 —— 音效槽位
    /// 的时间轴与开关无关。在播的一次性采样自然放完(<几百 ms);循环音
    /// 停掉,下一 tick(≤1 帧)用新采样原参数重启。两种开关状态的解析
    /// 结果各占一份缓存项,来回切换零重解。
    fn reswap_hitsounds(&mut self) {
        if self.hs_slots.is_empty() {
            return;
        }
        let Some(load) = self.last_load.clone() else { return };
        let Some(skin) = self.skin.as_ref() else { return };
        let store: Option<Box<dyn osu_replay_render::hitsound::BeatmapSampleStore>> =
            if self.beatmap_hitsounds {
                match &self.source {
                    Some(MapSource::Path { map_path }) => map_path.parent().map(|dir| {
                        Box::new(osu_replay_render::hitsound::DirectorySampleStore::new(dir))
                            as Box<dyn osu_replay_render::hitsound::BeatmapSampleStore>
                    }),
                    Some(MapSource::Virtual { files, .. }) => Some(Box::new(VirtualSampleStore {
                        files: files.clone(),
                    })),
                    None => None,
                }
            } else {
                None
            };
        let slots = self.hs_slots.clone();
        let mut fresh = HashMap::new();
        {
            let mut cache = std::mem::take(&mut self.sample_cache);
            for slot in &slots {
                if let Some(bytes) = resolve_slot_sample(
                    &mut cache,
                    load.skin.as_deref(),
                    load.path.as_str(),
                    slot,
                    store.as_deref(),
                    &skin,
                ) {
                    if let Ok(data) = kira::sound::static_sound::StaticSoundData::from_cursor(
                        std::io::Cursor::new(bytes),
                    ) {
                        fresh.insert(slot.clone(), data);
                    }
                }
            }
            self.sample_cache = cache;
        }
        log::info!(
            "谱面音效热切换: {} 采样(谱面层{})",
            fresh.len(),
            if self.beatmap_hitsounds { "开" } else { "关" }
        );
        self.hs_sounds = fresh;
        self.stop_loops();
    }

    /// 停掉全部循环音。
    fn stop_loops(&mut self) {
        for (_, _, h) in self.loop_playbacks.iter_mut() {
            h.stop(kira::Tween::default());
        }
        self.loop_playbacks.clear();
    }

    fn render_frame(&mut self) {
        let Some(surf) = &mut self.surf else { return };
        let Some(game_data) = &self.game else { return };
        let Some(scene) = &mut self.scene else { return };
        let Some(atlas) = &self.atlas else { return };
        let Some(fonts) = &self.fonts else { return };
        let Some(skin) = &self.skin else { return };

        let t = self.clock.t as f64;
        let snap = game::snapshot_at(game_data, t);
        self.list.clear();
        let assets = Assets {
            atlas,
            bold: &fonts.bold,
            semibold: &fonts.semibold,
            light: &fonts.light,
            venera: &fonts.venera,
            regular: &fonts.regular,
            skin,
        };
        scene.build_frame(game_data, &assets, &snap, &mut self.list);
        self.list.finish();
        // storyboard 先合成进图集槽位(队列序在场景提交之前)
        if let Some(sb) = &mut self.sb_layer {
            sb.render(t as f32, surf.renderer_mut(), atlas);
        }
        surf.render(&self.list, CLEAR);
    }

    fn send_status(&self) {
        if self.game.is_none() {
            return;
        }
        let duration = self.track_duration_ms;
        self.out.send(&Event::Status {
            t_ms: self.clock.t,
            duration_ms: duration,
            playing: self.clock.playing,
            speed: self.clock.speed,
            user_speed: self.user_speed,
            looping: self.looping,
            fps: self.measured_fps,
        });
    }

    /// 播放推进(窗口渲染与暂停态共用):时钟积分 + 音频锚定、打击音效
    /// 触发、曲终循环/结束判定。
    fn step_playback(&mut self) {
        // 预处理等待期:时钟冻结(不积分、不锚定、不触发音效、不判
        // 曲终),渲染循环照常画出起始帧 —— BGM 就绪后同帧起播
        if self.audio_pending {
            return;
        }
        let audio_pos = self.audio.as_ref().and_then(|a| a.position_ms());
        self.clock.step(audio_pos);
        // 时钟硬跳(seek 落地 / 事件循环阻塞后的音频对齐):播放头直接跳
        // 到当前时刻,期间事件不补播(否则密集爆发),循环音重置后由
        // update_loops 按新时刻重启
        if self.clock.take_jump() {
            let t = self.hs_time();
            self.hs_cursor = self.hs_events.partition_point(|e| e.time <= t);
            self.stop_loops();
        }
        self.fire_hitsounds();
        // 有音频在播时以"音频播完"为准;音频播完(position 不可用)后墙钟
        // 从歌末位置继续,自然进入循环/结束分支。
        let audio_alive = audio_pos.is_some();
        let limit = self.limit();
        if self.clock.playing && self.clock.t >= limit && !audio_alive {
            if self.looping {
                self.restart_from_zero();
            } else {
                self.clock.playing = false;
                if !self.ended_sent {
                    self.ended_sent = true;
                    self.out.send(&Event::Ended);
                }
            }
        }
    }

    /// 循环回到开头:重建场景状态(重置动画,HUD 保持当前开关),两路音频
    /// 整曲重开;视频管道重置(只向前推帧,不重置会冻在上一遍的最后帧)。
    fn restart_from_zero(&mut self) {
        self.clock.seek(0.0);
        self.ended_sent = false;
        let game_data = self.game.as_ref().unwrap();
        if self.window.is_some() {
            let (w, h) = self.scene_size;
            let mut scene = SceneState::new(game_data, w, h);
            scene.hud.visible = self.hud_visible;
            scene.hud.pp_display = self.pp_display;
            scene.hit_animations = self.hit_animations;
            scene.break_lighten = self.break_lighten;
            scene.show_cursor = self.cursor_on;
            scene.cursor_size = self.cursor_size;
            scene.hud.ur_bar = false; // autoplay 无 UR/热图意义,恒关
            scene.hud.offset_heatmap = false;
            scene.gameplay_hidden = self.gameplay_hidden;
            self.scene = Some(scene);
        }
        self.hs_cursor = 0;
        self.stop_sb_samples();
        self.sb_sample_cursor = 0;
        self.stop_loops();
        if let Some(sb) = &mut self.sb_layer {
            sb.reset_video();
        }
        let path = self.audio_path.clone();
        let speed = self.clock.speed;
        let fade_in = self.fade_in_ms();
        if let Some(out) = &mut self.audio {
            // 循环重开:tempo 预处理文件直接复用(无需重新处理);
            // 变调路径回原文件带速率
            if let Some(tmp) = self.last_stretch.clone() {
                out.play(&tmp, 0.0, 1.0, speed as f64, fade_in);
            } else if let Some(p) = &path {
                out.play(p, 0.0, speed, 1.0, fade_in);
            }
        }
    }

    /// 后台 SoundTouch tempo 预处理:整曲解码 → 变速不变调 → 临时 WAV,
    /// 完成后经 [`Command::AudioReady`] 回事件循环起播。多次变速时,
    /// 过期任务(`stretch_job` 序号落后)静默丢弃。
    fn spawn_stretch(&mut self, speed: f32, fade_ms: f64) {
        let Some(src) = self.audio_path.clone() else { return };
        let id = self.stretch_job.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let out_path = std::env::temp_dir().join(format!("aria-bgm-{id}.wav"));
        let proxy = self.proxy.clone();
        let job = self.stretch_job.clone();
        std::thread::spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                render_tempo_wav(&src, &out_path, speed as f64)
            }))
            .unwrap_or_else(|_| Err("预处理线程 panic".into()));
            if job.load(std::sync::atomic::Ordering::Relaxed) != id {
                // 已有更新的任务:丢弃本份结果
                let _ = std::fs::remove_file(&out_path);
                return;
            }
            match r {
                Ok(()) => {
                    let _ = proxy.send_event(Command::AudioReady { speed, path: Some(out_path), fade_ms });
                }
                Err(e) => {
                    log::warn!("tempo 预处理失败({e:?})");
                    let _ = std::fs::remove_file(&out_path);
                    let _ = proxy.send_event(Command::AudioReady { speed, path: None, fade_ms });
                }
            }
        });
    }

    /// 丢弃预处理状态:作废在途任务 + 删除在播临时文件 + 解除等待。
    fn drop_stretch(&mut self) {
        self.audio_pending = false;
        self.stretch_job.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(p) = self.last_stretch.take() {
            let _ = std::fs::remove_file(p);
        }
    }

    /// 壁纸窗口被外部销毁(explorer 重启):清状态,延迟重挂。
    fn on_detached(&mut self) {
        if self.window.is_none() {
            return;
        }
        self.destroy_state();
        log::warn!("壁纸窗口被外部销毁,{} 后重挂桌面", REATTACH_DELAY.as_secs());
        self.out.send(&Event::Detached);
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(REATTACH_DELAY);
            let _ = proxy.send_event(Command::Reload);
        });
    }
}

/// 皮肤采样解析(带缓存):wav 直读;mp3/ogg/flac 由上游 symphonia
/// 进程内解码(不再起 ffmpeg 子进程)。槽位语义与上游一致 —— 皮肤提供
/// 该槽位(含 0 字节禁用占位)= 权威,解码失败即静音、**不回退内置**;
/// lazer 虚拟文件表形态的谱面采样存储:stem 按 stable 扩展顺序
/// (stem → .wav → .mp3 → .ogg)经 `VirtualFiles` 解析(全小写匹配)。
struct VirtualSampleStore {
    files: VirtualFiles,
}

impl osu_replay_render::hitsound::BeatmapSampleStore for VirtualSampleStore {
    fn sample_path(&self, stem: &str) -> Option<PathBuf> {
        let stem = stem.replace('\\', "/");
        for ext in osu_replay_render::hitsound::SAMPLE_EXTENSIONS {
            let probe = if ext.is_empty() { stem.clone() } else { format!("{stem}.{ext}") };
            if let Some(p) = self.files.resolve(&probe) {
                return Some(p);
            }
        }
        None
    }
}

/// 打击音效槽位 → wav 字节,按 lazer 层级解析:谱面自带文件
/// (LegacyBeatmapSkin,`beatmap` = None 时跳过该层)→ 皮肤 → 内置
/// 默认。结果按 (皮肤, 槽位) 缓存,换歌零重解。
fn resolve_slot_sample(
    cache: &mut HashMap<(String, String), Option<Vec<u8>>>,
    skin_path: Option<&str>,
    map_path: &str,
    slot: &osu_replay_render::hitsound::SampleSlot,
    beatmap: Option<&dyn osu_replay_render::hitsound::BeatmapSampleStore>,
    skin: &skin::ResolvedSkin,
) -> Option<Vec<u8>> {
    // 谱面层开关状态入 key:开关切换后的原位重载必须重新解析(同谱面
    // 同槽位,开/关两种状态的结果不同),否则旧缓存会让开关"看起来
    // 没生效"。
    let key = (
        format!("{}/{map_path}#bm{}", skin_path.unwrap_or_default(), beatmap.is_some() as u8),
        slot_cache_key(slot),
    );
    if let Some(hit) = cache.get(&key) {
        return hit.clone();
    }
    let (bank, name, custom, filename) = match slot {
        osu_replay_render::hitsound::SampleSlot::File { filename } => {
            ("normal", "hitnormal", 1, Some(filename.as_str()))
        }
        osu_replay_render::hitsound::SampleSlot::Bank { bank, name, custom } => {
            (*bank, *name, *custom, None)
        }
    };
    let resolved = osu_replay_render::hitsound::resolve_sample_parts(
        bank, name, custom, filename, beatmap, skin,
    );
    cache.insert(key, resolved.clone());
    resolved
}

/// 槽位缓存键:bank/name/customIndex 或显式文件名,序列化成字符串。
fn slot_cache_key(slot: &osu_replay_render::hitsound::SampleSlot) -> String {
    match slot {
        osu_replay_render::hitsound::SampleSlot::Bank { bank, name, custom } => {
            format!("{bank}-{name}@{custom}")
        }
        osu_replay_render::hitsound::SampleSlot::File { filename } => format!("file:{filename}"),
    }
}

/// BGM 真实时长(ms):ffprobe format=duration(隐藏窗口;mp3 无 Xing 头时
/// symphonia 的帧数不可靠,ffprobe 对 mp3/ogg/flac/wav 都稳)。缺失时
/// 返回 None,调用方回退谱面物件时长。`ffprobe` = 手动指定路径(None =
/// PATH 查找)。
fn audio_duration_ms(path: &std::path::Path, ffprobe: Option<&std::path::Path>) -> Option<f64> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let bin = ffprobe.unwrap_or_else(|| std::path::Path::new("ffprobe"));
    let out = std::process::Command::new(bin)
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(path)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim().parse::<f64>().ok().map(|d| d * 1000.0)
}

/// 构建渲染会话的全部重活:背景解码、storyboard 解析(巨型 SB 可达
/// 数秒)、图集/字体构建、场景、GPU 资源(含 SurfaceRenderer)。同步执行
/// —— 调用方保证在音频起播**之前**完成,音乐 + 音效 + 画面同时开始。
fn prep_render_session(
    game: &game::GameData,
    skin: &mut skin::ResolvedSkin,
    source: &MapSource,
    raw_display: raw_window_handle::RawDisplayHandle,
    raw_window: raw_window_handle::RawWindowHandle,
    desk: (u32, u32),
    hud: bool,
    pp: bool,
    hit_animations: bool,
    break_lighten: bool,
    cursor_on: bool,
    cursor_size: f32,
    gameplay_hidden: bool,
    bg_opacity: f32,
    storyboard: bool,
    video: bool,
    hd_on: bool,
    ffmpeg_bin: Option<&std::path::Path>,
    ffprobe_bin: Option<&std::path::Path>,
) -> Result<RenderSession, String> {
    // 相对名解析(背景/storyboard 素材;来自保留的谱面来源)
    let resolve = |name: &str| -> Option<PathBuf> {
        match source {
            MapSource::Path { map_path } => {
                let cand = map_path
                    .parent()
                    .map(|d| d.join(name))
                    .unwrap_or_else(|| PathBuf::from(name));
                cand.is_file().then_some(cand)
            }
            MapSource::Virtual { files, .. } => files.resolve(name),
        }
    };

    let (w, h) = scene_size(desk.0, desk.1);

    // storyboard(虚拟流:文本 + 字节回调,零拷贝)。开关关闭时完全跳过
    // 解析(巨型 SB 省数秒)与渲染 —— 无论谱面是否存在。
    // 先解析故事板:背景接管(lazer ReplacesBackground)判定决定背景图
    // 是否还要解码上传——被接管的谱面不加载背景(execute me 等)。
    let sb_parsed = if !storyboard {
        log::info!("[load] storyboard: 关闭(跳过解析)");
        None
    } else {
        match source {
            MapSource::Path { map_path } => {
                storyboard::parse_beatmap_bins(map_path, game.map_background.as_deref(), ffprobe_bin)
            }
            MapSource::Virtual { text, files } => {
                let osb_text = files.osb_text();
                let lookup = files.clone();
                let assets = SbAssets::resolver(Box::new(move |logical: &str| {
                    lookup.resolve(logical).and_then(|blob| std::fs::read(blob).ok())
                }));
                storyboard::parse_beatmap_sourced(
                    text,
                    osb_text.as_deref(),
                    game.map_background.as_deref(),
                    &|name| files.resolve(name),
                    ffprobe_bin,
                    assets,
                )
            }
        }
    };
    if storyboard {
        log::info!(
            "[load] storyboard: {}",
            match (&sb_parsed, source) {
                (Some(_), MapSource::Virtual { files, .. }) if files.osb_text().is_some() => {
                    "active(含共享 .osb,零拷贝)"
                }
                (Some(_), _) => "active",
                (None, _) => "none",
            }
        );
    }

    // 背景接管(lazer ReplacesBackground)= 不解码不上传背景图,由故事板
    // 自己绘制那张背景(随故事板暗度衰减);未接管才解码上传。
    let bg_replaced = sb_parsed.as_ref().is_some_and(|p| p.replaces_background());
    let bg_image = if bg_replaced {
        log::info!("[load] 背景: 故事板接管,跳过加载");
        None
    } else {
        game.map_background
            .as_ref()
            .and_then(|name| resolve(name))
            .and_then(|cand| match osu_replay_render::decode_image_file(&cand) {
                Ok(img) => Some(img),
                Err(e) => {
                    log::warn!("背景解码失败 {}: {e}", cand.display());
                    None
                }
            })
    };
    let has_bg = bg_image.is_some();

    // storyboard 合成槽位:封顶 1080p(场景线性上采样)
    let sb_slot = (w.min(1920).max(1) & !1, h.min(1080).max(1) & !1);
    let slots = sb_parsed
        .as_ref()
        .map(|p| StoryboardSlots { width: sb_slot.0, height: sb_slot.1, foreground: p.has_foreground() });
    // 图集上限:探测适配器 2D 纹理上限(钳 16384,对齐上游 v0.13.1 修复)。
    // 写死 8192 时大皮肤(@2x,如 Bring it on)塞不下会整包 ×0.9 降采样,
    // note 贴图变小而滑条 body 是矢量不变,观感"note 小/滑条粗";
    // 8192 上限的设备自动保留 build_atlas 内的降采样兜底。
    let atlas_max_dim = osu_replay_render::render::Renderer::probe_max_texture_dimension_2d();
    let (mut atlas, fonts) = build_atlas(
        bg_image,
        Some(w as f32 / h.max(1) as f32),
        None,
        skin,
        atlas_max_dim,
        slots,
    );

    // SurfaceRenderer:内部 16:9 场景 letterbox 到桌面比例
    let surf = SurfaceRenderer::new(w, h, &atlas, raw_display, raw_window)
        .map_err(|e| format!("初始化渲染失败: {e}"))?;
    // 像素已上传 GPU,释放 CPU 侧图集副本(4096² = 64MB;壁纸端不用
    // set_atlas 热换,区域矩形查询不受影响)
    atlas.release_cpu_copy();

    // SB 声音采样(tutorial 语音讲解):路径按谱面目录解析,解码失败静默
    // 跳过;触发链路见 fire_hitsounds(音量 × 音效音量)。
    let mut sb_samples: Vec<(f32, f32, kira::sound::static_sound::StaticSoundData)> = Vec::new();
    if let Some(p) = sb_parsed.as_ref() {
        for smp in p.samples.iter() {
            let Some(path) = resolve(&smp.path) else { continue };
            let Ok(bytes) = std::fs::read(&path) else { continue };
            if let Ok(data) = kira::sound::static_sound::StaticSoundData::from_cursor(
                std::io::Cursor::new(bytes),
            ) {
                sb_samples.push((smp.time, smp.volume, data));
            }
        }
        if sb_samples.len() > 1 {
            sb_samples.sort_by_key(|(t, _, _)| *t as i64);
        }
        if !sb_samples.is_empty() {
            log::info!("[load] SB 采样: {} 条", sb_samples.len());
        }
    }
    let sb_layer = sb_parsed.map(|p| {
        let mut l = p.into_layer(surf.device(), surf.queue(), sb_slot.0, sb_slot.1);
        // 视频层:外部 ffmpeg(rawvideo 管道)+ ffprobe,PATH 上没有则
        // 静默跳过;视频直接喂 blob 路径,ffmpeg 按内容解封装。
        // 手动指定的 bin 路径优先于 PATH。
        l.set_video_bins(ffmpeg_bin);
        l.set_video_enabled(video);
        l.set_dim(bg_opacity.clamp(0.0, 1.0));
        // 贴图预取(至多 2s,按起播时刻序):帧动画式 SB 单拍激活数百张
        // 新贴图,惰性加载会让那一帧同步解码整批(首播卡一下、回看不
        // 卡的根因);预取把解码挪到载入期,首播与回看一致。
        let t0 = std::time::Instant::now();
        let n = l.prefetch_textures(Some(t0 + std::time::Duration::from_secs(2)));
        if n > 0 {
            log::info!("[load] SB 贴图预取 {n} 张({:.2}s)", t0.elapsed().as_secs_f32());
        }
        l
    });
    let sb_active = sb_layer.is_some();

    let mut scene = SceneState::new(game, w, h);
    scene.hud.visible = hud;
    scene.hud.pp_display = pp;
    scene.hit_animations = hit_animations;
    scene.break_lighten = break_lighten;
    scene.show_cursor = cursor_on;
    scene.cursor_size = cursor_size.clamp(0.1, 2.0);
    // autoplay 完美命中:UR 条与偏移热图永远无信息量,壁纸恒关
    //(上游默认 ur_bar 开,必须显式压掉)。
    scene.hud.ur_bar = false;
    scene.hud.offset_heatmap = false;
    scene.gameplay_hidden = gameplay_hidden;
    scene.hidden = hd_on;
    // 皮肤:用户皮肤目录或 Argon Pro
    scene.pro_skin = !skin.is_legacy();
    scene.bg_opacity = if has_bg { Some(bg_opacity.clamp(0.0, 1.0)) } else { None };
    // lazer 单一 DimLevel:背景与 storyboard(含视频)同一亮度(用户设置
    // 实时生效)。无背景图时不衰减(SB 是唯一内容)。
    scene.storyboard = if sb_active {
        Some(bg_opacity.clamp(0.0, 1.0))
    } else {
        None
    };
    scene.storyboard_fg = sb_layer.as_ref().is_some_and(|l| l.has_foreground());
    // lazer `Storyboard.ReplacesBackground`:仅当 SB 的 Background 层
    // 重新声明了背景文件才隐藏背景;仅 Foreground 小效果的谱面
    // (Crack Traxxxx 等)背景保持可见。
    scene.sb_replaces_bg = sb_layer.as_ref().is_some_and(|l| l.replaces_background());
    scene.has_bg = has_bg;

    Ok(RenderSession { surf, sb_layer, sb_samples, atlas, fonts, scene, scene_size: (w, h) })
}

impl WallApp {
    /// stdin 命令分发。
    fn handle_command(&mut self, event_loop: &ActiveEventLoop, cmd: Command) {
        match cmd {
            Command::Probe { path } => match self.library.probe(&path) {
                Ok(input) => {
                    self.out.send(&Event::DiffList { path, diffs: input.diffs.clone() })
                }
                Err(e) => self.out.send(&Event::Error { message: format!("{e:#}") }),
            },
            Command::Load { path, diff, speed, start, loop_playback, manifest, skin, force_colours, hidden, mods, storyboard, video, beatmap_hitsounds, .. } => {
                self.apply_load(
                    event_loop,
                    LoadParams { path, diff, speed, start, loop_playback, manifest, skin, force_colours, hidden, mods, storyboard, video, beatmap_hitsounds },
                );
            }
            Command::Reload => {
                // 重挂桌面:窗口已销毁,按上次参数重走加载
                if let Some(p) = self.last_load.clone() {
                    self.apply_load(event_loop, p);
                }
            }
            Command::AudioReady { speed, path, fade_ms } => match path {
                Some(tmp) => {
                    if let Some(out) = &mut self.audio {
                        // clock.t = 冻结的起播点(加载等待)或当前进度
                        // (变速替换);play 内换算到压缩文件的素材时间
                        let t = self.clock.t;
                        out.play(&tmp, t, 1.0, speed as f64, fade_ms);
                        if let Some(old) = self.last_stretch.replace(tmp) {
                            let _ = std::fs::remove_file(old);
                        }
                        // 等待期结束:重置墙钟锚(等待时长不计入 dt),
                        // 用户已暂停则 BGM 同步暂停
                        if self.audio_pending {
                            self.audio_pending = false;
                            self.clock.wall_at = None;
                            let fade = if self.fade_audio { 40.0 } else { 0.0 };
                            if !self.clock.playing {
                                out.pause(fade);
                            }
                        }
                        self.send_status();
                    }
                }
                None => {
                    // 预处理失败:回退原文件变调
                    log::warn!("tempo 预处理失败,回退变调播放");
                    if let (Some(out), Some(src)) = (&mut self.audio, self.audio_path.clone()) {
                        out.play(&src, self.clock.t, speed, 1.0, 0.0);
                        self.audio_pending = false;
                        self.clock.wall_at = None;
                    }
                }
            },
            Command::Unload => {
                self.destroy_state();
                self.out.send(&Event::Unloaded);
            }
            Command::Pause => {
                self.clock.playing = false;
                self.user_paused = true;
                let fade = self.fade_swap_ms();
                if let Some(out) = &mut self.audio {
                    out.pause(fade);
                }
                let tween = kira::Tween {
                    duration: std::time::Duration::from_secs_f64(fade.max(0.0) / 1000.0),
                    ..kira::Tween::default()
                };
                for (_, _, h) in self.loop_playbacks.iter_mut() {
                    h.pause(tween);
                }
            }
            Command::Resume => {
                self.user_paused = false;
                if self.game.is_some() {
                    self.clock.playing = true;
                    self.clock.wall_at = None;
                    let fade = self.fade_in_ms();
                    if let Some(out) = &mut self.audio {
                        out.resume(fade);
                    }
                    let tween = kira::Tween {
                        duration: std::time::Duration::from_secs_f64(fade.max(0.0) / 1000.0),
                        ..kira::Tween::default()
                    };
                    for (_, _, h) in self.loop_playbacks.iter_mut() {
                        h.resume(tween);
                    }
                }
            }
            Command::Seek { ms } => {
                // 钳制到音频末尾以内:seek 落在 EOF 上/之外(UI 进度条拉满、
                // 或上一首的陈旧时长)会让 kira 流式声立刻终结 →
                // audio_alive=false;Lullaby 一类低 limit(物件早早结束)
                // 的谱面下一帧就判定曲终,表现为"点击立刻自动切歌"。
                // 留 500ms 余量,让结尾走自然结束路径。
                let ms = ms
                    .max(0.0)
                    .min((self.track_duration_ms - 500.0).max(0.0));
                // 视频 seek:倒退(管道只能向前推帧)与大前跳都重置管道,
                // 按新时刻 -ss 重起(冻旧帧 ~0.3s 后直接跳到位)。大前跳
                // 不重置的话泵按 4 帧/拍顺序追赶,30s @30fps ≈ 3.7s 的
                // 高速快进——观感即"动画很长"。小步前跳(≤2s)保留顺序
                // 追赶:画面连续推进,无冻结。
                let jump = ms - self.clock.t;
                if jump < -500.0 || jump > 2000.0 {
                    if let Some(sb) = &mut self.sb_layer {
                        sb.reset_video();
                    }
                }
                self.clock.seek(ms);
                self.ended_sent = false;
                let t = self.hs_time();
                self.hs_cursor = self.hs_events.partition_point(|e| e.time <= t);
                // SB 采样:seek 即停掉在播语音,游标按目标时刻重定位
                self.stop_sb_samples();
                self.sb_sample_cursor =
                    self.sb_samples.partition_point(|(t0, _, _)| *t0 as f64 <= t);
                self.stop_loops();
                // 曲终后 BGM 流已死(Stopped):对死流 seek 无效、后续
                // resume 也救不回 kira 句柄 —— 按当前会话形态原位重建
                // 流式声、从 seek 点续播(拖回重听)。不重建的话下一帧
                // `t >= limit && !audio_alive` 立即再判曲终:BGM 一个音
                // 都不放就跳下一首。
                let stream_dead = self
                    .audio
                    .as_ref()
                    .is_some_and(|a| a.position_ms().is_none());
                if stream_dead {
                    self.revive_bgm_at(ms);
                    // seek = 新的播放意图(镜像 apply_load;UI 的 seek 后
                    // 总跟 resume,这里先行置位让无 resume 的调用方也成立)
                    self.clock.playing = true;
                    self.user_paused = false;
                } else if let Some(out) = &mut self.audio {
                    out.seek(ms);
                }
            }
            Command::SetSpeed { x } => {
                // x 是用户倍速:实际会话速度还要乘上曲目速率 mod(DT/HT)
                self.user_speed = x.clamp(0.05, 16.0);
                let eff = (self.track_rate * self.user_speed).clamp(0.05, 16.0);
                self.clock.speed = eff;
                if self.pitch_preserve && (eff - 1.0).abs() > 1e-3 {
                    // 保持音调:后台重新预处理;旧速率播放继续到 AudioReady
                    // 无缝替换(约 1–2 秒后生效)
                    self.spawn_stretch(eff, 150.0);
                } else if self.last_stretch.is_some() {
                    // 变调路径(NC / 回到 1×):从 tempo 文件切回原文件
                    self.drop_stretch();
                    if let (Some(out), Some(src)) = (&mut self.audio, self.audio_path.clone()) {
                        out.play(&src, self.clock.t, eff, 1.0, 150.0);
                    }
                } else if let Some(out) = &mut self.audio {
                    out.set_rate(eff);
                }
            }
            Command::SetFail { .. } => {} // autoplay 模式无 Pass/Fail 层区分
            Command::SetLoop { on } => self.looping = on,
            Command::Status => self.send_status(),
            Command::SetVolume { v } => {
                if let Some(out) = &mut self.audio {
                    out.set_volume(v);
                }
            }
            Command::SetMaster { v } => {
                if let Some(out) = &mut self.audio {
                    out.set_master(v);
                }
            }
            Command::SetOffset { ms } => {
                // 实时生效:时间轴即时平移,已过/未过的事件由下帧游标自然对齐
                self.audio_offset_ms = ms;
                let t = self.hs_time();
                self.hs_cursor = self.hs_events.partition_point(|e| e.time <= t);
            }
            Command::SetHitsVolume { v } => {
                if let Some(out) = &mut self.audio {
                    out.set_hits_volume(v);
                }
            }
            Command::SetBeatmapHitsounds { on } => {
                self.beatmap_hitsounds = on;
                self.reswap_hitsounds();
            }
            Command::SetFps { fps } => {
                self.fps = fps;
                self.frame_at = None;
            }
            Command::SetLog { on } => crate::logging::set_enabled(on),
            Command::SetFade { on } => {
                self.fade_audio = on;
            }
            Command::SetGameplayHidden { on } => {
                self.gameplay_hidden = on;
                if let Some(scene) = &mut self.scene {
                    scene.gameplay_hidden = on;
                }
            }
            Command::SetHud { on } => {
                self.hud_visible = on;
                if let Some(scene) = &mut self.scene {
                    scene.hud.visible = on;
                }
            }
            Command::SetPp { on } => {
                self.pp_display = on;
                if let Some(scene) = &mut self.scene {
                    scene.hud.pp_display = on;
                }
            }
            Command::SetHitAnimations { on } => {
                self.hit_animations = on;
                if let Some(scene) = &mut self.scene {
                    scene.hit_animations = on;
                }
            }
            Command::SetBreakLighten { on } => {
                self.break_lighten = on;
                if let Some(scene) = &mut self.scene {
                    scene.break_lighten = on;
                }
            }
            Command::SetCursor { on } => {
                self.cursor_on = on;
                if let Some(scene) = &mut self.scene {
                    scene.show_cursor = on;
                }
            }
            Command::SetCursorSize { x } => {
                self.cursor_size = x.clamp(0.1, 2.0);
                if let Some(scene) = &mut self.scene {
                    scene.cursor_size = self.cursor_size;
                }
            }
            Command::SetSkin { skin, force_colours } => {
                self.reswap_skin(skin, force_colours);
            }
            Command::SetBgOpacity { v } => {
                self.bg_opacity = v.clamp(0.0, 1.0);
                // 暗度逐实例预乘进故事板精灵(合成端不再乘),实时生效
                if let Some(sb) = &mut self.sb_layer {
                    sb.set_dim(self.bg_opacity);
                }
                if let Some(scene) = &mut self.scene {
                    if scene.has_bg {
                        scene.bg_opacity = Some(self.bg_opacity);
                    }
                }
            }
            Command::SetHidden { on } => {
                self.hd_on = on || self.track_hd;
                if let Some(scene) = &mut self.scene {
                    scene.hidden = self.hd_on;
                }
            }
            Command::SetVideo { on } => {
                self.video_enabled = on;
                if let Some(sb) = &mut self.sb_layer {
                    sb.set_video_enabled(on);
                }
            }
            // 无缝切换:不重载、不打断音频,仅拆/建渲染会话与暂停/恢复播放
            Command::SetRenderMode { mode } => {
                let m = RenderMode::parse(&mode);
                if m != self.render_mode {
                    self.render_mode = m;
                    // 从"遮挡暂停播放"系切走:恢复播放(桌面可见场景)
                    if self.fs_paused && !m.pauses_playback_on_cover() && !self.covered {
                        self.fs_paused = false;
                        self.cover_resume_playback(event_loop);
                    }
                    self.sync_render(event_loop);
                }
            }
            Command::SetFfmpegBins { ffmpeg, ffprobe } => {
                // 仅供下次载入使用:不打断当前播放/渲染
                self.ffmpeg_bin = ffmpeg.map(PathBuf::from);
                self.ffprobe_bin = ffprobe.map(PathBuf::from);
            }
            Command::Quit => {
                self.drop_stretch();
                self.out.send(&Event::Exited);
                event_loop.exit();
                return; // 已退出,不再上报状态
            }
        }
        if self.window.is_some() || self.game.is_some() {
            self.send_status();
        }
    }
}

impl ApplicationHandler<Command> for WallApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        // 壁纸窗口按需在 Load/Reload 时创建,这里无事可做
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, cmd: Command) {
        self.handle_command(event_loop, cmd);
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::Resized(size) => {
                self.desk = (size.width.max(1), size.height.max(1));
                if let Some(surf) = &mut self.surf {
                    surf.resize(self.desk.0, self.desk.1);
                }
            }
            WindowEvent::RedrawRequested => {
                self.step_playback();
                self.render_frame();
                self.frame_count += 1;
                if self.fps_measure_at.elapsed() >= Duration::from_secs(1) {
                    self.measured_fps = self.frame_count;
                    self.frame_count = 0;
                    self.fps_measure_at = Instant::now();
                }
                if self.status_at.elapsed() >= Duration::from_secs(1) {
                    self.status_at = Instant::now();
                    self.send_status();
                }
            }
            WindowEvent::CloseRequested => { /* 壁纸窗口不接受用户关闭 */ }
            WindowEvent::Destroyed => self.on_detached(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // 每秒轮询(先于播放泵:暂停态没有窗口,遮挡检测仍需推进):
        // 桌面尺寸跟随 + 渲染自动暂停的遮挡检测
        if self.poll_at.elapsed() >= Duration::from_secs(1) {
            self.poll_at = Instant::now();
            // 尺寸跟随:显示器热插拔 / 分辨率变化(只读尺寸)
            if let Some(host) = self.host {
                let (w, h) = win::client_size(host.parent);
                if w > 0 && h > 0 && (w, h) != self.desk {
                    log::info!("桌面尺寸变化: {}×{} → {}×{}", self.desk.0, self.desk.1, w, h);
                    let (w, h) = win::fill_parent(host.child, host.parent);
                    self.desk = (w.max(1), h.max(1));
                    if let Some(surf) = &mut self.surf {
                        surf.resize(self.desk.0, self.desk.1);
                    }
                }
            }
            // 遮挡检测(带迟滞:连续遮挡 2s 才触发,桌面重新可见 0.5s 即恢复)。
            // 窗口销毁的暂停态用保留的 wall_hwnd 检测,恢复才能触发。
            // AutoPause/FsPause/FsSleep 都吃遮挡;FsPause 只暂停播放
            // (壁纸会话保留,画面冻结),FsSleep 暂停播放 + 拆会话释放内存。
            if !self.render_mode.pure()
                && self.render_mode != RenderMode::Always
                && self.game.is_some()
            {
                let covered = self.wall_hwnd.map(win::desktop_covered).unwrap_or(false);
                if covered != self.covered {
                    log::info!("[render] 桌面遮挡: {} → {}", self.covered, covered);
                    self.covered = covered;
                    self.covered_at = Instant::now();
                }
                let dwell = self.covered_at.elapsed();
                if (self.covered && dwell >= COVER_HOLD)
                    || (!self.covered && dwell >= UNCOVER_HOLD)
                {
                    // 播放暂停(边沿触发,fs_paused 防重入)
                    if self.render_mode.pauses_playback_on_cover() && self.game.is_some() {
                        if self.covered && !self.fs_paused {
                            self.fs_paused = true;
                            self.cover_pause_playback();
                        } else if !self.covered && self.fs_paused {
                            self.fs_paused = false;
                            self.cover_resume_playback(event_loop);
                        }
                    }
                    // 会话拆/建(AutoPause/FsSleep 遮挡拆、可见建)
                    if self.render_mode.drops_render_on_cover() {
                        self.sync_render(event_loop);
                    }
                }
            }
        }
        // 无渲染会话(纯净模式 / 渲染暂停):事件循环自驱动
        // —— 音乐与打击音效照播(storyboard 预备再久也不停),只是不画。
        if self.game.is_some() && self.surf.is_none() {
            self.step_playback();
            if self.status_at.elapsed() >= Duration::from_secs(1) {
                self.status_at = Instant::now();
                self.send_status();
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + PURE_TICK));
            return;
        }
        // 帧率门限:0 = 跟随屏幕刷新率(不节流);否则按目标帧率节流重绘。
        // 关键:未到帧边界时必须用 ControlFlow::WaitUntil 定时唤醒 —— 只
        // 跳过 request_redraw 而不设唤醒源的话,事件循环会休眠,壁纸冻结。
        // 动画(缩圈等)由主时钟采样,速度不随帧率变化,帧率只决定采样密度。
        let Some(w) = &self.window else { return };
        let now = Instant::now();
        match self.fps {
            0 => w.request_redraw(),
            fps => {
                let min_dt = Duration::from_secs_f64(1.0 / fps as f64);
                match self.frame_at {
                    Some(last) if now < last + min_dt => {
                        event_loop.set_control_flow(ControlFlow::WaitUntil(last + min_dt));
                    }
                    _ => {
                        self.frame_at = Some(now);
                        w.request_redraw();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod hits_cache_tests {
    use super::*;

    /// 手工最小 PCM16 单声道 wav(长度即层身份:谱面文件原样透传,
    /// 内置默认是另一套字节)。
    fn tiny_wav(frames: usize) -> Vec<u8> {
        let data_len = frames * 2;
        let mut out = Vec::with_capacity(44 + data_len);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&44100u32.to_le_bytes());
        out.extend_from_slice(&88200u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data_len as u32).to_le_bytes());
        out.extend(std::iter::repeat(0u8).take(data_len));
        out
    }

    /// 播放途中开关谱面音效 = 原位重载同谱面:缓存不得让关状态命中
    /// 开状态的旧解析(谱面文件 vs 皮肤/内置,字节不同),来回切换各自
    /// 稳定。
    #[test]
    fn toggle_reresolves_instead_of_stale_cache() {
        let dir = std::env::temp_dir().join(format!("aria_bm_toggle_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wav = tiny_wav(2205); // 50ms
        std::fs::write(dir.join("soft-hitnormal.wav"), &wav).unwrap();
        let store = osu_replay_render::hitsound::DirectorySampleStore::new(&dir);
        let skin = skin::load_skin(None).unwrap();
        let slot = osu_replay_render::hitsound::SampleSlot::Bank {
            bank: "soft",
            name: "hitnormal",
            custom: 1,
        };

        let mut cache: HashMap<(String, String), Option<Vec<u8>>> = HashMap::new();
        let on = resolve_slot_sample(&mut cache, None, "map.osu", &slot, Some(&store), &skin)
            .expect("开状态应命中谱面文件");
        assert_eq!(on.len(), wav.len(), "谱面 wav 原样透传");
        let off = resolve_slot_sample(&mut cache, None, "map.osu", &slot, None, &skin)
            .expect("关状态应回退内置默认");
        assert_ne!(off.len(), on.len(), "关状态不得沿用开状态的缓存字节");

        // 来回切换:各自缓存项命中,结果稳定
        let on2 = resolve_slot_sample(&mut cache, None, "map.osu", &slot, Some(&store), &skin).unwrap();
        let off2 = resolve_slot_sample(&mut cache, None, "map.osu", &slot, None, &skin).unwrap();
        assert_eq!(on2.len(), on.len());
        assert_eq!(off2.len(), off.len());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
