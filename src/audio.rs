//! 子进程音频输出:kira —— 谱面 BGM(它的播放位置是主时钟)
//! + 打击音效按**事件实时触发**(播放头跨过事件表即播放对应
//! 采样,与画面共用同一时钟,天然无偏移)。循环音(滑条滑动/转盘旋转)
//! 由调用方管理生命周期,经 `play_effect` 起播。无音频设备时
//! `AudioOut::new` 返回 None,壁纸照常。
//!
//! 一次性打击音有**同采样并发上限**(lazer 同款 [`SAMPLE_CONCURRENCY`],
//! 单实例复用的采样只有 1 个槽,见 [`AudioOut::fire`]):谱面把大量同槽位
//! 事件压进同一毫秒时(aspire 的 65 连零长滑条),不限制会直接叠成爆音。
//!
//! 变速不变调由宿主**离线预处理**实现(wall.rs:SoundTouch tempo 全曲
//! 处理成临时 WAV 后 1.0× 播放)——播放链路上没有实时效果器,**零管线
//! 延迟**,音效/画面/BGM 同轴。变调路径(Nightcore / 预处理失败回退)
//! 直接 kira `playback_rate` 重采样。两条路径的时间轴换算见
//! [`AudioOut::timeline_scale`]。

use kira::sound::static_sound::{StaticSoundData, StaticSoundHandle, StaticSoundSettings};
use kira::sound::streaming::StreamingSoundData;
use kira::sound::streaming::{StreamingSoundHandle, StreamingSoundSettings};
use kira::sound::{FromFileError, PlaybackState};
use kira::{
    AudioManager, AudioManagerSettings, Decibels, Panning, PlaybackRate, Tween,
    // cpal 经 kira re-export 借用(同一实例,选设备 API 的类型就是它),
    // 本工程不直接依赖 cpal
    backend::cpal::{CpalBackendSettings, cpal},
};
use std::collections::HashMap;
use std::path::Path;

use osu_replay_render::hitsound::{HitsoundEvent, SampleSlot};

/// lazer `OsuGameBase.SAMPLE_CONCURRENCY` → `ISample.PlaybackConcurrency`:
/// 同一个采样文件最多几个实例同时发声。游戏里 BASS 以
/// `BassFlags.SampleOverrideLongestPlaying` 装载,第 7 个同时播放会顶掉
/// **播得最久**的那个。
const SAMPLE_CONCURRENCY: usize = 6;

/// 被顶掉实例的停止补间(ms)。BASS 是在同一时刻硬停(波形跳变 = "咔哒");
/// 这里保持**切断时刻完全一致**,只把边沿做成 2ms 线性淡出,消掉咔哒。
const STEAL_FADE_MS: f64 = 2.0;

/// 一条打击音事件允许的并发实例数(与 `osu-replay-render` 的
/// `HitSample::voice_cap` 以及 lazer 一致):
///
/// - 普通采样 = [`SAMPLE_CONCURRENCY`](6);
/// - `spinnerbonus-max`(`DrawableSpinner.maxBonusSample`)与
///   `combobreak`(`ComboEffects.comboBreakSample`)由**同一个**
///   `SkinnableSound` 实例复用,`Play()` 等价 `Stop(); Play();`,只有 1 个槽。
fn hitsound_concurrency(name: &str) -> usize {
    match name {
        "spinnerbonus-max" | "combobreak" => 1,
        _ => SAMPLE_CONCURRENCY,
    }
}

/// 枚举系统输出设备(id 用于设置/命令,名字给 UI 显示)。id 为
/// DeviceId 的 Display 形态(可持久化/跨进程传,kira 侧同源)。
pub fn output_devices() -> Vec<(String, String)> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let mut out = Vec::new();
    if let Some(devices) = host.output_devices().ok() {
        for d in devices {
            let id = d.id().map(|i| i.to_string()).unwrap_or_default();
            let name = d
                .description()
                .map(|x| x.name().to_string())
                .unwrap_or_else(|_| id.clone());
            if !id.is_empty() {
                out.push((id, name));
            }
        }
    }
    out
}

