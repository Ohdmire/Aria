//! 设置持久化(%APPDATA%\<identifier>\settings.json)与开机自启
//! (HKCU\...\CurrentVersion\Run,自启带 `--hidden` 静默到托盘)。

use serde::{Deserialize, Serialize};

use crate::playlist::PlayMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub path: Option<String>,
    pub diff: Option<String>,
    pub fail: bool,
    pub speed: f32,
    #[serde(default)]
    pub loop_playback: bool,
    pub autostart: bool,
    /// 曲库源:"lazer" / "stable"。
    #[serde(default = "default_source")]
    pub source: String,
    /// 手动指定的 osu!lazer 数据目录(基础目录;None = 自动检测)。
    #[serde(default)]
    pub lazer_dir: Option<String>,
    /// 手动指定的 osu!stable 安装目录(None = 自动检测)。
    #[serde(default)]
    pub stable_dir: Option<String>,
    /// 曲库播放模式(单曲循环/顺序/随机)。
    #[serde(default)]
    pub play_mode: PlayMode,
    /// 顺序/随机播放的来源收藏夹(None = 全部谱面)。
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default = "default_volume")]
    pub volume: f32,
    /// 打击音效开关(默认开)与其音量(无独立 UI,由总音量统一调)。
    #[serde(default = "default_hitsound")]
    pub hitsound: bool,
    /// 谱面自带音效(lazer "Beatmap hitsounds",默认开):谱面集内的
    /// 采样文件(normal-hitnormal.wav / soft-hitclap2.ogg / 显式文件名
    /// 采样)按槽位优先于皮肤。加载期生效。
    #[serde(default = "default_on")]
    pub beatmap_hitsounds: bool,
    #[serde(default = "default_hits_volume")]
    pub hits_volume: f32,
    /// 总音量(0.0–1.0):主增益,同时作用于音乐与打击音效。
    #[serde(default = "default_master")]
    pub master_volume: f32,
    /// BGM 淡入淡出(换曲淡出旧曲、起播淡入;只作用于 BGM,默认开)。
    #[serde(default = "default_true")]
    pub fade_audio: bool,
    /// 音效偏移(ms):正值 = 音效提前,负值 = 延后(相对 BGM;实时生效)。
    #[serde(default)]
    pub audio_offset_ms: f32,
    /// HUD(计分板/连击/血条/按键提示),壁纸默认关。
    #[serde(default)]
    pub hud: bool,
    /// PP 计数器显示(HUD 开启时的子项;PP 时间线随 HUD 在加载期计算)。
    /// 默认关,与 HUD/UR 同一套壁纸缺省。
    #[serde(default)]
    pub pp: bool,
    /// 减少打击动画(lazer "hit animations" 关闭态,#38371):判定命中的
    /// 圆圈 60ms 整体淡出替代弹出动画。默认关 = 完整动画。
    #[serde(default)]
    pub reduce_anim: bool,
    /// 休息段背景变亮(lazer "Lighten during breaks"):break 期间
    /// 背景图暗度亮起 0.3,进出以 800ms 淡变;前奏与曲末同算 break。
    /// 壁纸特设默认关 —— 刻意与上游/lazer 默认(开)不同,壁纸常驻
    /// 桌面不希望亮度随 break 呼吸;上游 CLI 保持 lazer 同款默认开。
    #[serde(default)]
    pub break_lighten: bool,
    /// 光标渲染开关(默认开;关 = 光标与拖尾都不画)。
    #[serde(default = "default_true")]
    pub cursor: bool,
    /// 光标大小倍率(0.1–2.0,同上游 --cursor-size;光标与拖尾同步缩放)。
    #[serde(default = "default_cursor_size")]
    pub cursor_size: f32,
    /// 隐藏游玩画面模式:只渲染背景 + storyboard(隐藏 note/光标等;音频照常)。
    pub gameplay_hidden: bool,
    /// 渲染模式:"always"(始终渲染)/ "autopause"(全屏/最大化遮挡时不渲染
    /// 画面,默认)/ "off"(播放器模式:不渲染,只播音乐 + 音效)。
    #[serde(default = "default_render_mode")]
    pub render_mode: String,
    /// 手动指定的 ffmpeg 完整路径(视频层解码;None = PATH 查找)。
    #[serde(default)]
    pub ffmpeg: Option<String>,
    /// 手动指定的 ffprobe 完整路径(视频探测 + BGM 时长;None = PATH 查找)。
    #[serde(default)]
    pub ffprobe: Option<String>,
    /// 旧版独立开关(已并入 render_mode;读取时迁移)。
    #[serde(default)]
    pub pure_audio: bool,
    #[serde(default = "default_auto_pause")]
    pub auto_pause_render: bool,
    /// 背景亮度(0.0–1.0;谱面背景图与 storyboard 的混合基准)。
    #[serde(default = "default_bg_opacity")]
    pub bg_opacity: f32,
    /// HD(Hidden)视觉:物件在命中前淡出。纯视觉,不改判定;加载期生效。
    #[serde(default)]
    pub hidden: bool,
    /// storyboard 渲染开关(关 = 完全跳过解析与渲染,巨型 SB 可省数秒加载;
    /// 视频随之一并关闭——lazer ShowStoryboard 同款单开关)。
    #[serde(default = "default_on")]
    pub storyboard: bool,
    /// 皮肤目录(lazer 已安装皮肤挂载 / stable Skins/ 下目录);None = 内置 Argon。
    #[serde(default)]
    pub skin: Option<String>,
    /// 强制使用皮肤的 combo 颜色(覆盖谱面自带 [Colours];lazer
    /// "Beatmap skins" 关闭时的行为)。加载期生效。
    #[serde(default)]
    pub force_skin_colours: bool,
    /// 帧率上限(0 = 跟随屏幕刷新率;30/60/120/240/360)。
    #[serde(default)]
    pub fps: u32,
    /// 点播难度目标星数(None = 播最难;Some(x) = 播最接近 x 星的难度)。
    #[serde(default)]
    pub target_star: Option<f64>,
    /// 曲库全局星级下限(0 = 不过滤;例如 6 = 只显示含 6★+ 谱面的谱面集)。
    #[serde(default)]
    pub star_min: f64,
    /// 上次曲库播放的谱面集/难度(启动恢复优先于手动 path)。
    #[serde(default)]
    pub lazer_set: Option<String>,
    #[serde(default)]
    pub lazer_sha2: Option<String>,
    /// 关闭主窗口的行为:None = 每次询问;"tray" = 最小化到托盘;
    /// "exit" = 退出程序。
    #[serde(default)]
    pub close_action: Option<String>,
    /// 日志记录开关:开 = 本进程与壁纸子进程的运行日志写入数据目录
    /// log/aria.log(设置界面可导出);关 = 仅 stderr。
    #[serde(default)]
    pub log_enabled: bool,
    /// 双击曲库谱面行改为加入播放列表(默认关 = 双击立即播放)。纯 UI 行为。
    #[serde(default)]
    pub dblclick_add: bool,
    /// 默认导入选项(默认开):单击曲库封面直接快速导入播放列表,
    /// 不弹对话框;右键/批量按钮仍走完整对话框。
    #[serde(default = "default_true")]
    pub quick_add: bool,
    /// 快速导入应用的默认 mods(osu! legacy 位,0 = 无)。
    #[serde(default)]
    pub quick_mods: u32,
    /// 播放列表持久化(用户手动增删的队列 + 当前位置)。
    #[serde(default)]
    pub playlist: Vec<SavedTrack>,
    #[serde(default)]
    pub playlist_pos: usize,
}

