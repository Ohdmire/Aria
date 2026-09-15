//! osu!lazer 曲库:只读解析 client.realm(谱面集 + 收藏夹)。
//! 点播零拷贝:谱面集文件名 → files/ 内容寻址库 blob 实际路径的映射表
//! (manifest)随加载命令直传渲染端,由渲染端按名解析,全程不复制。
//!
//! realm 解析:表加载策略 / RealmNamedFileUsage 解引用 / 收藏夹 md5 →
//! 谱面映射,仅保留 Aria 需要的部分。
//! 数据目录检测:%APPDATA%\osu\storage.ini 的 FullPath 优先(本机 lazer
//! 迁移存储后 client.realm 与 files/ 都在该目录)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use realm_db_reader::{Group, Link, Realm, Row, Value};
use serde::Serialize;

/// 行数不超过该阈值的表整表全载;超过的表按行懒加载。
const BULK_ROW_LIMIT: usize = 50_000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LazerFile {
    pub filename: String,
    pub hash: String,
    /// blob 实际大小(解析后 stat;0 = blob 缺失,已过滤)。
    pub size: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LazerBeatmap {
    pub sha2: String,
    pub md5: String,
    /// 难度名(DifficultyName)。
    pub name: String,
    /// 模式短名(osu/taiko/fruits/mania)。
    pub ruleset: String,
    pub star_rating: f64,
    pub length_ms: f64,
    /// 难度参数(class_BeatmapDifficulty;0 = realm 未记录,UI 隐藏)。
    pub ar: f64,
    pub cs: f64,
    pub od: f64,
    pub hp: f64,
    /// BPM(lazer 导入时算好的单值;stable 取未继承 timing point 的最大值)。
    pub bpm: f64,
    /// 在线难度 id(class_Beatmap.OnlineID,未同步的本地谱为 -1/0)。
    pub online_id: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LazerSet {
    pub id: String,
    pub online_id: i64,
    /// 导入 lazer 的时间(class_BeatmapSet.DateAdded,Unix ms;0 = 未记录)。
    /// stable 源用谱面集目录 mtime 近似。曲库默认按此降序(最新导入在前)。
    pub date_added_ms: i64,
    pub artist: String,
    pub artist_unicode: String,
    pub title: String,
    pub title_unicode: String,
    pub creator: String,
    /// 谱面标签(空格分隔;来自 BeatmapMetadata.Tags)。
    pub tags: String,
    /// 来源(BeatmapMetadata.Source,常为空)。
    pub source: String,
    /// stable 源专用:谱面集目录完整路径(目录即物化,直接播放);
    /// lazer 源恒 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// 简单判定:谱面集带 storyboard(.osb)或视频文件(只看文件名
    /// 扩展名,不解析 .osu,内嵌 storyboard 检出不到)。曲库过滤用。
    pub sb_video: bool,
    pub beatmaps: Vec<LazerBeatmap>,
    pub files: Vec<LazerFile>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LazerCollection {
    pub name: String,
    /// 收藏顺序的难度 md5 列表(已去重)。
    pub md5s: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LazerLibrary {
    pub sets: Vec<LazerSet>,
    pub collections: Vec<LazerCollection>,
    /// 已安装皮肤(class_Skin,含文件清单;内置皮肤无文件列表,不含)。
    pub skins: Vec<LazerSkin>,
}

/// lazer 已安装皮肤(realm class_Skin):名称 + 文件清单(文件名 →
/// files/ blob)。选中时物化挂载到缓存目录。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LazerSkin {
    pub id: String,
    pub name: String,
    pub creator: String,
    pub files: Vec<LazerFile>,
}

// ---------- 数据目录检测 ----------

/// 用户手动指定的 lazer 数据目录(基础目录,storage.ini FullPath 仍会
/// 解析);None = 自动检测。
static CUSTOM_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// 设置(或清除,None = 回自动检测)手动数据目录。所选目录可直接含
/// client.realm,也可只含 storage.ini(由 FullPath 重定向);按解析后的
/// 实际位置校验,不含 client.realm 则回滚报错。
pub fn set_custom_data_dir(dir: Option<&Path>) -> Result<(), String> {
    let mut slot = CUSTOM_DIR.lock().map_err(|_| "配置锁中毒".to_string())?;
    *slot = None;
    if let Some(dir) = dir {
        let root =
            read_storage_ini_fullpath(&dir.join("storage.ini")).unwrap_or_else(|| dir.to_path_buf());
        if !root.join("client.realm").is_file() {
            return Err(format!("解析后的目录中没有 client.realm:{}", root.display()));
        }
        *slot = Some(dir.to_path_buf());
    }
    Ok(())
}

pub fn custom_data_dir() -> Option<PathBuf> {
    CUSTOM_DIR.lock().ok().and_then(|s| s.clone())
}

/// osu!lazer 数据根:手动指定优先;storage.ini 的 FullPath 无条件优先。
pub fn data_root() -> Option<PathBuf> {
    let base = custom_data_dir().or_else(|| {
        std::env::var_os("APPDATA").map(PathBuf::from).map(|appdata| appdata.join("osu"))
    })?;
    let root = read_storage_ini_fullpath(&base.join("storage.ini")).unwrap_or(base);
    root.join("client.realm").is_file().then_some(root)
}

pub fn realm_path() -> Option<PathBuf> {
    data_root().map(|root| root.join("client.realm"))
}

fn read_storage_ini_fullpath(storage_ini: &Path) -> Option<PathBuf> {
    use std::io::BufRead;
    for line in std::io::BufReader::new(std::fs::File::open(storage_ini).ok()?).lines().flatten() {
        if let Some(value) = line.strip_prefix("FullPath").and_then(|rest| rest.split('=').nth(1)) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }
    None
}

/// files/ 内容寻址库中的 blob 相对路径:<首字符>/<前两字符>/<hash>。
pub(crate) fn blob_relative_path(hash: &str) -> String {
    if hash.len() >= 2 {
        format!("{}/{}/{}", hash[..1].to_ascii_lowercase(), hash[..2].to_ascii_lowercase(), hash)
    } else {
        hash.to_string()
    }
}

// ---------- realm 解析 ----------

/// 顺序流式读一遍文件,把内容喂进 OS 页缓存(读后即弃)。随机 mmap
/// 单行访问冷启动时逐页硬缺页,预热后全部命中内存。
fn prewarm_pages(path: &Path) {
    let t = std::time::Instant::now();
    let Ok(mut file) = std::fs::File::open(path) else { return };
    use std::io::Read;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0usize;
    while let Ok(n) = file.read(&mut buf) {
        if n == 0 {
            break;
        }
        total += n;
    }
    log::info!("realm 预热:{:.1}MB 顺序读({:.0}ms)", total as f64 / 1048576.0, t.elapsed().as_millis());
}

/// 直接只读解析 client.realm:并发读内存映射是安全的,lazer 正在写时
/// 最多读到写入中间态(行级解析对坏行容忍,极少见的解析失败重试即可)。
pub fn parse(realm: &Path) -> Result<LazerLibrary, String> {
    log::info!("解析 realm: {}", realm.display());
    let started = std::time::Instant::now();
    // 顺序预热:File/NamedFileUsage 两张大表走逐行懒加载,合计约 10 万次
    // 随机 mmap 缺页 —— 冷启动(页缓存空、系统忙)时单页可达毫秒级,
    // 实测行解析阶段能拖到 20s+;先把整个文件流式读一遍(几十 MB 顺序
    // 读 <100ms),后续随机访问全部命中 OS 页缓存,冷热表现一致。
    prewarm_pages(realm);
    let opened = Realm::open(realm).map_err(|e| format!("打开 client.realm 失败：{e}"))?;
    let group = opened
        .into_group()
        .map_err(|e| format!("读取 Realm 组失败：{e}"))?;
    let mut store = RowStore::new(&group);
    let mut library = LazerLibrary::default();
    parse_beatmap_sets(&mut store, &mut library)?;
    log::info!("realm 谱面集:{} 个({:.2}s)", library.sets.len(), started.elapsed().as_secs_f64());
    parse_collections(&mut store, &mut library)?;
    log::info!("realm 收藏夹:{} 个(累计 {:.2}s)", library.collections.len(), started.elapsed().as_secs_f64());
    parse_skins(&mut store, &mut library)?;
    log::info!("realm 皮肤:{} 个(累计 {:.2}s)", library.skins.len(), started.elapsed().as_secs_f64());
    // 文件存在性过滤:曲库里留下
    // 的每个谱面集都必须可物化播放,封面也直接用记录的大小。
    let root = realm.parent().unwrap_or(Path::new(""));
    retain_playable(&mut library, root);
    // SB/视频标记:在 retain 过滤后的文件清单上判定(blob 缺失的
    // .osb/视频本来也播不了),纯内存一趟,开销可忽略
    for set in &mut library.sets {
        set.sb_video = set.files.iter().any(|f| is_sb_video_name(&f.filename));
    }
    Ok(library)
}

/// stat 每个 set 文件在 files/ 中的 blob:缺失条目剔除(内容寻址,hash
/// 在即内容在);文件列表或难度 blob 缺失的谱面集整体剔除。files/ 目录
/// 不存在(异常布局)时跳过,保留原始解析结果。约 4 万次 stat 串行在
/// 实测盘上要 20s+(首触扫描),按 CPU 数并行压到秒级。
fn retain_playable(library: &mut LazerLibrary, root: &Path) {
    let files_root = root.join("files");
    if !files_root.is_dir() {
        log::warn!("{} 下没有 files/ 目录,跳过文件存在性过滤", files_root.display());
        return;
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 8);
    let chunk_size = library.sets.len().div_ceil(threads);
    let started = std::time::Instant::now();
    std::thread::scope(|scope| {
        for chunk in library.sets.chunks_mut(chunk_size) {
            scope.spawn(|| {
                for set in chunk {
                    for file in &mut set.files {
                        file.size = std::fs::metadata(files_root.join(blob_relative_path(&file.hash)))
                            .map(|m| m.len())
                            .unwrap_or(0);
                    }
                    set.files.retain(|f| f.size > 0);
                    set.beatmaps.retain(|b| set.files.iter().any(|f| f.hash == b.sha2));
                }
            });
        }
    });
    let before = library.sets.len();
    library.sets.retain(|s| !s.files.is_empty() && !s.beatmaps.is_empty());
    log::info!(
        "曲库过滤:{before} → {} 个可播放谱面集({} 个线程,{:.2}s)",
        library.sets.len(),
        threads,
        started.elapsed().as_secs_f64()
    );
}

fn parse_beatmap_sets(store: &mut RowStore<'_>, library: &mut LazerLibrary) -> Result<(), String> {
    for row in store.bulk_rows("class_BeatmapSet")? {
        if matches!(row.get("DeletePending"), Some(Value::Bool(true))) {
            continue;
        }
        let files = resolve_named_files(store, row.get("Files"))?;
        let mut beatmaps = Vec::new();
        if let Some(Value::LinkList(links)) = row.get("Beatmaps") {
            for link in links {
                let Some(beatmap) = store.row(link)? else { continue };
                let sha2 = string_value(beatmap.get("Hash"));
                if sha2.is_empty() || !files.iter().any(|file| file.hash == sha2) {
                    continue;
                }
                // 只保留 osu! 模式:autoplay 渲染器(osu-replay-render)仅支持
                // osu!standard,其他模式点击必失败 —— 直接从曲库剔除
                if ruleset_short_name(store, beatmap.get("Ruleset")) != "osu" {
                    continue;
                }
                // 难度参数(class_BeatmapDifficulty 链接行;Information 卡显示)
                let (ar, cs, od, hp) = match beatmap.get("Difficulty") {
                    Some(Value::Link(link)) => match store.row(&link)? {
                        Some(diff) => (
                            float_value(diff.get("ApproachRate")),
                            float_value(diff.get("CircleSize")),
                            float_value(diff.get("OverallDifficulty")),
                            float_value(diff.get("DrainRate")),
                        ),
                        None => (0.0, 0.0, 0.0, 0.0),
                    },
                    _ => (0.0, 0.0, 0.0, 0.0),
                };
                beatmaps.push(LazerBeatmap {
                    sha2,
                    md5: string_value(beatmap.get("MD5Hash")),
                    name: string_value(beatmap.get("DifficultyName")),
                    ruleset: ruleset_short_name(store, beatmap.get("Ruleset")),
                    star_rating: double_value(beatmap.get("StarRating"), 0.0),
                    length_ms: double_value(beatmap.get("Length"), 0.0),
                    ar,
                    cs,
                    od,
                    hp,
                    bpm: double_value(beatmap.get("BPM"), 0.0),
                    online_id: int_value(beatmap.get("OnlineID"), -1),
                });
            }
        }
        if beatmaps.is_empty() {
            continue;
        }
        // 元数据取第一张难度关联的 BeatmapMetadata(与 lazer 一致)。
        let metadata = row
            .get("Beatmaps")
            .and_then(first_link)
            .and_then(|link| store.row(link).ok().flatten())
            .and_then(|beatmap| match beatmap.get("Metadata") {
                Some(Value::Link(link)) => Some(link.clone()),
                _ => None,
            })
            .and_then(|link| store.row(&link).ok().flatten());
        let (artist, artist_unicode, title, title_unicode, creator, tags, source) = match metadata {
            Some(metadata) => {
                let artist = string_value(metadata.get("Artist"));
                let title = string_value(metadata.get("Title"));
                (
                    artist.clone(),
                    non_empty_or(string_value(metadata.get("ArtistUnicode")), artist),
                    title.clone(),
                    non_empty_or(string_value(metadata.get("TitleUnicode")), title),
                    match metadata.get("Author") {
                        Some(Value::Link(user_link)) => store
                            .row(user_link)
                            .ok()
                            .flatten()
                            .map(|user| string_value(user.get("Username")))
                            .unwrap_or_default(),
                        _ => String::new(),
                    },
                    string_value(metadata.get("Tags")),
                    string_value(metadata.get("Source")),
                )
            }
            None => (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ),
        };
        library.sets.push(LazerSet {
            id: uuid_string(row.get("ID")),
            online_id: int_value(row.get("OnlineID"), -1),
            date_added_ms: timestamp_ms(row.get("DateAdded")),
            artist,
            artist_unicode,
            title,
            title_unicode,
            creator,
            tags,
            source,
            root: None,
            sb_video: false,
            beatmaps,
            files,
        });
    }
    Ok(())
}

fn parse_collections(store: &mut RowStore<'_>, library: &mut LazerLibrary) -> Result<(), String> {
    for row in store.bulk_rows("class_BeatmapCollection")? {
        let name = string_value(row.get("Name"));
        if name.is_empty() {
            continue;
        }
        let mut md5s: Vec<String> = Vec::new();
        if let Some(Value::List(values)) = row.get("BeatmapMD5Hashes") {
            for value in values {
                if let Value::String(md5) = value {
                    // 同一难度可能被收藏多次,去重
                    if !md5s.contains(md5) {
                        md5s.push(md5.clone());
                    }
                }
            }
        }
        library.collections.push(LazerCollection { name, md5s });
    }
    // lazer 的 "Favourites" 收藏夹固定置顶
    library.collections.sort_by_key(|c| !c.name.eq_ignore_ascii_case("favourites"));
    Ok(())
}

/// 解析已安装皮肤(class_Skin):跳过 DeletePending;无 skin.ini
/// 的(内置 Argon/Triangles 等,无文件列表)不含。文件清单指向 files/
/// 内容寻址 blob,选中时物化挂载。
fn parse_skins(store: &mut RowStore<'_>, library: &mut LazerLibrary) -> Result<(), String> {
    for row in store.bulk_rows("class_Skin")? {
        if matches!(row.get("DeletePending"), Some(Value::Bool(true))) {
            continue;
        }
        let files = resolve_named_files(store, row.get("Files"))?;
        if !files.iter().any(|f| f.filename.eq_ignore_ascii_case("skin.ini")) {
            continue; // 内置皮肤
        }
        library.skins.push(LazerSkin {
            id: uuid_string(row.get("ID")),
            name: string_value(row.get("Name")),
            creator: string_value(row.get("Creator")),
            files,
        });
    }
    library.skins.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(())
}

/// 文件名是否为 storyboard(.osb)或视频(扩展名大小写不敏感,
/// 不分配)。视频扩展名 = osu! 支持列表(ffmpeg 直读)。
pub(crate) fn is_sb_video_name(name: &str) -> bool {
    const SUFFIXES: [&[u8]; 8] = [b".osb", b".mp4", b".avi", b".flv", b".m4v", b".mov", b".webm", b".wmv"];
    let bytes = name.as_bytes();
    SUFFIXES.iter().any(|suf| {
        bytes.len() >= suf.len()
            && bytes[bytes.len() - suf.len()..]
                .iter()
                .zip(*suf)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// 文件名清洗(物化挂载用,同 zip 条目规则)。
pub(crate) fn sanitize_filename(name: &str) -> String {
    let name = name.replace('\\', "/");
    let mut out = String::with_capacity(name.len());
    for seg in name.split('/') {
        if seg == ".." || seg.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&seg.replace([':', '*', '?', '"', '<', '>', '|'], "_"));
    }
    out
}

/// 物化挂载 realm 皮肤:files/ blob → 缓存目录内的普通文件名(幂等,
/// 大小一致跳过)。之后渲染端按目录直接使用。
pub fn mount_skin(root: &Path, skin: &LazerSkin, cache: &Path) -> Result<PathBuf, String> {
    let dir_name = sanitize_filename(&skin.name);
    let dir = if dir_name.is_empty() { cache.to_path_buf() } else { cache.join(&dir_name) };
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建皮肤缓存目录失败:{e}"))?;
    for f in &skin.files {
        let blob = root.join("files").join(blob_relative_path(&f.hash));
        let target = dir.join(sanitize_filename(&f.filename));
        let cached = std::fs::metadata(&target)
            .map(|m| m.len() == std::fs::metadata(&blob).map(|b| b.len()).unwrap_or(u64::MAX))
            .unwrap_or(false);
        if cached {
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建皮肤子目录失败:{e}"))?;
        }
        std::fs::copy(&blob, &target)
            .map_err(|e| format!("挂载皮肤文件 {} 失败:{e}", f.filename))?;
    }
    Ok(dir)
}

/// 谱面集封面:文件列表里最大的图片(大小在解析时
/// 已 stat 进 LazerFile.size),读 blob 转 data URL(base64 + 嗅探 mime),
/// 供前端 <img> 直接使用。
pub fn cover_data_url(root: &Path, set: &LazerSet) -> Option<String> {
    use base64::Engine as _;
    let best = set
        .files
        .iter()
        .filter(|f| {
            let lower = f.filename.to_ascii_lowercase();
            [".jpg", ".jpeg", ".png", ".webp"].iter().any(|e| lower.ends_with(e))
        })
        .max_by_key(|f| f.size)?;
    if best.size == 0 {
        return None;
    }
    let bytes = std::fs::read(root.join("files").join(blob_relative_path(&best.hash))).ok()?;
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

// ---------- 谱面集点播(零拷贝) ----------

/// 谱面集难度 blob 的实际路径(点播的 .osu 直接传给渲染端)。
pub fn beatmap_blob(root: &Path, sha2: &str) -> PathBuf {
    root.join("files").join(blob_relative_path(sha2))
}

fn non_empty_or(value: String, fallback: String) -> String {
    if value.is_empty() { fallback } else { value }
}

fn first_link<'a>(value: &'a Value) -> Option<&'a Link> {
    match value {
        Value::LinkList(links) => links.first(),
        _ => None,
    }
}

fn string_value(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        _ => String::new(),
    }
}

fn int_value(value: Option<&Value>, fallback: i64) -> i64 {
    match value {
        Some(Value::Int(value)) => *value,
        _ => fallback,
    }
}

fn double_value(value: Option<&Value>, fallback: f64) -> f64 {
    match value {
        Some(Value::Double(value)) => *value,
        _ => fallback,
    }
}

/// realm 单精度列(AR/CS/OD/HP 存为 Float)。
fn float_value(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Float(value)) => *value as f64,
        _ => 0.0,
    }
}

/// realm 时间值 → Unix ms(realm Timestamp 为 chrono DateTime<Utc>)。
fn timestamp_ms(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Timestamp(t)) => t.timestamp_millis(),
        _ => 0,
    }
}