/// 按 id 找输出设备(找不到 = 已拔出/禁用,调用方决定回退)。
pub fn device_by_id(id: &str) -> Option<cpal::Device> {
    use cpal::traits::{DeviceTrait, HostTrait};
    if id.is_empty() {
        return None;
    }
    cpal::default_host()
        .output_devices()
        .ok()?
        .find(|d| d.id().map(|i| i.to_string() == id).unwrap_or(false))
}

/// 当前默认输出端点 id(wasapi-rs 查询,`wasapi:{端点id}` 形态)。
/// "跟随系统默认"不再走 cpal 的默认设备哨兵 —— 该路径的 IAudioClient
/// 后台预激活在部分环境稳定触发 COM 公寓冲突(RPC_E_CHANGED_MODE,
/// "无法在设置线程模式后对其加以更改"),而按具体端点打开无此问题;
/// 默认跟随改由本函数解析出具体端点 + 设备事件监听驱动重建。
pub fn default_output_id() -> Option<String> {
    let enumerator = wasapi::DeviceEnumerator::new().ok()?;
    let dev = enumerator.get_default_device(&wasapi::Direction::Render).ok()?;
    Some(format!("wasapi:{}", dev.get_id().ok()?))
}

/// 解析应打开的**具体端点**:指定 id 优先,失效/未指定时回退当前默认。
/// 返回 (设备, 解析到的端点 id)。解析与打开都含 WASAPI 调用,只在
/// 后台构建线程上执行(见 wall::rebuild_audio)。
pub fn resolve_target(desired: Option<&str>) -> (Option<cpal::Device>, Option<String>) {
    // 构建线程同样要先初始化 COM(wasapi-rs 查询默认端点要用)
    com_init_mta();
    if let Some(id) = desired {
        if let Some(d) = device_by_id(id) {
            return (Some(d), Some(id.to_string()));
        }
    }
    match default_output_id() {
        Some(id) => (device_by_id(&id), Some(id)),
        None => (None, None),
    }
}

/// 线程级 COM 初始化(MTA)。wasapi-rs 的 `DeviceEnumerator::new()`
/// 直接 `CoCreateInstance`,不预初始化 COM 的裸线程上会
/// CO_E_NOTINITIALIZED 失败(设备事件监听"从未启动"的根因)。已初始化
/// 成其他公寓(RPC_E_CHANGED_MODE)无妨 —— COM 会跨公寓封送,照常可用;
/// cpal 侧自己的 STA 初始化也容忍该返回值,互不冲突。
fn com_init_mta() {
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    // 初始化后不再撤销:调用线程要么常驻(监视线程),要么短命但重复
    // 调用得到 S_FALSE,均无害
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    debug_assert!(hr.is_ok() || hr == RPC_E_CHANGED_MODE);
}

/// 设备热拔插事件(去抖合并后的一组,见 [`spawn_device_watcher`])。
/// id 统一为 cpal DeviceId 的 Display 形态(`wasapi:{端点id}`),
/// 与设置存储/`device_by_id` 同源可比。
#[derive(Debug, Clone)]
pub enum AudioDeviceEvent {
    /// 默认输出设备变化(新默认 id;None = 已无默认设备)。
    DefaultChanged(Option<String>),
    /// 设备出现/激活。
    DeviceActive(String),
    /// 设备移除/失活。
    DeviceGone(String),
}