/// 播放列表里的一首曲目(谱面集;难度 None = 播放时按目标星级解析)。
/// title/artist/length_ms 为显示快照:播放列表启动秒显,不等曲库解析。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedTrack {
    pub set_id: String,
    #[serde(default)]
    pub sha2: Option<String>,
    /// osu! legacy mod 位(0 = 无;加入播放列表时选定)。
    #[serde(default)]
    pub mods: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub length_ms: f64,
}

fn default_source() -> String {
    "lazer".to_string()
}

fn default_volume() -> f32 {
    0.6
}

fn default_hitsound() -> bool {
    true
}

fn default_auto_pause() -> bool {
    true
}

fn default_render_mode() -> String {
    "autopause".to_string()
}

fn default_bg_opacity() -> f32 {
    0.7
}

fn default_on() -> bool {
    true
}

fn default_hits_volume() -> f32 {
    0.8
}

fn default_true() -> bool {
    true
}

fn default_cursor_size() -> f32 {
    1.0
}

fn default_master() -> f32 {
    1.0
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            path: None,
            diff: None,
            fail: false,
            speed: 1.0,
            loop_playback: false,
            autostart: false,
            source: default_source(),
            lazer_dir: None,
            stable_dir: None,
            play_mode: PlayMode::ListOrder,
            collection: None,
            volume: default_volume(),
            hitsound: default_hitsound(),
            beatmap_hitsounds: default_on(),
            hits_volume: default_hits_volume(),
            master_volume: default_master(),
            fade_audio: true,
            audio_offset_ms: 0.0,
            hud: false,
            pp: false,
            reduce_anim: false,
            break_lighten: false,
            cursor: true,
            cursor_size: default_cursor_size(),
            gameplay_hidden: false,
            render_mode: default_render_mode(),
            ffmpeg: None,
            ffprobe: None,
            pure_audio: false,
            auto_pause_render: default_auto_pause(),
            bg_opacity: default_bg_opacity(),
            hidden: false,
            storyboard: default_on(),
            skin: None,
            force_skin_colours: false,
            fps: 0,
            target_star: None,
            star_min: 0.0,
            lazer_set: None,
            lazer_sha2: None,
            close_action: None,
            log_enabled: false,
            dblclick_add: false,
            quick_add: true,
            quick_mods: 0,
            playlist: Vec::new(),
            playlist_pos: 0,
        }
    }
}