fn uuid_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::Uuid(bytes)) => bytes.iter().map(|b| format!("{b:02x}")).collect(),
        _ => String::new(),
    }
}

fn ruleset_short_name(store: &mut RowStore<'_>, value: Option<&Value>) -> String {
    let link = match value {
        Some(Value::Link(link)) => link.clone(),
        _ => return String::new(),
    };
    store
        .row(&link)
        .ok()
        .flatten()
        .map(|row| string_value(row.get("ShortName")))
        .unwrap_or_default()
}

fn resolve_named_files(store: &mut RowStore<'_>, value: Option<&Value>) -> Result<Vec<LazerFile>, String> {
    let Some(Value::LinkList(links)) = value else {
        return Ok(Vec::new());
    };
    let mut files = Vec::with_capacity(links.len());
    for link in links {
        let Some(usage) = store.row(link)? else { continue };
        let filename = string_value(usage.get("Filename"));
        if filename.is_empty() {
            continue;
        }
        let hash = match usage.get("File") {
            Some(Value::Link(file_link)) => match store.row(file_link)? {
                Some(row) => string_value(row.get("Hash")),
                None => continue,
            },
            _ => continue,
        };
        files.push(LazerFile { filename, hash, size: 0 });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手动冒烟辅助(默认忽略):打印第一个"带真 storyboard + 音频"谱面集
    /// 的零拷贝 Load 命令 JSON,可管道给 `aria.exe --wallpaper`:
    /// `cargo test smoke_load_command -- --ignored --nocapture`。
    #[test]
    #[ignore]
    fn smoke_load_command() {
        let Some(realm) = realm_path() else {
            eprintln!("本机无 client.realm,跳过");
            return;
        };
        let lib = parse(&realm).expect("parse");
        let root = realm.parent().unwrap().to_path_buf();
        let set = lib
            .sets
            .iter()
            .find(|s| {
                !s.beatmaps.is_empty()
                    && s.files.iter().any(|f| {
                        f.filename.to_ascii_lowercase().ends_with(".osb") && f.size > 10_000
                    })
                    && s.files.iter().any(|f| {
                        let l = f.filename.to_ascii_lowercase();
                        l.ends_with(".mp3") || l.ends_with(".ogg") || l.ends_with(".wav")
                    })
            })
            .unwrap();
        let files: Vec<crate::ipc::VFile> = set
            .files
            .iter()
            .map(|f| crate::ipc::VFile {
                name: f.filename.clone(),
                path: root
                    .join("files")
                    .join(blob_relative_path(&f.hash))
                    .to_string_lossy()
                    .into_owned(),
            })
            .collect();
        let cmd = crate::ipc::Command::Load {
            path: beatmap_blob(&root, &set.beatmaps[0].sha2).to_string_lossy().into_owned(),
            diff: None,
            fail: false,
            speed: 1.0,
            mods: 0,
            start: 0.0,
            loop_playback: false,
            manifest: Some(files),
            skin: None,
            force_colours: false,
            hidden: false,
            storyboard: true,
            video: true,
            beatmap_hitsounds: true,
            upscale: "off".into(),
            upscale_quality: "m".into(),
            monitor: None,
        };
        println!("SMOKE {}", serde_json::to_string(&cmd).unwrap());
    }

    #[test]
    fn parse_real_realm() {
        let Some(realm) = realm_path() else {
            eprintln!("本机无 client.realm,跳过");
            return;
        };
        eprintln!("realm: {}", realm.display());
        let lib = parse(&realm).expect("parse 失败");
        eprintln!("sets={} collections={}", lib.sets.len(), lib.collections.len());
        assert!(!lib.sets.is_empty(), "谱面集为空");
        let with_files = lib.sets.iter().filter(|s| !s.files.is_empty()).count();
        eprintln!("含文件列表的谱面集: {with_files}");
        assert!(with_files > 0, "所有谱面集都无文件列表");
        // 过滤后所有条目的 blob 都必须存在(零拷贝点播可直读)
        assert!(
            lib.sets.iter().all(|s| s.files.iter().all(|f| f.size > 0)),
            "过滤后仍有 size=0 的文件条目"
        );
        // 零拷贝路径:.osu blob 与 manifest 内全部 blob 都必须真实存在
        let root = realm.parent().unwrap().to_path_buf();
        for set in lib.sets.iter().take(20) {
            for b in &set.beatmaps {
                let osu = beatmap_blob(&root, &b.sha2);
                assert!(osu.is_file(), "难度 blob 缺失: {}", osu.display());
            }
            for f in &set.files {
                let blob = root.join("files").join(blob_relative_path(&f.hash));
                assert!(blob.is_file(), "素材 blob 缺失: {}", blob.display());
            }
        }
    }
}

/// 表加载策略:小表整表全载,大表(RealmNamedFileUsage/File)按行懒加载。
enum TableData {
    Bulk(Vec<Row<'static>>),
    Lazy {
        table: realm_db_reader::Table,
        rows: HashMap<usize, Row<'static>>,
    },
}

struct RowStore<'a> {
    group: &'a Group,
    tables: HashMap<usize, TableData>,
}

impl<'a> RowStore<'a> {
    fn new(group: &'a Group) -> Self {
        Self { group, tables: HashMap::new() }
    }

    fn load_table(&mut self, number: usize) -> Result<(), String> {
        if self.tables.contains_key(&number) {
            return Ok(());
        }
        let table = self
            .group
            .get_table(number)
            .map_err(|error| error.to_string())?;
        let data = if table.row_count().unwrap_or(0) <= BULK_ROW_LIMIT {
            let rows: Vec<Row<'static>> = table
                .get_rows()
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(Row::into_owned)
                .collect();
            TableData::Bulk(rows)
        } else {
            TableData::Lazy { table, rows: HashMap::new() }
        };
        self.tables.insert(number, data);
        Ok(())
    }

    fn bulk_rows(&mut self, name: &str) -> Result<Vec<Row<'static>>, String> {
        let number = self
            .group
            .get_table_names()
            .iter()
            .position(|table_name| table_name == name)
            .ok_or_else(|| format!("数据库中没有 {name} 表"))?;
        self.load_table(number)?;
        match self.tables.get(&number).expect("上面已确保存在") {
            TableData::Bulk(rows) => Ok(rows.clone()),
            TableData::Lazy { .. } => Err(format!("{name} 行数过多，不支持整表载入")),
        }
    }

    fn row(&mut self, link: &Link) -> Result<Option<Row<'static>>, String> {
        self.load_table(link.target_table_number)?;
        let data = self
            .tables
            .get_mut(&link.target_table_number)
            .expect("上面已确保存在");
        match data {
            TableData::Bulk(rows) => Ok(rows.get(link.row_number).cloned()),
            TableData::Lazy { table, rows } => {
                if let Some(row) = rows.get(&link.row_number) {
                    return Ok(Some(row.clone()));
                }
                let row = table
                    .get_row(link.row_number)
                    .map_err(|error| error.to_string())?
                    .into_owned();
                rows.insert(link.row_number, row.clone());
                Ok(Some(row))
            }
        }
    }
}

