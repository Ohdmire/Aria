//! 临时诊断:打印指定谱面集指定难度的零拷贝 Load 命令 JSON(管道给 --wallpaper)。
#[path = "../src/lazer.rs"]
mod lazer;

use serde_json::json;

fn main() {
    let kw = std::env::args().nth(1).unwrap_or_else(|| "My Love".into()).to_lowercase();
    let diff_kw = std::env::args().nth(2).unwrap_or_default();
    let lib = lazer::parse(std::path::Path::new(r"D:\osu\client.realm")).unwrap();
    let root = std::path::Path::new(r"D:\osu");
    let set = lib.sets.iter().find(|s| s.title.to_lowercase().contains(&kw)).expect("set 不在曲库");
        let bm = if diff_kw.is_empty() { &set.beatmaps[0] } else { set.beatmaps.iter().find(|b| b.name.contains(&diff_kw)).expect("diff 不存在") };
    let files: Vec<serde_json::Value> = set.files.iter().map(|f| json!({
        "name": f.filename,
        "path": root.join("files").join(lazer::blob_relative_path(&f.hash)).to_string_lossy(),
    })).collect();
    let cmd = json!({
        "cmd": "load",
        "path": root.join("files").join(lazer::blob_relative_path(&bm.sha2)).to_string_lossy(),
        "diff": null,
        "fail": false,
        "speed": 1.0,
        "start": 0.0,
        "loop_playback": false,
        "manifest": files,
        "skin": null,
        "force_colours": false,
        "hidden": false,
        "storyboard": true,
        "video": true,
    });
    println!("{}", cmd);
}