pub fn load(app: &tauri::AppHandle) -> Settings {
    use tauri::Manager;
    let Ok(dir) = app.path().app_data_dir() else {
        return Settings::default();
    };
    // 项目更名(com.ohdmire.osu-player → com.ohdmire.aria):旧设置一次性
    // 搬家,播放列表/上次曲目/全部偏好原样保留(skin-cache 可再生不搬)
    let file = dir.join("settings.json");
    if !file.is_file() {
        if let Some(old) = dir.parent().map(|p| p.join("com.ohdmire.osu-player").join("settings.json")) {
            if old.is_file() {
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::copy(&old, &file);
            }
        }
    }
    let mut s: Settings = std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    // 旧版独立开关迁移到统一的 render_mode(一次性:迁移后写回)
    if s.pure_audio {
        s.render_mode = "off".into();
        s.pure_audio = false;
    } else if !s.auto_pause_render {
        s.render_mode = "always".into();
        s.auto_pause_render = true;
    }
    if !matches!(s.render_mode.as_str(), "always" | "autopause" | "fs_pause" | "fs_sleep" | "off") {
        s.render_mode = default_render_mode();
    }
    // 皮肤存档清理:旧版曾把"导入缓存目录绝对路径"存进皮肤字段,该
    // 功能不存在,置空回默认皮肤
    if let Some(p) = &s.skin {
        if p.contains("skin-cache") {
            s.skin = None;
        }
    }
    s
}

pub fn save(app: &tauri::AppHandle, s: &Settings) {
    use tauri::Manager;
    if let Ok(dir) = app.path().app_data_dir() {
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(json) = serde_json::to_string_pretty(s) {
            let _ = std::fs::write(dir.join("settings.json"), json);
        }
    }
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Aria";
/// 项目更名前的自启注册表值名:读写新值时顺手清掉,避免残留指向旧 exe
const RUN_VALUE_LEGACY: &str = "osu-player";

pub fn set_autostart(on: bool) -> Result<(), String> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if on {
        let (key, _) = hkcu.create_subkey(RUN_KEY).map_err(|e| e.to_string())?;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        key.set_value(RUN_VALUE, &format!("\"{}\" --hidden", exe.display()))
            .map_err(|e| e.to_string())?;
    } else if let Ok(key) = hkcu.open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE) {
        let _ = key.delete_value(RUN_VALUE);
    }
    if let Ok(key) = hkcu.open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE) {
        let _ = key.delete_value(RUN_VALUE_LEGACY);
    }
    Ok(())
}

pub fn autostart_enabled() -> bool {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE};
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_QUERY_VALUE);
    match key {
        Ok(k) => k.get_value::<String, _>(RUN_VALUE).is_ok()
            || k.get_value::<String, _>(RUN_VALUE_LEGACY).is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 谱面音效默认开;旧版 settings.json(无该字段)反序列化后同样
    /// 保持默认开(serde default),升级用户行为不变。
    #[test]
    fn beatmap_hitsounds_defaults_on() {
        assert!(Settings::default().beatmap_hitsounds, "默认必须开启谱面音效");
        let legacy = serde_json::from_str::<Settings>(
            "{\"path\":null,\"diff\":null,\"fail\":false,\"speed\":1.0,\"autostart\":false,\"gameplay_hidden\":false}",
        )
        .unwrap();
        assert!(legacy.beatmap_hitsounds, "旧配置缺字段应回落默认开");
        // 关闭后持久化为 false,重载读回仍为关
        let mut off = Settings::default();
        off.beatmap_hitsounds = false;
        let json = serde_json::to_string(&off).unwrap();
        let reread: Settings = serde_json::from_str(&json).unwrap();
        assert!(!reread.beatmap_hitsounds, "显式关闭必须被持久化");
    }

    /// 休息段背景变亮壁纸特设默认关(刻意与上游/lazer 的默认开不同);
    /// 旧 settings.json(无该字段)同样回落默认关。
    #[test]
    fn break_lighten_defaults_off() {
        assert!(!Settings::default().break_lighten, "壁纸特设:默认关");
        let legacy = serde_json::from_str::<Settings>(
            "{\"path\":null,\"diff\":null,\"fail\":false,\"speed\":1.0,\"autostart\":false,\"gameplay_hidden\":false}",
        )
        .unwrap();
        assert!(!legacy.break_lighten, "旧配置缺字段应回落默认关");
    }
}