#[cfg(test)]
mod diag_tests {
    use super::*;

    /// 诊断:按标题模糊查找谱面集(SB_QUERY 环境变量),打印 .osu/.osb
    /// 的实际 blob 路径(SB 内容排查用)。
    #[test]
    fn find_map_by_title() {
        let q = std::env::var("SB_QUERY").unwrap_or_default().to_lowercase();
        if q.is_empty() {
            return;
        }
        let Some(realm) = realm_path() else { return };
        let lib = parse(&realm).expect("parse");
        let root = realm.parent().unwrap().to_path_buf();
        for s in &lib.sets {
            if s.id.to_lowercase() == q
                || s.title.to_lowercase().contains(&q)
                || s.artist.to_lowercase().contains(&q)
            {
                eprintln!("=== {} - {} (set {})", s.artist, s.title, s.id);
                for f in &s.files {
                    let l = f.filename.to_ascii_lowercase();
                    if l.ends_with(".osb") || l.ends_with(".osu") {
                        eprintln!(
                            "    {} ({})",
                            f.filename,
                            root.join("files").join(blob_relative_path(&f.hash)).display()
                        );
                    }
                }
            }
        }
    }

    /// 诊断:难度参数(AR/CS/OD/HP/BPM)与 tags 是否从 realm 读出
    /// (Information 卡显示的数据来源)。
    #[test]
    fn dump_difficulty_params() {
        let Some(realm) = realm_path() else { return };
        let lib = parse(&realm).expect("parse");
        let with_ar = lib.sets.iter().filter(|s| s.beatmaps.iter().any(|b| b.ar > 0.0)).count();
        let with_tags = lib.sets.iter().filter(|s| !s.tags.is_empty()).count();
        eprintln!("=== 含 AR: {with_ar}/{}  含 tags: {with_tags}", lib.sets.len());
        if let Some(set) = lib
            .sets
            .iter()
            .find(|s| !s.tags.is_empty() && s.beatmaps.iter().any(|b| b.ar > 0.0))
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

    /// 诊断:含视频文件的谱面集占比(视频 blob 由 ffmpeg 直读,零拷贝路径
    /// 同样适用)。
    #[test]
    fn count_video_sets() {
        let Some(realm) = realm_path() else { return };
        let lib = parse(&realm).expect("parse");
        let video_sets = lib
            .sets
            .iter()
            .filter(|s| {
                s.files.iter().any(|f| {
                    let l = f.filename.to_ascii_lowercase();
                    l.ends_with(".mp4")
                        || l.ends_with(".avi")
                        || l.ends_with(".flv")
                        || l.ends_with(".m4v")
                        || l.ends_with(".mov")
                        || l.ends_with(".webm")
                        || l.ends_with(".wmv")
                })
            })
            .count();
        eprintln!("=== 含视频文件的谱面集: {video_sets} / {}", lib.sets.len());
    }
}

