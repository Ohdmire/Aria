//! BGM 解码探针:用壁纸同款 kira 流式入口打开音频文件,打印错误。
fn main() {
    let path = std::env::args().nth(1).expect("usage: bgm_probe <audio>");
    eprintln!("probing: {path}");
    match kira::sound::streaming::StreamingSoundData::from_file(&path) {
        Ok(_) => println!("STREAM OK"),
        Err(e) => println!("STREAM ERR: {e:?}"),
    }
    let bytes = std::fs::read(&path).unwrap();
    match kira::sound::static_sound::StaticSoundData::from_cursor(std::io::Cursor::new(bytes)) {
        Ok(d) => println!("STATIC OK: {:.1}s", d.duration().as_secs_f64()),
        Err(e) => println!("STATIC ERR: {e:?}"),
    }
}
