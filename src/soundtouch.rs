//! SoundTouch(LGPL-2.1)的安全包装:C++ 源码由 build.rs 经 cc 静态编译,
//! `native/soundtouch_shim.cpp` 导出纯 C 接口(避免 MSVC C++ 符号修饰
//! 与 Rust 侧的绑定差异)。本文件只封装壁纸用到的最小子集:流式
//! `put_samples`/`receive_samples` + pitch(变调不变速,帧率守恒)。

use std::ffi::c_void;

mod ffi {
    use super::c_void;
    unsafe extern "C" {
        pub(super) fn st_new() -> *mut c_void;
        pub(super) fn st_free(st: *mut c_void);
        pub(super) fn st_set_channels(st: *mut c_void, n: u32);
        pub(super) fn st_set_sample_rate(st: *mut c_void, rate: u32);
        pub(super) fn st_set_pitch(st: *mut c_void, pitch: f64);
        pub(super) fn st_set_tempo(st: *mut c_void, tempo: f64);
        pub(super) fn st_put_samples(st: *mut c_void, samples: *const f32, n: u32);
        pub(super) fn st_receive_samples(st: *mut c_void, out: *mut f32, max: u32) -> u32;
        pub(super) fn st_flush(st: *mut c_void);
        pub(super) fn st_clear(st: *mut c_void);
    }
}

/// 一个 SoundTouch 处理实例(多声道交错流)。非 Clone;Drop 释放。
pub struct SoundTouch(*mut c_void);

impl SoundTouch {
    pub fn new() -> SoundTouch {
        SoundTouch(unsafe { ffi::st_new() })
    }

    /// 声道数(交错输入按帧计数)。
    pub fn set_channels(&mut self, n: u32) {
        unsafe { ffi::st_set_channels(self.0, n) }
    }

    pub fn set_sample_rate(&mut self, rate: u32) {
        unsafe { ffi::st_set_sample_rate(self.0, rate) }
    }

    /// pitch 因子(1.0 = 原调;变调不变速,长期帧率守恒)。
    pub fn set_pitch(&mut self, pitch: f64) {
        unsafe { ffi::st_set_pitch(self.0, pitch) }
    }

    /// tempo 因子(1.0 = 原速;变速不变调)。
    pub fn set_tempo(&mut self, tempo: f64) {
        unsafe { ffi::st_set_tempo(self.0, tempo) }
    }

    /// 输入交错样本(`samples.len() = 帧数 × 声道数`,`n` = 帧数)。
    pub fn put_samples(&mut self, samples: &[f32], n: usize) {
        unsafe { ffi::st_put_samples(self.0, samples.as_ptr(), n as u32) }
    }

    /// 取出至多 `max` 帧到 `out`(`out.len() ≥ max × 声道数`),
    /// 返回实际帧数。
    pub fn receive_samples(&mut self, out: &mut [f32], max: usize) -> usize {
        unsafe { ffi::st_receive_samples(self.0, out.as_mut_ptr(), max as u32) as usize }
    }

    /// 冲刷尾样本(离线处理用;实时流不调)。
    pub fn flush(&mut self) {
        unsafe { ffi::st_flush(self.0) }
    }

    /// 清空内部状态(速率/音调设置保留)。
    pub fn clear(&mut self) {
        unsafe { ffi::st_clear(self.0) }
    }
}

impl Drop for SoundTouch {
    fn drop(&mut self) {
        unsafe { ffi::st_free(self.0) }
    }
}

// kira Effect 在渲染线程持有;实例不做内部共享,跨线程 move 安全。
unsafe impl Send for SoundTouch {}

impl Default for SoundTouch {
    fn default() -> Self {
        Self::new()
    }
}

/// 离线变速不变调(tempo):交错立体声 f32 全曲一次处理。
/// 输出时长 = 输入 / tempo;播放侧零延迟(实时管线才会有的固有
/// buffering 延迟不存在于"先处理好再播"的用法)。
pub fn stretch_tempo(samples: &[f32], sample_rate: u32, tempo: f64) -> Vec<f32> {
    let mut st = SoundTouch::new();
    st.set_channels(2);
    st.set_sample_rate(sample_rate);
    st.set_tempo(tempo);
    // 分块喂入 + 随手收干,内存峰值 ≈ 输出本身
    let mut out: Vec<f32> = Vec::with_capacity((samples.len() as f64 / tempo) as usize + 4096);
    let mut buf = vec![0.0f32; 8192 * 2];
    for chunk in samples.chunks(8192 * 2) {
        if chunk.len() % 2 == 0 {
            st.put_samples(chunk, chunk.len() / 2);
        }
        loop {
            let got = st.receive_samples(&mut buf, 8192);
            if got == 0 {
                break;
            }
            out.extend_from_slice(&buf[..got * 2]);
        }
    }
    st.flush();
    loop {
        let got = st.receive_samples(&mut buf, 8192);
        if got == 0 {
            break;
        }
        out.extend_from_slice(&buf[..got * 2]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tempo = 变速不变调:时长 ×1/tempo、频率保持。
    #[test]
    fn tempo_keeps_pitch() {
        let sr = 44_100u32;
        let n = sr as usize; // 1 秒
        let input: Vec<f32> = (0..n * 2)
            .map(|i| {
                let t = (i / 2) as f32 / sr as f32;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5
            })
            .collect();
        let out = stretch_tempo(&input, sr, 1.5);
        // 时长 ≈ 1/1.5 秒(容差 ±5%:WSOLA 端部余量)
        let ratio = out.len() as f64 / input.len() as f64;
        assert!((ratio - 1.0 / 1.5).abs() < 0.05, "时长比 {ratio:.3}");
        // 频率保持 440(过零点估频)
        let mut crossings = 0usize;
        for w in out.chunks(2).skip(2000).map(|c| c[0]).collect::<Vec<_>>().windows(2) {
            if (w[0] < 0.0 && w[1] >= 0.0) || (w[0] >= 0.0 && w[1] < 0.0) {
                crossings += 1;
            }
        }
        let secs = (out.len() / 2 - 2000) as f32 / sr as f32;
        let f = crossings as f32 / 2.0 / secs;
        assert!((f - 440.0).abs() < 20.0, "频率 {f:.1} Hz,期望 ~440");
    }
}
