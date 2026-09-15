//! osu!stable 曲库:
//! `osu!.db` 提供谱面全量数据(元数据 / 星级 / 时长 / 文件夹与 .osu 文件名),
//! `collection.db` 提供收藏夹(MD5 列表)。曲目直接按
//! `Songs\<folder>\<难度>.osu` 完整路径播放 —— 目录本身就是解包形态,
//! 无需物化复制。
//!
//! 数据目录检测:注册表 `HKCU\Software\osu!` 的 `path` 值 >
//! `%LOCALAPPDATA%\osu!` > 各固定盘根下的 `osu!` 目录;以 `osu!.db`
//! 存在为准。

use crate::lazer::{is_sb_video_name, LazerBeatmap, LazerCollection, LazerFile, LazerLibrary, LazerSet};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 用户手动指定的 stable 安装目录;None = 自动检测。
static CUSTOM_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// 设置(或清除,None = 回自动检测)手动 stable 目录;要求目录内有 osu!.db。
pub fn set_custom_stable_dir(dir: Option<&Path>) -> Result<(), String> {
    let mut slot = CUSTOM_DIR.lock().map_err(|_| "配置锁中毒".to_string())?;
    *slot = None;
    if let Some(dir) = dir {
        if !dir.join("osu!.db").is_file() {
            return Err(format!("所选目录中没有 osu!.db:{}", dir.display()));
        }
        *slot = Some(dir.to_path_buf());
    }
    Ok(())
}

pub fn custom_stable_dir() -> Option<PathBuf> {
    CUSTOM_DIR.lock().ok().and_then(|s| s.clone())
}

/// stable 安装根目录(含 osu!.db 与 Songs/);手动指定优先。
pub fn stable_root() -> Option<PathBuf> {
    if let Some(dir) = custom_stable_dir() {
        return dir.join("osu!.db").is_file().then_some(dir);
    }
    stable_root_auto()
}

fn stable_root_auto() -> Option<PathBuf> {
    // 注册表 path(安装器写入;卸载残留只有 UninstallID 时不算)
    if let Ok(key) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey_with_flags("Software\\osu!", winreg::enums::KEY_QUERY_VALUE)
    {
        if let Ok(path) = key.get_value::<String, _>("path") {
            let p = PathBuf::from(&path);
            if p.join("osu!.db").is_file() {
                return Some(p);
            }
        }
    }
    // 默认安装位置
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let p = PathBuf::from(local).join("osu!");
        if p.join("osu!.db").is_file() {
            return Some(p);
        }
    }
    // 各固定盘根的 osu! 目录(自定义安装的常见摆法)
    for letter in b'C'..=b'Z' {
        let p = PathBuf::from(format!("{}:\\osu!", letter as char));
        if p.join("osu!.db").is_file() {
            return Some(p);
        }
    }
    None
}

/// 无 Mod 星级(osu!.db 星级表按 ModSet 位掩码存,0 = NM)。
fn nomod_star(ratings: &[(osu_db::ModSet, f64)]) -> f64 {
    ratings
        .iter()
        .find(|(m, _)| m.0 == 0)
        .map(|(_, r)| *r)
        .unwrap_or(0.0)
}

/// BPM:未继承(green)timing point 在 osu!.db 里 ms_per_beat 为正
/// (继承点为负的百分比),BPM = 60000 / ms;多段取最大值。
fn bpm_max(timing_points: &[osu_db::listing::TimingPoint]) -> f64 {
    timing_points
        .iter()
        .filter(|tp| tp.bpm > 0.0)
        .map(|tp| 60_000.0 / tp.bpm)
        .fold(0.0, f64::max)
}

