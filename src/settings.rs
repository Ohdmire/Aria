//! 设置持久化(%APPDATA%\<identifier>\settings.json)与开机自启
//! (HKCU\...\CurrentVersion\Run,自启带 `--hidden` 静默到托盘)。

use serde::{Deserialize, Serialize};

use crate::playlist::PlayMode;
use std::collections::HashMap;

/// 单个谱面集(歌曲)的个性设置。None = 跟随全局;`enabled = false` 时整体
/// 跟随全局(保留已填值,方便再次开启)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetOverride {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub storyboard: Option<bool>,
    #[serde(default)]
    pub audio_offset_ms: Option<f32>,
    #[serde(default)]
    pub bg_opacity: Option<f32>,
    #[serde(default)]
    pub hitsound: Option<bool>,
    #[serde(default)]
    pub beatmap_hitsounds: Option<bool>,
    /// 隐藏游玩画面(只留背景与故事板)。None = 跟随全局。
    #[serde(default)]
    pub gameplay_hidden: Option<bool>,
}

/// 某谱面集实际生效的播放参数(个性设置覆盖后的结果)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectiveMedia {
    pub storyboard: bool,
    pub beatmap_hitsounds: bool,
    pub bg_opacity: f32,
    pub audio_offset_ms: f32,
    /// 已按打击音效开关折算(关 = 0)。
    pub hits_volume: f32,
    /// 隐藏游玩画面:只渲染背景与故事板。
    pub gameplay_hidden: bool,
}

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
    /// 指定输出设备(cpal 设备 id;None = 跟随系统默认,热拔插自动跟随)。
    #[serde(default)]
    pub audio_device: Option<String>,
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
    /// 滑条渐入(lazer Snaking in sliders,默认开)。
    #[serde(default = "default_true")]
    pub snaking_in: bool,
    /// 滑条渐出(lazer Snaking out sliders,默认开)。
    #[serde(default = "default_true")]
    pub snaking_out: bool,
    /// 光标轨迹(lazer Cursor trail,默认开)。关 = 只留光标。
    #[serde(default = "default_true")]
    pub cursor_trail: bool,
    /// 光标波纹(lazer Cursor ripples,默认关)。按下时扩出一圈。
    #[serde(default)]
    pub cursor_ripples: bool,
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
    /// 超分模式(off / fsr / anime4k):视频帧实时生效,BG 载入期放大。
    #[serde(default = "default_upscale")]
    pub upscale: String,
    /// Anime4K 质量档(s/m/l/vl/ul,缺省 m;仅 Anime4K 滤镜生效)。
    #[serde(default = "default_upscale_quality")]
    pub upscale_quality: String,
    /// 壁纸渲染目标显示器(设备名如 `\.\DISPLAY2`;None = 主屏)。
    /// 更改后重载当前曲目生效。
    #[serde(default)]
    pub monitor: Option<String>,
    /// 调试窗口模式(默认关):不挂桌面层,用常规带边框窗口播放,
    /// 排障用。桌面挂接彻底失败时也会自动退回此模式(仅当次会话)。
    /// --preview 启动参数与之等效且优先。
    #[serde(default)]
    pub debug_window: bool,
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
    /// 曲名 / 作者优先显示 Unicode 原名(默认开;关 = 罗马音 / 英文)。
    #[serde(default = "default_true")]
    pub display_unicode: bool,
    /// 全局快捷键总开关(默认关:不开机注册任何热键)。
    #[serde(default)]
    pub hotkeys_enabled: bool,
    /// 上一首组合键(如 `Ctrl+Alt+Left`;None/空 = 不绑定,默认留空)。
    #[serde(default)]
    pub hotkey_prev: Option<String>,
    /// 播放/暂停组合键(默认留空)。
    #[serde(default)]
    pub hotkey_toggle: Option<String>,
    /// 下一首组合键(默认留空)。
    #[serde(default)]
    pub hotkey_next: Option<String>,
    /// 谱面集个性设置(set_id → override);只包含用户改过的集。
    #[serde(default)]
    pub set_overrides: HashMap<String, SetOverride>,
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

fn default_render_mode() -> String {
    "always".to_string()
}

fn default_bg_opacity() -> f32 {
    0.7
}

fn default_on() -> bool {
    true
}

