//! 谱面资源路径缓存(set_id → 各难度 .osu 路径 + 谱面集文件清单)。
//! 让播放列表完全自持:托盘切歌/启动恢复直接下发 Load 命令,**不碰
//! realm / osu!.db** —— 数据库只在真正需要时(打开曲库、缓存失效)
//! 才解析。路径失效(地图被删、lazer 迁移存储)时调用方回退慢路径
//! 重新解析并自动重建缓存条目。
//!
//! 存储为 appData/path-cache.json:按 set_id 一条(谱面集内各难度共享
//! 一份 manifest,不随播放列表条目重复存储);上限条数防无限增长。

use crate::ipc::VFile;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 单个难度的离线播放信息(.osu 路径 + 显示元数据)。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct CachedDiff {
    pub sha2: String,
    pub name: String,
    pub star: f64,
    pub length_ms: f64,
    /// 该难度的 .osu 完整路径(lazer blob / stable 目录内文件)。
    pub osu_path: String,
}

/// 一个谱面集的离线播放信息。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct CachedSet {
    pub diffs: Vec<CachedDiff>,
    /// lazer:文件名 → blob 路径清单(与 Load 命令同口径,worker 按
    /// 名解析音频/背景/storyboard);stable:None(目录即形态)。
    pub manifest: Option<Vec<VFile>>,
}

/// 条目上限(按插入序淘汰最旧的;播放过的谱面集数量级远小于此)。
const MAX_SETS: usize = 400;

type CacheMap = HashMap<String, CachedSet>;

fn cache_file(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    use tauri::Manager;
    app.path().app_data_dir().ok().map(|d| d.join("path-cache.json"))
}

fn load_map(app: &tauri::AppHandle) -> CacheMap {
    let Some(file) = cache_file(app) else { return CacheMap::new() };
    std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn store_map(app: &tauri::AppHandle, map: &CacheMap) {
    if let Some(file) = cache_file(app) {
        if let Ok(json) = serde_json::to_string(map) {
            let _ = std::fs::create_dir_all(file.parent().unwrap_or(std::path::Path::new("")));
            let _ = std::fs::write(file, json);
        }
    }
}

/// 取一个谱面集的缓存条目。
pub fn get(app: &tauri::AppHandle, set_id: &str) -> Option<CachedSet> {
    load_map(app).get(set_id).cloned()
}

/// 写入/覆盖一个条目(超出上限淘汰最旧)。
pub fn put(app: &tauri::AppHandle, set_id: &str, set: CachedSet) {
    let mut map = load_map(app);
    if map.len() >= MAX_SETS && !map.contains_key(set_id) {
        if let Some(oldest) = map.keys().next().cloned() {
            map.remove(&oldest);
        }
    }
    map.insert(set_id.to_string(), set);
    store_map(app, &map);
}

/// 删除条目(路径失效时调用,慢路径会重建)。
pub fn remove(app: &tauri::AppHandle, set_id: &str) {
    let mut map = load_map(app);
    if map.remove(set_id).is_some() {
        store_map(app, &map);
    }
}