/// 订阅系统音频设备事件(IMMNotificationClient,wasapi-rs 封装),
/// 去抖 300ms 合并为一次回调 —— 一次拔插会触发成簇事件。回调在专用
/// 监视线程上执行(非渲染线程)。枚举器与注册句柄都是 !Send 且必须
/// 同线程存活,本函数自管线程,随进程存活。返回是否订阅成功(失败时
/// 调用方退回轮询兜底)。
pub fn spawn_device_watcher(
    on_events: impl Fn(&[AudioDeviceEvent]) + Send + 'static,
) -> bool {
    use std::sync::mpsc;
    // wasapi-rs 给裸端点 id;本项目标识带 "wasapi:" 前缀(cpal Display),
    // 统一转换后发出
    fn with_host_prefix(id: String) -> String {
        format!("wasapi:{id}")
    }
    let (tx, rx) = mpsc::channel::<AudioDeviceEvent>();
    let spawned = std::thread::Builder::new()
        .name("audio-dev-watch".into())
        .spawn(move || {
            // 裸线程先初始化 COM(见 com_init_mta 注释),否则下面的
            // CoCreateInstance 必败 —— 监听从未启动过正是这个原因
            com_init_mta();
            // 启动期可能撞上设备切换的扰动,失败带错误重试几次
            let mut enumerator = None;
            for attempt in 1..=3 {
                match wasapi::DeviceEnumerator::new() {
                    Ok(e) => {
                        enumerator = Some(e);
                        break;
                    }
                    Err(err) => {
                        log::warn!("[audio] 设备事件枚举器创建失败(第{attempt}次):{err:?}");
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            }
            let Some(mut enumerator) = enumerator else {
                log::warn!("[audio] 设备事件监听不可用,退回轮询兜底");
                return;
            };
            let mut cbs = wasapi::DeviceEventCallbacks::new();
            {
                let tx = tx.clone();
                cbs.set_default_device_callback(move |dir, _role, id| {
                    if matches!(dir, wasapi::Direction::Render) {
                        let _ = tx.send(AudioDeviceEvent::DefaultChanged(
                            id.map(with_host_prefix),
                        ));
                    }
                });
            }
            {
                let tx = tx.clone();
                cbs.set_device_added_callback(move |id| {
                    let _ = tx.send(AudioDeviceEvent::DeviceActive(with_host_prefix(id)));
                });
            }
            {
                let tx = tx.clone();
                cbs.set_device_removed_callback(move |id| {
                    let _ = tx.send(AudioDeviceEvent::DeviceGone(with_host_prefix(id)));
                });
            }
            {
                let tx = tx.clone();
                cbs.set_device_state_callback(move |id, state| {
                    // 状态回到 Active 视为出现,其余(禁用/拔出/不在场)视为失活
                    let ev = if matches!(state, wasapi::DeviceState::Active) {
                        AudioDeviceEvent::DeviceActive(with_host_prefix(id))
                    } else {
                        AudioDeviceEvent::DeviceGone(with_host_prefix(id))
                    };
                    let _ = tx.send(ev);
                });
            }
            // 注册句柄必须与枚举器同线程持有直到进程退出
            let _registration = match enumerator.register_notification_callback(cbs) {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("[audio] 设备事件注册失败({e:?}),退回轮询兜底");
                    return;
                }
            };
            log::info!("[audio] 设备事件监听已启动 (IMMNotificationClient)");
            // 去抖循环:首事件后 300ms 内到达的合并为一组,一次回调
            while let Ok(first) = rx.recv() {
                let mut batch = vec![first];
                while let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
                    batch.push(ev);
                }
                on_events(&batch);
            }
        });
    spawned.is_ok()
}

/// 线性振幅 → 分贝(kira 音量单位)。
pub fn amplitude_to_decibels(amplitude: f32) -> Decibels {
    if amplitude <= 0.001 {
        Decibels::SILENCE
    } else {
        Decibels(20.0 * amplitude.log10())
    }
}

/// osu 打击位置 x(0..512)→ kira 声像(0..1)。
pub fn osu_panning(x: f32) -> f32 {
    let b = (1.6 * (x as f64 / 512.0 - 0.5) * 100.0).round() / 100.0;
    ((b.clamp(-1.0, 1.0) + 1.0) / 2.0).clamp(0.0, 1.0) as f32
}

/// 淡入淡出补间(等强缓动,听感线性)。
fn fade_tween(ms: f64) -> Tween {
    Tween { duration: std::time::Duration::from_secs_f64(ms.max(0.0) / 1000.0), ..Tween::default() }
}