fn default_upscale() -> String {
    "off".into()
}

fn default_upscale_quality() -> String {
    "m".into()
}

fn default_hits_volume() -> f32 {
    0.6
}

fn default_true() -> bool {
    true
}

fn default_cursor_size() -> f32 {
    1.0
}

fn default_master() -> f32 {
    0.6
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
            audio_device: None,
            audio_offset_ms: 0.0,
            hud: false,
            pp: false,
            reduce_anim: false,
            break_lighten: false,
            cursor: true,
            cursor_size: default_cursor_size(),
            snaking_in: true,
            snaking_out: true,
            cursor_trail: true,
            cursor_ripples: false,
            gameplay_hidden: false,
            render_mode: default_render_mode(),
            ffmpeg: None,
            ffprobe: None,
            bg_opacity: default_bg_opacity(),
            hidden: false,
            storyboard: default_on(),
            skin: None,
            force_skin_colours: false,
            upscale: default_upscale(),
            upscale_quality: default_upscale_quality(),
            monitor: None,
            debug_window: false,
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
            display_unicode: true,
            hotkeys_enabled: false,
            hotkey_prev: None,
            hotkey_toggle: None,
            hotkey_next: None,
            set_overrides: HashMap::new(),
            playlist: Vec::new(),
            playlist_pos: 0,
        }
    }
}

impl Settings {
    /// 某谱面集实际生效的播放参数:个性设置开启时逐项覆盖全局,
    /// 未填(None)的项继续跟随全局。数值统一钳位,避免坏存档。
    pub fn effective_for(&self, set_id: &str) -> EffectiveMedia {
        let over = self.set_overrides.get(set_id).filter(|o| o.enabled);
        let storyboard = over.and_then(|o| o.storyboard).unwrap_or(self.storyboard);
        let beatmap_hitsounds = over
            .and_then(|o| o.beatmap_hitsounds)
            .unwrap_or(self.beatmap_hitsounds);
        let hitsound = over.and_then(|o| o.hitsound).unwrap_or(self.hitsound);
        let bg_opacity = over.and_then(|o| o.bg_opacity).unwrap_or(self.bg_opacity);
        let audio_offset_ms = over
            .and_then(|o| o.audio_offset_ms)
            .unwrap_or(self.audio_offset_ms);
        let gameplay_hidden = over
            .and_then(|o| o.gameplay_hidden)
            .unwrap_or(self.gameplay_hidden);
        EffectiveMedia {
            storyboard,
            beatmap_hitsounds,
            bg_opacity: bg_opacity.clamp(0.0, 1.0),
            audio_offset_ms: audio_offset_ms.clamp(-1000.0, 1000.0),
            gameplay_hidden,
            hits_volume: if hitsound {
                self.hits_volume.clamp(0.0, 1.0)
            } else {
                0.0
            },
        }
    }
}

