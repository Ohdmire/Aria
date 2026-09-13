//! 谱面输入解析:.osz 解包 / .osu → 供 autoplay 渲染的谱面路径。
//!
//! .osz 解包逻辑移植自 osu-storyboard-render CLI,按 Aria 的需要
//! 拆成 Probe(枚举难度)与 Load(按名选难度)两步;解包目录按谱面路径
//! 哈希稳定命名并缓存,重复加载不重复解压。

use anyhow::{bail, Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// 一个输入谱面的解析结果(.osz 解包后 / 单文件)。
pub struct BeatmapInput {
    /// 素材根目录(音频/背景/皮肤素材从这里找)。
    pub root: PathBuf,
    /// 输入 .osu(纯 .osb 输入时为 .osb 路径,is_osb = true)。
    pub input: PathBuf,
    pub is_osb: bool,
    /// 可选难度列表(.osz 内 .osu 的相对路径,按 zip 内顺序)。
    pub diffs: Vec<String>,
}

/// Load 的产物:选中的谱面 .osu 路径。
pub struct LoadedWall {
    pub map_path: PathBuf,
    pub diff: Option<String>,
}

/// 按输入路径缓存解包结果。
pub struct Library {
    cache: HashMap<String, BeatmapInput>,
}

impl Library {
    pub fn new() -> Library {
        Library { cache: HashMap::new() }
    }

    /// 解析输入(必要时解包 .osz),结果缓存。
    pub fn probe(&mut self, path: &str) -> Result<&BeatmapInput> {
        if !self.cache.contains_key(path) {
            let input = resolve(Path::new(path)).with_context(|| format!("解析 {path}"))?;
            self.cache.insert(path.to_string(), input);
        }
        Ok(&self.cache[path])
    }

    /// 选中难度。diff 先按文件名精确匹配(不区分大小写),失败回退唯一
    /// 子串匹配(与 CLI --diff 一致)。
    pub fn load(&mut self, path: &str, diff: Option<&str>) -> Result<LoadedWall> {
        let input = self.probe(path)?;
        if input.is_osb {
            bail!("播放模式需要 .osz / .osu(纯 .osb 没有可游玩内容)");
        }
        let picked = pick_diff(&input.diffs, diff)?;
        // 单 .osu 文件输入(曲库物化/手动选文件)没有难度列表,文件即输入
        let (map_path, chosen) = match picked.or_else(|| input.diffs.first().cloned()) {
            Some(name) => (input.root.join(&name), Some(name)),
            None => (input.input.clone(), None),
        };
        Ok(LoadedWall { map_path, diff: chosen })
    }
}

/// 难度名 → zip 相对路径的挑选;返回匹配项(无 diffs 时为 None)。
fn pick_diff(diffs: &[String], diff: Option<&str>) -> Result<Option<String>> {
    let Some(want) = diff else { return Ok(diffs.first().cloned()) };
    if want.is_empty() {
        return Ok(diffs.first().cloned());
    }
    let lower = want.to_lowercase();
    if let Some(hit) = diffs.iter().find(|d| d.to_lowercase() == lower) {
        return Ok(Some(hit.clone()));
    }
    let hits: Vec<&String> = diffs
        .iter()
        .filter(|d| d.to_lowercase().contains(&lower))
        .collect();
    match hits.len() {
        0 => bail!("未找到难度 “{want}”,可用难度: {}", diffs.join(" / ")),
        1 => Ok(Some(hits[0].clone())),
        _ => bail!("难度 “{want}” 匹配到多个文件,请用更长的名称"),
    }
}

fn resolve(path: &Path) -> Result<BeatmapInput> {
    if !path.is_file() {
        bail!("文件不存在: {}", path.display());
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext != "osz" {
        let root = path
            .parent()
            .map(|p| p.to_path_buf())
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".").to_path_buf());
        return Ok(BeatmapInput {
            root,
            input: path.to_path_buf(),
            is_osb: ext == "osb",
            diffs: Vec::new(),
        });
    }
    unpack_osz(path)
}

/// .osz 解包到 temp/aria/<路径哈希>/,已存在且同大小的文件跳过。
fn unpack_osz(path: &Path) -> Result<BeatmapInput> {
    let mut hasher = DefaultHasher::new();
    path.to_string_lossy().hash(&mut hasher);
    let root = std::env::temp_dir()
        .join("aria")
        .join(format!("{:016x}", hasher.finish()));
    std::fs::create_dir_all(&root)?;

    let file = std::fs::File::open(path).with_context(|| format!("打开 {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("读取 .osz 压缩包 {}", path.display()))?;

    let mut osb: Option<String> = None;
    let mut osus: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        // zip-slip 防护
        let name = entry.name().to_string();
        if name.split(['/', '\\']).any(|seg| seg == "..") {
            continue;
        }
        let out_path = root.join(&name);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // 幂等:同大小视为已解包,跳过覆盖(大包重复加载提速)
        let skip = std::fs::metadata(&out_path).map(|m| m.len() == entry.size()).unwrap_or(false);
        if !skip {
            let mut out = std::fs::File::create(&out_path)?;
            std::io::copy(&mut entry, &mut out)?;
        }
        let lower = name.to_lowercase();
        if lower.ends_with(".osb") {
            osb.get_or_insert(name);
        } else if lower.ends_with(".osu") {
            osus.push(name);
        }
    }

    if !osus.is_empty() {
        let input = root.join(&osus[0]);
        Ok(BeatmapInput { root, input, is_osb: false, diffs: osus })
    } else if let Some(osb) = osb {
        let input = root.join(osb);
        Ok(BeatmapInput { root, input, is_osb: true, diffs: Vec::new() })
    } else {
        bail!("{} 中未找到 .osb/.osu 文件", path.display());
    }
}
