//! 子进程音频输出:kira —— 谱面 BGM(它的播放位置是主时钟)
//! + 打击音效按**事件实时触发**(播放头跨过事件表即播放对应
//! 采样,与画面共用同一时钟,天然无偏移)。循环音(滑条滑动/转盘旋转)
//! 由调用方管理生命周期,经 `play_effect` 起播。无音频设备时
//! `AudioOut::new` 返回 None,壁纸照常。
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
                    bgm_volume: 1.0,
                    hits_volume: 0.8,
                    master: 1.0,
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
        self.timeline_scale = timeline_scale.max(1e-6);
        let tween = Tween::default();
        match StreamingSoundData::from_file(bgm_path) {
            Ok(data) => {
                // 起播即带速率(起播后再 set 会有 1× 瞬态)
                let data = data.with_settings(
                    StreamingSoundSettings::new().playback_rate(rate.clamp(0.05, 16.0) as f64),
                );
                match self.manager.play(data) {
                    Ok(mut h) => {
                        let target = amplitude_to_decibels(self.master * self.bgm_volume);
                        if fade_ms > 0.0 {
                            h.set_volume(Decibels::SILENCE, Tween::default());
                            h.set_volume(target, fade_tween(fade_ms));
                        } else {
                            h.set_volume(target, tween);
                        }
                        if start_ms > 250.0 {
                            h.seek_to(start_ms as f64 / 1000.0 / self.timeline_scale);
                        }
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
    /// key 是槽位(bank/name/customIndex 或谱面显式文件名)。
    pub fn fire(
        &mut self,
        sounds: &HashMap<SampleSlot, StaticSoundData>,
        event: &HitsoundEvent,
    ) {
        // 用 hits_volume()(master × hits_volume)而非原始字段——
        // 之前直接读 self.hits_volume 导致总音量不影响单击音效
        if self.hits_volume() <= 0.0 {
            return;
        }
        let Some(data) = sounds.get(&event.slot()) else { return };
        let volume = event.volume.max(5) as f32 / 100.0 * self.hits_volume();
        let mut data = data.clone();
        data.settings = StaticSoundSettings::new()
            .volume(amplitude_to_decibels(volume))
            .panning(Panning(osu_panning(event.pan_x)));
        let _ = self.manager.play(data);
    }

    /// 循环音(滑条/转盘)起播:调用方持有句柄管理生命周期。
    pub fn play_effect(&mut self, data: StaticSoundData) -> Option<StaticSoundHandle> {
        self.manager.play(data).ok()
    }

    /// 打击音效实际音量(总音量 × 音效分量)。
    pub fn hits_volume(&self) -> f32 {
        self.master * self.hits_volume
    }

    pub fn stop(&mut self) {
        if let Some(mut h) = self.bgm.take() {
            let _ = h.stop(Tween::default());
        }
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
}