pub struct AudioOut {
    manager: AudioManager,
    /// 谱面 BGM(流式解码,内存占用 ≈ 小缓冲而非整曲 PCM)。
    bgm: Option<StreamingSoundHandle<FromFileError>>,
    /// 音乐时间换算:position()/seek 的文件时间 × scale = 音乐时间。
    /// 1.0 = 原文件(kira 的 playback_rate 已按速率推进素材时间轴);
    /// s = tempo 预处理文件以 1.0× 播放(文件时长 = 音乐时长 / s)。
    timeline_scale: f64,
    bgm_volume: f32,
    hits_volume: f32,
    /// 总音量(主增益):实际音量 = master × 各分量。
    master: f32,
    /// 在播的一次性打击音句柄,按**池**(槽位 + `layered`)分组 —— 并发上限
    /// 的账本,见 [`AudioOut::fire`]。kira 的句柄只是控制柄,丢弃不会停声,
    /// 所以必须留着才能"顶掉最久的那个"。
    hits_active: HashMap<(SampleSlot, bool), Vec<StaticSoundHandle>>,
}

impl AudioOut {
    /// 打开输出设备(`None` = 系统默认,kira 内置 500ms 轮询跟随默认设备
    /// 切换/热拔插);失败则本进程无音频(壁纸继续)。
    /// **含 WASAPI 阻塞调用,不得在渲染线程上执行**(由 wall.rs 后台
    /// 线程构建后经事件循环挂载)。
    pub fn new(device: Option<cpal::Device>) -> Option<AudioOut> {
        let settings = AudioManagerSettings {
            backend_settings: CpalBackendSettings { device, config: None },
            ..Default::default()
        };
        match AudioManager::new(settings) {
            Ok(manager) => {
                log::info!("音频输出已打开 (kira)");
                Some(AudioOut {
                    manager,
                    bgm: None,
                    timeline_scale: 1.0,
                    bgm_volume: 0.6,
                    hits_volume: 0.6,
                    master: 0.6,
                    hits_active: HashMap::new(),
                })
            }
            Err(e) => {
                log::warn!("无可用音频设备,静音播放: {e:?}");
                None
            }
        }
    }

    /// 换曲:BGM 流式起播,从音乐时间 `start_ms` 起。`rate` = kira 播放
    /// 速率(变调路径 = 有效速度,恒播原文件;预处理文件恒 1.0);
    /// `timeline_scale` 见结构体字段。`fade_ms > 0` 时从静音淡入。
    ///
    /// 音量/淡入/起点必须全部写进**创建参数**(StreamingSoundSettings),
    /// 不能起播后拿句柄补:句柄命令最早也要到下一个渲染批次才被声音
    /// 读到,窗口期里新声以 kira 默认音量 0dB(满格振幅)直出——换曲
    /// 瞬间"突然很大声、音量限制不生效"正是这个竞态;seek_to 同理由
    /// 解码线程异步消费,跳转生效前会先把预充的开头帧放掉。settings 的
    /// volume 从第一帧就位,fade_in_tween 由 kira 原生从静音补间到该
    /// 音量,start_position 让解码器出声前就定位到目标帧。
    pub fn play(
        &mut self,
        bgm_path: &Path,
        start_ms: f32,
        rate: f32,
        timeline_scale: f64,
        fade_ms: f64,
    ) -> bool {
        // 旧句柄让位:带短淡出(与新句柄的淡入交叉,变速替换/换曲无
        // 硬切);fade_ms = 0(用户关淡入淡出)时瞬切。kira 的
        // stop(tween) = 补间到静音后停止。
        if let Some(mut h) = self.bgm.take() {
            let _ = h.stop(if fade_ms > 0.0 { fade_tween(120.0) } else { Tween::default() });
        }
        // 旧谱面还在响的一次性打击音同样让位(和新 BGM 同一条淡出曲线),
        // 否则换曲后残留的打击音会盖到新曲开头
        self.stop_hits(if fade_ms > 0.0 { 120.0 } else { 0.0 });
        self.timeline_scale = timeline_scale.max(1e-6);
        match StreamingSoundData::from_file(bgm_path) {
            Ok(data) => {
                // 起播即带速率与目标音量(起播后再 set 会有按默认值
                // 直出的瞬态)
                let mut settings = StreamingSoundSettings::new()
                    .playback_rate(rate.clamp(0.05, 16.0) as f64)
                    .volume(amplitude_to_decibels(self.master * self.bgm_volume));
                if fade_ms > 0.0 {
                    settings = settings.fade_in_tween(fade_tween(fade_ms));
                }
                if start_ms > 250.0 {
                    settings =
                        settings.start_position(start_ms as f64 / 1000.0 / self.timeline_scale);
                }
                match self.manager.play(data.with_settings(settings)) {
                    Ok(h) => {
                        self.bgm = Some(h);
                        true
                    }
                    Err(e) => {
                        log::warn!("BGM 流式起播失败: {e:?}");
                        false
                    }
                }
            }
            Err(e) => {
                log::warn!("BGM 解码失败(格式不受支持?) {}: {e:?}", bgm_path.display());
                false
            }
        }
    }

