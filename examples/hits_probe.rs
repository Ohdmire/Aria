//! 诊断:真实谱面的打击音效解析链 —— 事件槽位的 customIndex 是否从
//! timing point sampleIndex / 对象 hitSample 正确合并(lazer ApplyTo),
//! 谱面采样层(DirectorySampleStore)命中哪些文件、与纯皮肤链的差异。
//!
//! ```text
//! cargo run --example hits_probe -- "D:/osu!/Songs/<set>/<diff>.osu"
//! ```

use osu_replay_render::hitsound::BeatmapSampleStore as _;
use std::collections::HashMap;
use std::path::Path;

fn main() {
    let map = std::env::args().nth(1).expect("usage: hits_probe <map.osu>");
    let dir = Path::new(&map).parent().unwrap().to_path_buf();
    let game = osu_replay_render::game::load_autoplay(&map, 0, false, false).expect("parse map");
    let events = osu_replay_render::hitsound::collect_events(&game, &game.sample_data);
    let loops =
        osu_replay_render::hitsound::collect_loop_events(&game, &game.sample_data);
    let store = osu_replay_render::hitsound::DirectorySampleStore::new(&dir);
    let skin = osu_replay_render::skin::load_skin(None).unwrap();

    let mut slots: Vec<_> = events.iter().map(|e| e.slot()).chain(loops.iter().map(|e| e.slot())).collect();
    slots.sort_by_key(|s| format!("{s:?}"));
    slots.dedup();

    // 每槽位:谱面层命中哪个候选文件;关掉谱面层(皮肤/内置)又是哪个字节。
    let mut beatmap_hits = 0;
    for slot in &slots {
        let (bank, name, custom, filename) = match slot {
            osu_replay_render::hitsound::SampleSlot::File { filename } => {
                ("normal", "hitnormal", 1, Some(filename.as_str()))
            }
            osu_replay_render::hitsound::SampleSlot::Bank { bank, name, custom } => {
                (*bank, *name, *custom, None)
            }
        };
        let mut answered = String::from("(miss)");
        for cand in
            osu_replay_render::hitsound::beatmap_lookup_names(bank, name, custom, filename)
        {
            if let Some(p) = store.sample_path(&cand) {
                answered = format!("{} [{}B]", p.file_name().unwrap().to_string_lossy(), std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0u64));
                beatmap_hits += 1;
                break;
            }
        }
        let with = osu_replay_render::hitsound::resolve_sample_parts(
            bank, name, custom, filename, Some(&store), &skin,
        );
        let without = osu_replay_render::hitsound::resolve_sample_parts(
            bank, name, custom, filename, None, &skin,
        );
        let len = |b: &Option<Vec<u8>>| b.as_ref().map(|v| v.len()).unwrap_or(0);
        println!(
            "{slot:?} -> beatmap {answered} | bytes with-layer={} without-layer={} {}",
            len(&with),
            len(&without),
            if with != without { "(DIFFERS: beatmap file wins)" } else { "" },
        );
    }
    println!(
        "\n{} events / {} loops / {} slots, {} slots served by beatmap files",
        events.len(),
        loops.len(),
        slots.len(),
        beatmap_hits
    );
    // customIndex 分布
    let mut dist: HashMap<i32, usize> = HashMap::new();
    for e in &events {
        *dist.entry(e.custom).or_default() += 1;
    }
    println!("customIndex distribution: {:?}", dist);
}