pub fn load(app: &tauri::AppHandle) -> Settings {
    use tauri::Manager;
    let Ok(dir) = app.path().app_data_dir() else {
        return Settings::default();
    };
    let file = dir.join("settings.json");
    let mut s: Settings = std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if !matches!(s.render_mode.as_str(), "always" | "autopause" | "fs_pause" | "fs_sleep" | "off") {
        s.render_mode = default_render_mode();
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
    Ok(())
}

pub fn autostart_enabled() -> bool {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE};
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_QUERY_VALUE);
    match key {
        Ok(k) => k.get_value::<String, _>(RUN_VALUE).is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 谱面音效默认开。settings.json 缺该字段时同样回落默认开。
    #[test]
    fn beatmap_hitsounds_defaults_on() {
        assert!(Settings::default().beatmap_hitsounds, "默认必须开启谱面音效");
        let legacy = serde_json::from_str::<Settings>(
            "{\"path\":null,\"diff\":null,\"fail\":false,\"speed\":1.0,\"autostart\":false,\"gameplay_hidden\":false}",
        )
        .unwrap();
        assert!(legacy.beatmap_hitsounds, "缺字段应回落默认开");
        // 关闭后持久化为 false,重载读回仍为关
        let mut off = Settings::default();
        off.beatmap_hitsounds = false;
        let json = serde_json::to_string(&off).unwrap();
        let reread: Settings = serde_json::from_str(&json).unwrap();
        assert!(!reread.beatmap_hitsounds, "显式关闭必须被持久化");
    }

    /// 休息段背景变亮壁纸特设默认关(刻意与上游/lazer 的默认开不同)。
    /// settings.json 缺该字段时同样回落默认关。
    #[test]
    fn break_lighten_defaults_off() {
        assert!(!Settings::default().break_lighten, "壁纸特设:默认关");
        let legacy = serde_json::from_str::<Settings>(
            "{\"path\":null,\"diff\":null,\"fail\":false,\"speed\":1.0,\"autostart\":false,\"gameplay_hidden\":false}",
        )
        .unwrap();
        assert!(!legacy.break_lighten, "缺字段应回落默认关");
    }

    /// 全局快捷键默认关闭,且不预设任何组合(留给用户自行填写)。
    #[test]
    fn hotkeys_default_off_and_empty() {
        let d = Settings::default();
        assert!(!d.hotkeys_enabled, "快捷键默认必须关闭");
        assert_eq!(d.hotkey_prev, None);
        assert_eq!(d.hotkey_toggle, None);
        assert_eq!(d.hotkey_next, None);
        let legacy = serde_json::from_str::<Settings>(
            "{\"path\":null,\"diff\":null,\"fail\":false,\"speed\":1.0,\"autostart\":false,\"gameplay_hidden\":false}",
        )
        .unwrap();
        assert!(!legacy.hotkeys_enabled, "缺字段应回落默认关");
        assert_eq!(legacy.hotkey_next, None, "缺字段不指定默认快捷键");
    }

    /// 曲名默认按 Unicode 原名显示(旧设置缺字段时同样默认勾选)。
    #[test]
    fn display_unicode_defaults_on() {
        assert!(Settings::default().display_unicode, "Unicode 显示默认开");
        let legacy = serde_json::from_str::<Settings>(
            "{\"path\":null,\"diff\":null,\"fail\":false,\"speed\":1.0,\"autostart\":false,\"gameplay_hidden\":false}",
        )
        .unwrap();
        assert!(legacy.display_unicode, "缺字段应回落默认开");
    }

    /// 谱面集个性设置:关闭时整体跟随全局;开启后逐项覆盖,未填项回退全局。
    #[test]
    fn set_override_effective_media() {
        let mut s = Settings::default();
        s.storyboard = true;
        s.beatmap_hitsounds = true;
        s.hitsound = true;
        s.hits_volume = 0.8;
        s.bg_opacity = 0.7;
        s.audio_offset_ms = 0.0;

        // 无条目 / enabled=false:完全跟随全局
        let base = s.effective_for("s1");
        assert_eq!(base.storyboard, true);
        assert_eq!(base.hits_volume, 0.8);
        s.set_overrides.insert(
            "s1".into(),
            SetOverride {
                enabled: false,
                storyboard: Some(false),
                audio_offset_ms: Some(120.0),
                bg_opacity: Some(0.2),
                hitsound: Some(false),
                beatmap_hitsounds: Some(false),
                gameplay_hidden: Some(true),
            },
        );
        assert_eq!(s.effective_for("s1"), base, "未开启时忽略全部覆盖");

        // 开启后:覆盖项生效,未填项跟随全局
        s.set_overrides.get_mut("s1").unwrap().enabled = true;
        let over = s.effective_for("s1");
        assert!(!over.storyboard);
        assert!(!over.beatmap_hitsounds);
        assert_eq!(over.bg_opacity, 0.2);
        assert_eq!(over.audio_offset_ms, 120.0);
        assert_eq!(over.hits_volume, 0.0, "关闭打击音效 = 音量 0");
        assert!(over.gameplay_hidden, "覆盖开启隐藏游玩画面");

        // 只覆盖部分字段:其余仍用全局
        s.set_overrides.insert(
            "s2".into(),
            SetOverride {
                enabled: true,
                bg_opacity: Some(1.5),
                ..Default::default()
            },
        );
        let partial = s.effective_for("s2");
        assert!(partial.storyboard, "未覆盖 storyboard 跟随全局");
        assert!(!partial.gameplay_hidden, "未覆盖隐藏游玩画面跟随全局");
        assert_eq!(partial.bg_opacity, 1.0, "坏值钳位");
        assert_eq!(partial.audio_offset_ms, 0.0);
    }
}