    /// 当前 BGM 淡出(换曲/停止前调用:与载入耗时重叠,过渡无感)。
    pub fn fade_out(&mut self, ms: f64) {
        if let Some(h) = &mut self.bgm {
            h.set_volume(Decibels::SILENCE, fade_tween(ms));
        }
    }

    /// 当前 BGM 淡入到目标音量(暂停恢复防曝音)。
    pub fn fade_in(&mut self, ms: f64) {
        if let Some(h) = &mut self.bgm {
            h.set_volume(amplitude_to_decibels(self.master * self.bgm_volume), fade_tween(ms));
        }
    }

    /// 触发一次性打击音效:事件音量 × 音效主音量
    /// + 位置声像。采样在加载时预解码,此处仅克隆 settings 起播。表的
    /// key 是槽位(bank/name/customIndex 或谱面显式文件名)加 `layered`
    /// 位:同一槽位可能来自普通采样,也可能来自自动附带的 layered
    /// hitnormal,而 `LayeredHitSounds: 0` 只静音后者。
    ///
    /// **同采样并发上限**(lazer 同款,见 [`hitsound_concurrency`]):每个池
    /// 最多 `cap` 个实例同时发声,再多就顶掉**播得最久**的那个。没有这层
    /// 限制时,谱面把大量同槽位事件压进同一毫秒就会叠成爆音 —— 例如
    /// aspire 图 5839931(`Camellia - FM Synthesis Experiment`)第 5774 行的
    /// `398,380,124774,6,0,L|399:381,65,2147483647`:`slides=65` 的零长度
    /// 滑条让 64 个 repeat 全判在同一毫秒,同一发 `soft-hitnormal`(85%)
    /// 要叠 65 层 ≈ +36 dB,直接削顶;游戏里 BASS 只留 6 个,所以不会这样。
    /// 池按 `(槽位, layered)` 分:BASS 是按**文件**分池的,若谱面里两个
    /// 槽位恰好解析到同一个文件会有两个池(只是这种谱面才有的近似,两个
    /// 池各自最多 6 个,总量仍被限制住,不会再现 65 层)。
    pub fn fire(
        &mut self,
        sounds: &HashMap<(SampleSlot, bool), StaticSoundData>,
        event: &HitsoundEvent,
    ) {
        // 用 hits_volume()(master × hits_volume)而非原始字段——
        // 之前直接读 self.hits_volume 导致总音量不影响单击音效
        if self.hits_volume() <= 0.0 {
            return;
        }
        let slot = event.slot();
        let Some(data) = sounds.get(&(slot.clone(), event.layered)) else { return };
        let volume = event.volume.max(5) as f32 / 100.0 * self.hits_volume();
        let mut data = data.clone();
        data.settings = StaticSoundSettings::new()
            .volume(amplitude_to_decibels(volume))
            .panning(Panning(osu_panning(event.pan_x)));

        let cap = hitsound_concurrency(event.name);
        let pool = self.hits_active.entry((slot, event.layered)).or_default();
        // 先清掉已经播完的(账本只留活着的,否则句柄无限增长)
        pool.retain(|h| !matches!(h.state(), PlaybackState::Stopped));
        // 满了就顶掉队首(最早起播 = 播得最久),与
        // `SampleOverrideLongestPlaying` 同一时刻、同一个受害者
        while pool.len() >= cap {
            let mut oldest = pool.remove(0);
            let _ = oldest.stop(fade_tween(STEAL_FADE_MS));
        }
        if let Ok(handle) = self.manager.play(data) {
            pool.push(handle);
        }
    }

