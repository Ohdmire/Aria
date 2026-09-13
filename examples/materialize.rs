//! 临时工具:按谱面集 online id 物化整个 lazer 谱面集到目录
//! (files/ blob → 普通文件名),供渲染端按目录直接使用。
//! usage: materialize <online_id> <dest_dir>
#[path = "../src/lazer.rs"]
mod lazer;

use std::path::Path;

fn main() {
    let online_id: i64 = std::env::args().nth(1).expect("usage: materialize <online_id> <dest_dir>").parse().expect("online_id");
    let dest = Path::new(&std::env::args().nth(2).expect("dest_dir")).to_path_buf();
    let lib = lazer::parse(Path::new(r"D:\osu\client.realm")).unwrap();
    let root = Path::new(r"D:\osu");
    let set = lib.sets.iter().find(|s| s.online_id == online_id).unwrap_or_else(|| panic!("set {online_id} 不在曲库"));
    println!("物化: {} - {} ({} 个文件)", set.artist, set.title, set.files.len());
    std::fs::create_dir_all(&dest).expect("mkdir");
    for f in &set.files {
        let blob = root.join("files").join(lazer::blob_relative_path(&f.hash));
        let target = dest.join(lazer::sanitize_filename(&f.filename));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        if target.is_file() {
            continue;
        }
        std::fs::copy(&blob, &target).unwrap_or_else(|e| panic!("copy {}: {e}", f.filename));
    }
    println!("完成 → {}", dest.display());
}
