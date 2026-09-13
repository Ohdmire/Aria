//! seek/结尾状态探针:壁纸同款 kira 流式入口打开音频,seek 到指定秒,
//! 轮询句柄 state/position(`AudioOut::position_ms` 同款判定:
//! Playing|Paused => Some,否则 None),复现"seek 进尾部无 note 段"
//! 与"自然播完"两个场景下音频存活/曲终判定的真实行为。
//! usage: seek_probe <audio> <seek_sec> [poll_secs]
fn main() {
    let path = std::env::args().nth(1).expect("usage: seek_probe <audio> <seek_sec> [poll_secs]");
    let seek_sec: f64 = std::env::args().nth(2).expect("seek_sec").parse().unwrap();
    let poll_secs: f64 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30.0);

    let mut manager =
        kira::AudioManager::<kira::backend::cpal::CpalBackend>::new(Default::default())
            .expect("audio manager");
    let data = kira::sound::streaming::StreamingSoundData::from_file(&path).expect("stream open");
    let data = data.with_settings(kira::sound::streaming::StreamingSoundSettings::new());
    let mut h = manager.play(data).expect("play");
    // 静音探测,不打扰
    h.set_volume(kira::Decibels::SILENCE, kira::Tween::default());

    let start = std::time::Instant::now();
    let mut sought = false;
    let mut last_state = String::new();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let state = h.state();
        let pos = h.position();
        let s = format!("{state:?}");
        if s != last_state {
            println!(
                "[{:>7.1}s] state={:?} pos={:.3}s",
                start.elapsed().as_secs_f64(),
                state,
                pos
            );
            last_state = s;
        } else if (start.elapsed().as_secs_f64() * 5.0) as i64 % 25 == 0 {
            println!("[{:>7.1}s] ... pos={:.3}s", start.elapsed().as_secs_f64(), pos);
        }
        if !sought && start.elapsed().as_secs_f64() > 0.05 {
            println!("[{:>7.1}s] >>> seek_to({seek_sec:.2}s)", start.elapsed().as_secs_f64());
            h.seek_to(seek_sec);
            sought = true;
        }
        // position_ms 判定:非 Playing/Paused 即音频死亡(audio_alive=false)
        let alive = matches!(state, kira::sound::PlaybackState::Playing | kira::sound::PlaybackState::Paused);
        if !alive {
            println!(
                "[{:>7.1}s] !!! STREAM DEAD state={state:?} pos={pos:.3}s (audio_alive=false)",
                start.elapsed().as_secs_f64()
            );
            return;
        }
        if start.elapsed().as_secs_f64() > poll_secs {
            println!("[{:>7.1}s] poll window over, still alive, pos={pos:.3}s", start.elapsed().as_secs_f64());
            return;
        }
    }
}