    /// 循环音(滑条/转盘)起播:调用方持有句柄管理生命周期。
    pub fn play_effect(&mut self, data: StaticSoundData) -> Option<StaticSoundHandle> {
        self.manager.play(data).ok()
    }

    /// 打击音效实际音量(总音量 × 音效分量)。
    pub fn hits_volume(&self) -> f32 {
        self.master * self.hits_volume
    }

    /// 收掉在播的一次性打击音(换曲/停止/重建会话时调用)。
    ///
    /// kira 的 `StaticSoundHandle` 只是控制柄:句柄丢弃**不会**停止声音,
    /// 所以这些实例必须由本结构显式 `stop`,否则换曲后旧谱面的打击音还会
    /// 继续响完,且账本里留下再也无法回收的死句柄。
    fn stop_hits(&mut self, ms: f64) {
        for (_, pool) in self.hits_active.drain() {
            for mut h in pool {
                let _ = h.stop(if ms > 0.0 { fade_tween(ms) } else { Tween::default() });
            }
        }
    }

    pub fn stop(&mut self) {
        if let Some(mut h) = self.bgm.take() {
            let _ = h.stop(Tween::default());
        }
        self.stop_hits(0.0);
        // 循环音句柄由调用方持有并停止
    }

    /// 暂停:`ms > 0` 时音量补间到静音再暂停(kira pause(tween) 语义),
    /// 防瞬断曝音。
    pub fn pause(&mut self, ms: f64) {
        if let Some(h) = &mut self.bgm {
            h.pause(if ms > 0.0 { fade_tween(ms) } else { Tween::default() });
        }
    }

    /// 恢复:`ms > 0` 时从静音补间回目标音量,防瞬起曝音。
    pub fn resume(&mut self, ms: f64) {
        if let Some(h) = &mut self.bgm {
            h.resume(if ms > 0.0 { fade_tween(ms) } else { Tween::default() });
        }
    }

    /// 跳到音乐时间 `ms`(换算到当前文件的素材时间)。
    pub fn seek(&mut self, ms: f32) {
        let secs = ms.max(0.0) as f64 / 1000.0 / self.timeline_scale;
        if let Some(h) = &mut self.bgm {
            h.seek_to(secs);
        }
    }

    /// 实时变速(变调:kira 重采样)。仅原文件路径适用——tempo 预处理
    /// 文件的变速需要重新预处理,由宿主负责换文件。
    pub fn set_rate(&mut self, x: f32) {
        if let Some(h) = &mut self.bgm {
            h.set_playback_rate(PlaybackRate(x.clamp(0.05, 16.0) as f64), Tween::default());
        }
    }

    /// BGM 音量(0.0–1.0 线性振幅;总音量之下的分量)。
    pub fn set_volume(&mut self, v: f32) {
        self.bgm_volume = v.clamp(0.0, 1.0);
        if let Some(h) = &mut self.bgm {
            h.set_volume(amplitude_to_decibels(self.master * self.bgm_volume), Tween::default());
        }
    }