/// 解析 stable 曲库:osu!.db 全量 → 按文件夹聚合谱面集(只保留 osu!
/// standard 难度),folder 目录必须真实存在;.osu 文件按 sha2 字段携带
/// 文件名(md5 照实填充,收藏夹关联用)。collection.db 收藏夹与 lazer
/// 同构(name + md5 序列)。
pub fn library(root: &Path) -> Result<LazerLibrary, String> {
    let started = std::time::Instant::now();
    let songs = root.join("Songs");
    let listing = osu_db::listing::Listing::from_file(root.join("osu!.db"))
        .map_err(|e| format!("解析 osu!.db 失败：{e}"))?;

    // 文件夹聚合(osu!.db 顺序即导入顺序,同文件夹难度保持库内顺序)
    struct Acc {
        artist: String,
        artist_unicode: String,
        title: String,
        title_unicode: String,
        creator: String,
        online_id: i64,
        tags: String,
        source: String,
        beatmaps: Vec<LazerBeatmap>,
    }
    let mut folders: HashMap<String, Acc> = HashMap::new();
    let mut order: Vec<String> = Vec::new(); // 首次出现的文件夹序,保住曲库排序
    for b in &listing.beatmaps {
        if b.mode != osu_db::Mode::Standard {
            continue;
        }
        let (Some(folder), Some(file)) = (&b.folder_name, &b.file_name) else { continue };
        let acc = folders.entry(folder.clone()).or_insert_with(|| {
            order.push(folder.clone());
            Acc {
                artist: b.artist_ascii.clone().unwrap_or_default(),
                artist_unicode: b.artist_unicode.clone().unwrap_or_default(),
                title: b.title_ascii.clone().unwrap_or_default(),
                title_unicode: b.title_unicode.clone().unwrap_or_default(),
                creator: b.creator.clone().unwrap_or_default(),
                online_id: b.beatmapset_id as i64,
                tags: b.tags.clone().unwrap_or_default(),
                source: b.song_source.clone().unwrap_or_default(),
                beatmaps: Vec::new(),
            }
        });
        // 上架编号只信非零值:首难度的 beatmapset_id 可能为 0(未上架
        // 本地图),后续难度带合法 ID 时补上,排序键不会恒卡在 0
        if acc.online_id <= 0 && b.beatmapset_id > 0 {
            acc.online_id = b.beatmapset_id as i64;
        }
        acc.beatmaps.push(LazerBeatmap {
            sha2: file.clone(), // stable:难度 .osu 文件名(集合内唯一)
            md5: b.hash.clone().unwrap_or_default(),
            name: b.difficulty_name.clone().unwrap_or_default(),
            ruleset: "osu".to_string(),
            star_rating: nomod_star(&b.std_ratings),
            length_ms: b.total_time as f64,
            ar: b.approach_rate as f64,
            cs: b.circle_size as f64,
            od: b.overall_difficulty as f64,
            hp: b.hp_drain as f64,
            bpm: bpm_max(&b.timing_points),
            online_id: b.beatmap_id as i64,
        });
    }

    // 组装谱面集:目录必须存在,图片文件列出供封面挑选(并行 stat)
    let mut sets: Vec<LazerSet> = Vec::with_capacity(order.len());
    let mut missing = 0usize;
    {
        let mut prepared: Vec<(String, Acc, PathBuf)> = Vec::new();
        for folder in order {
            if let Some(acc) = folders.remove(&folder) {
                let dir = songs.join(&folder);
                if !dir.is_dir() || acc.beatmaps.is_empty() {
                    missing += 1;
                    continue;
                }
                prepared.push((folder, acc, dir));
            }
        }
        let chunk = prepared.len().div_ceil(
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, 8),
        );
        let threads = std::thread::scope(|scope| {
            let handles: Vec<_> = prepared
                .chunks_mut(chunk)
                .map(|chunk| {
                    scope.spawn(|| {
                        let mut out = Vec::new();
                        for (folder, acc, dir) in chunk.iter_mut() {
                            let mut files = Vec::new();
                            // SB/视频标记与图片清单一趟判定(同一次 read_dir,
                            // 零额外 I/O)
                            let mut sb_video = false;
                            if let Ok(entries) = std::fs::read_dir(&*dir) {
                                for e in entries.flatten() {
                                    let name = e.file_name().to_string_lossy().into_owned();
                                    let lower = name.to_ascii_lowercase();
                                    if [".jpg", ".jpeg", ".png", ".webp"].iter().any(|x| lower.ends_with(x)) {
                                        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                                        if size > 0 {
                                            files.push(LazerFile { filename: name, hash: String::new(), size });
                                        }
                                    } else if is_sb_video_name(&name) {
                                        sb_video = true;
                                    }
                                }
                            }
                            // stable 无导入时间记录,用谱面集目录 mtime 近似
                            let date_added_ms = std::fs::metadata(&*dir)
                                .and_then(|m| m.modified())
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_millis() as i64)
                                .unwrap_or(0);
                            out.push(LazerSet {
                                id: folder.clone(),
                                online_id: acc.online_id,
                                date_added_ms,
                                artist: std::mem::take(&mut acc.artist),
                                artist_unicode: std::mem::take(&mut acc.artist_unicode),
                                title: std::mem::take(&mut acc.title),
                                title_unicode: std::mem::take(&mut acc.title_unicode),
                                creator: std::mem::take(&mut acc.creator),
                                tags: std::mem::take(&mut acc.tags),
                                source: std::mem::take(&mut acc.source),
                                root: Some(dir.to_string_lossy().into_owned()),
                                sb_video,
                                beatmaps: std::mem::take(&mut acc.beatmaps),
                                files,
                            });
                        }
                        out
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_default()).collect::<Vec<Vec<LazerSet>>>()
        });
        for part in threads {
            sets.extend(part);
        }
    }

    // collection.db 收藏夹(name + md5 序列,与 lazer 同构;去重)
    let mut collections: Vec<LazerCollection> = Vec::new();
    if let Ok(list) = osu_db::collection::CollectionList::from_file(root.join("collection.db")) {
        for c in list.collections {
            let name = c.name.unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let mut md5s: Vec<String> = Vec::new();
            for hash in c.beatmap_hashes.iter().flatten() {
                if !md5s.contains(hash) {
                    md5s.push(hash.clone());
                }
            }
            collections.push(LazerCollection { name, md5s });
        }
    }
    collections.sort_by_key(|c| !c.name.eq_ignore_ascii_case("favourites"));

    log::info!(
        "stable 曲库:{} 个谱面集({} 个目录缺失)/ {} 个收藏夹({:.2}s)",
        sets.len(),
        missing,
        collections.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(LazerLibrary { sets, collections, skins: Vec::new() })
}

/// stable 谱面集封面:目录里最大的图片(与 lazer 封面挑选同策略)。
pub fn cover_data_url(set: &LazerSet) -> Option<String> {
    use base64::Engine as _;
    let dir = PathBuf::from(set.root.as_deref()?);
    let best = set
        .files
        .iter()
        .filter(|f| f.size > 0)
        .max_by_key(|f| f.size)?;
    let bytes = std::fs::read(dir.join(&best.filename)).ok()?;
    let mime = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg"
    } else if bytes.starts_with(b"RIFF") && bytes.len() > 11 && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        return None;
    };
    Some(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_real_stable() {
        let Some(root) = stable_root() else {
            eprintln!("本机无 osu!stable,跳过");
            return;
        };
        eprintln!("stable: {}", root.display());
        let lib = library(&root).expect("解析失败");
        assert!(!lib.sets.is_empty(), "谱面集为空");
        assert!(lib.sets.iter().all(|s| s.root.is_some()), "root 缺失");
        // 难度 .osu 必须真实存在
        for s in lib.sets.iter().take(50) {
            let dir = PathBuf::from(s.root.as_deref().unwrap());
            for b in &s.beatmaps {
                assert!(dir.join(&b.sha2).is_file(), "缺 .osu: {}", b.sha2);
            }
        }
        let starred = lib.sets.iter().filter(|s| s.beatmaps.iter().any(|b| b.star_rating > 0.0)).count();
        eprintln!("含星级谱面集: {starred}/{} 收藏夹: {}", lib.sets.len(), lib.collections.len());
        // 封面抽查
        let with_cover = lib.sets.iter().find(|s| !s.files.is_empty()).unwrap();
        assert!(cover_data_url(with_cover).is_some(), "封面读取失败");
    }
}

#[cfg(test)]
mod missing_diag {
    use super::*;

    /// 诊断:难度参数(AR/CS/OD/HP/BPM)与 tags 是否从 osu!.db 读出
    /// (Information 卡显示的数据来源)。
    #[test]
    fn dump_stable_params() {
        let Some(root) = stable_root() else { return };
        let lib = library(&root).expect("parse");
        let with_ar = lib.sets.iter().filter(|s| s.beatmaps.iter().any(|b| b.ar > 0.0)).count();
        let with_tags = lib.sets.iter().filter(|s| !s.tags.is_empty()).count();
        eprintln!("=== 含 AR: {with_ar}/{}  含 tags: {with_tags}", lib.sets.len());
        if let Some(set) = lib
            .sets
            .iter()
            .find(|s| !s.tags.is_empty() && s.beatmaps.iter().any(|b| b.ar > 0.0 && b.bpm > 0.0))
        {
            let b = &set.beatmaps[0];
            eprintln!(
                "    {} | {} | ★{:.2} AR{} CS{} OD{} HP{} BPM{} bid={} | tags={}",
                set.title, b.name, b.star_rating, b.ar, b.cs, b.od, b.hp, b.bpm, b.online_id, set.tags
            );
            let with_bid = lib
                .sets
                .iter()
                .filter(|s| s.beatmaps.iter().any(|b| b.online_id > 0))
                .count();
            eprintln!("=== 含 bid 的谱面集: {with_bid}/{}", lib.sets.len());
        }
    }
    /// 全量诊断"部分谱面找不到文件夹/文件":目录存在性、.osu 存在性、
    /// .osu [Events] 声明的 Video 文件存在性。
    #[test]
    fn find_missing_files() {
        let Some(root) = stable_root() else { return };
        let lib = library(&root).expect("parse");
        let mut bad_folder = 0usize;
        let mut bad_osu: Vec<String> = Vec::new();
        let mut bad_video: Vec<String> = Vec::new();
        for set in &lib.sets {
            let Some(r) = &set.root else { continue };
            let dir = PathBuf::from(r);
            if !dir.is_dir() {
                bad_folder += 1;
                continue;
            }
            for b in &set.beatmaps {
                let osu = dir.join(&b.sha2);
                if !osu.is_file() {
                    bad_osu.push(format!("{} | {}", set.title, b.sha2));
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(&osu) {
                    for line in text.lines() {
                        let line = line.trim();
                        if let Some(rest) = line.strip_prefix("Video,") {
                            let name = rest.splitn(3, ',').nth(2).unwrap_or("").trim().trim_matches('"');
                            if !name.is_empty() && !dir.join(name).is_file() {
                                bad_video.push(format!("{} | {}", set.title, name));
                            }
                        }
                    }
                }
            }
        }
        eprintln!("=== 坏目录: {bad_folder}");
        eprintln!("=== 缺 .osu: {}", bad_osu.len());
        for x in bad_osu.iter().take(8) { eprintln!("    {x}") }
        eprintln!("=== 缺视频文件: {}", bad_video.len());
        for x in bad_video.iter().take(8) { eprintln!("    {x}") }
    }
}
