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
use kira::{AudioManager, Decibels, Panning, PlaybackRate, Tween};
use std::collections::HashMap;
use std::path::Path;

use osu_replay_render::hitsound::{HitsoundEvent, SampleSlot};

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
    /// 打开默认输出设备;失败则本进程无音频(壁纸继续)。
    pub fn new() -> Option<AudioOut> {
        match AudioManager::new(Default::default()) {
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

    /// BGM 时间线(音乐时间)上的当前位置(毫秒)。Paused 返回冻结位置
    /// (锚),播完/停止返回 None,调用方回退自由计时。
    pub fn position_ms(&self) -> Option<f64> {
        let h = self.bgm.as_ref()?;
        match h.state() {
            PlaybackState::Playing | PlaybackState::Paused => {
                Some(h.position() * 1000.0 * self.timeline_scale)
            }
            _ => None,
        }
    }
}