    /// 总音量(主增益):立即作用于在播 BGM;音效为触发时读取,天然生效。
    pub fn set_master(&mut self, v: f32) {
        self.master = v.clamp(0.0, 1.0);
        if let Some(h) = &mut self.bgm {
            h.set_volume(amplitude_to_decibels(self.master * self.bgm_volume), Tween::default());
        }
    }

    /// 打击音效主音量(0.0–1.0,0 = 关;作用于事件触发与循环音)。
    pub fn set_hits_volume(&mut self, v: f32) {
        self.hits_volume = v.clamp(0.0, 1.0);
    }

    /// BGM 时间线(音乐时间)上的当前位置(毫秒)。Paused/WaitingToResume
    /// 返回冻结位置(锚),播完(Stopped,不可再恢复)返回 None,调用方
    /// 回退自由计时。补间中(Pausing/Resuming/Stopping)仍在出声,必须
    /// 算活 —— 漏掉 Resuming 会把刚 resume/重建的流误判为死,曲终门
    /// 一帧瞬杀(表现为"seek 进结尾无 note 段立刻跳下一首")。
    pub fn position_ms(&self) -> Option<f64> {
        let h = self.bgm.as_ref()?;
        match h.state() {
            PlaybackState::Stopped => None,
            _ => Some(h.position() * 1000.0 * self.timeline_scale),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 设备枚举/按 id 解析冒烟:枚举不 panic;本机有设备时 id 能回查。
    /// (无输出设备的 CI 机器上仅验证不 panic。)
    #[test]
    fn audio_device_enumerate_and_lookup() {
        let devs = output_devices();
        if let Some((id, name)) = devs.first().cloned() {
            assert!(!id.is_empty() && !name.is_empty());
            assert!(
                device_by_id(&id).is_some(),
                "枚举出的设备应能按 id 回查"
            );
        }
        assert!(device_by_id("").is_none(), "空 id 不应解析出设备");
        assert!(device_by_id("不存在的设备id").is_none());
    }

    /// 并发池容量与 lazer(`osu-replay-render::hitsound` 的
    /// `HitSample::voice_cap`)一致:普通采样 6,单实例复用的
    /// `spinnerbonus-max` / `combobreak` 只 1 个槽。
    #[test]
    fn hitsound_concurrency_matches_lazer() {
        assert_eq!(hitsound_concurrency("hitnormal"), SAMPLE_CONCURRENCY);
        assert_eq!(hitsound_concurrency("hitclap"), SAMPLE_CONCURRENCY);
        assert_eq!(hitsound_concurrency("slidertick"), SAMPLE_CONCURRENCY);
        assert_eq!(hitsound_concurrency("spinnerbonus"), SAMPLE_CONCURRENCY);
        assert_eq!(hitsound_concurrency("spinnerbonus-max"), 1);
        assert_eq!(hitsound_concurrency("combobreak"), 1);
        assert_eq!(SAMPLE_CONCURRENCY, 6, "OsuGameBase.SAMPLE_CONCURRENCY");
    }

    /// 声像必须夹在 [0,1]:aspire 的 `pixelLength = i32::MAX` 滑条会把
    /// 路径位置放大到 ~1.5e9(见 `osu-replay-render` 的 `balance`),不夹
    /// 的话 kira 会收到越界 panning。
    #[test]
    fn osu_panning_clamps_out_of_range_positions() {
        assert!((osu_panning(0.0) - 0.1).abs() < 1e-6, "最左:bal=-0.8 → 0.1");
        assert!((osu_panning(512.0) - 0.9).abs() < 1e-6, "最右:bal=+0.8 → 0.9");
        assert_eq!(osu_panning(256.0), 0.5);
        for x in [1518500608.0f32, -1518500608.0, 1.0e30, -1.0e30] {
            let p = osu_panning(x);
            assert!((0.0..=1.0).contains(&p), "osu_panning({x}) = {p} 必须落在 [0,1]");
        }
        assert_eq!(osu_panning(1518500608.0), 1.0, "越界 → 硬右");
        assert_eq!(osu_panning(-1518500608.0), 0.0, "越界 → 硬左");
    }
}
