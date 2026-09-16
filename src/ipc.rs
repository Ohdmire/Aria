//! 父进程(设置界面)与壁纸渲染子进程之间的 JSON-lines 协议。
//!
//! 父 → 子走子进程 stdin,每行一条 [`Command`];子 → 父走 stdout,
//! 每行一条 [`Event`]。子进程日志输出到 stderr,不占用协议通道;
//! stdin EOF(父进程退出)即子进程的退出信号,不会残留壁纸窗口。

use serde::{Deserialize, Serialize};

fn default_loop() -> bool {
    true
}

fn default_on() -> bool {
    true
}

fn default_upscale() -> String {
    "off".into()
}

fn default_quality() -> String {
    "m".into()
}

fn one() -> f32 {
    1.0
}

/// lazer 谱面集的虚拟文件(零拷贝):谱面集内文件名 → files/ 内容寻址库
/// blob 的实际路径。渲染端按名解析(.osu / 音频 / 背景 / storyboard 素材 /
/// 视频),全程不复制。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VFile {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// 枚举谱面难度(.osz 内的 .osu 列表;单个 .osu/.osb 为空表),回 [`Event::DiffList`]。
    Probe { path: String },
    /// 加载并播放;已有壁纸窗口时原位切换内容,没有则先创建窗口。
    /// `fail` 为旧版 storyboard Pass/Fail 开关,autoplay 模式已忽略,
    /// 保留字段兼容旧父进程。
    /// `manifest`:lazer 虚拟文件表(Some = 零拷贝按名解析;None = `path`
    /// 所在目录即素材根,普通路径流)。
    /// `skin`:皮肤目录(lazer 已安装挂载 / stable Skins/ 目录;
    /// None = 内置 Argon)。
    /// `force_colours`:皮肤 combo 颜色强制覆盖谱面 [Colours]。
    /// `hidden`:HD(Hidden)视觉,加载期生效。
    /// `mods`:osu! legacy mod 位(壁纸可选 HD/HR/EZ/DT/HT/NC)。难度类
    /// (HR/EZ)改判定难度并镜像 playfield,速率类(DT/HT/NC)按 rate
    /// 加速整个会话(NC 额外保留 BGM 升调),HD 为视觉;加载期生效。
        /// `storyboard`/`video`:storyboard 与其视频层渲染开关(关 = 完全
        /// 跳过解析与渲染,无论谱面是否存在)。
        /// `beatmap_hitsounds`:谱面自带音效开关(lazer "Beatmap
        /// hitsounds",默认开):谱面集内的采样文件按槽位优先于皮肤。
        Load {
            path: String,
            diff: Option<String>,
            #[serde(default)]
            fail: bool,
            speed: f32,
            #[serde(default)]
            start: f32,
            #[serde(default = "default_loop")]
            loop_playback: bool,
            #[serde(default)]
            manifest: Option<Vec<VFile>>,
            #[serde(default)]
            skin: Option<String>,
            #[serde(default)]
            force_colours: bool,
            #[serde(default)]
            hidden: bool,
            #[serde(default)]
            mods: u32,
            #[serde(default = "default_on")]
            storyboard: bool,
            #[serde(default = "default_on")]
            video: bool,
            #[serde(default = "default_on")]
            beatmap_hitsounds: bool,
            /// 超分模式(off / fsr / anime4k):视频帧与 BG(载入期)。
            #[serde(default = "default_upscale")]
            upscale: String,
            /// 目标显示器 bounds [x,y,w,h](虚拟屏绝对坐标;缺省 = 铺满
            /// 桌面层)。窗口创建时生效。
            #[serde(default)]
            monitor: Option<Vec<i32>>,
            /// Anime4K 质量档(s/m/l/vl/ul,缺省 m)。
            #[serde(default = "default_quality")]
            upscale_quality: String,
        },
    /// 卸载内容并销毁壁纸窗口,恢复桌面原壁纸。
    Unload,
    Pause,
    Resume,
    Seek { ms: f32 },
    SetSpeed { x: f32 },
    SetFail { on: bool },
    SetLoop { on: bool },
    /// 立即上报一次 [`Event::Status`]。
    Status,
    /// 音乐音量(0.0–1.0,总音量之下的分量)。
    SetVolume { v: f32 },
    /// 总音量(0.0–1.0):主增益,同时作用于音乐与打击音效。
    SetMaster { v: f32 },
    /// 音效偏移(ms):正值 = 音效提前,负值 = 延后;实时生效。
    SetOffset { ms: f32 },
    /// 打击音效音量(0.0–1.0,0 = 关)。
    SetHitsVolume { v: f32 },
    /// 谱面自带音效运行期热切换:按新状态重建打击音效采样表(谱面文件
    /// 层开/关),音乐/画面/进度/游标不动 —— 音效槽位时间轴与开关无关。
    SetBeatmapHitsounds { on: bool },
    /// HUD(计分板/连击/血条/按键提示)开关。
    SetHud { on: bool },
    /// PP 计数器开关(HUD 的子项,实时生效;PP 时间线在加载期随 HUD
    /// 一起计算,与该开关无关)。
    SetPp { on: bool },
    /// osu! 打击动画开关(on = 完整动画,默认;off = 减少模式:命中
    /// 圆圈 60ms 整体淡出,legacy 数字快速淡出 hack 绕过)。实时生效。
    SetHitAnimations { on: bool },
    /// 休息段背景变亮开关(实时生效):break 期间背景暗度亮起 0.3,
    /// 800ms OutQuint 淡变;仅背景图(storyboard 层暗度为宿主预乘)。
    SetBreakLighten { on: bool },
    /// 光标渲染开关(实时生效):关 = 光标与拖尾都不画。
    SetCursor { on: bool },
    /// 光标大小倍率(0.1–2.0,实时生效;光标与拖尾同步缩放)。
    SetCursorSize { x: f32 },
    /// 皮肤热切换(不重载不重解析):重解皮肤 → 重打包图集 →
    /// set_atlas 热换 GPU 纹理 → 重置皮肤缓存 → 重涂 combo 色 →
    /// 热换打击音效采样。音频/时钟/判定/storyboard 全部保留;
    /// 无渲染会话时(纯音频/未加载)仅更新皮肤与后续加载参数。
    /// `skin`:已解析的皮肤目录路径(None = 内置 Argon)。
    SetSkin { skin: Option<String>, force_colours: bool },
    /// BGM 淡入淡出(换曲淡出旧曲、起播淡入;只作用于 BGM)。
    SetFade { on: bool },
    /// 隐藏游玩画面模式:只渲染背景 + storyboard,隐藏 note/滑条/转盘/光标
    /// 等 gameplay 元素(音频/判定照常)。
    SetGameplayHidden { on: bool },
    /// 背景亮度(0.0–1.0,实时生效;仅在有谱面背景时可见)。
    SetBgOpacity { v: f32 },
    /// HD(Hidden)视觉,实时生效(纯渲染旗标,不重载)。
    SetHidden { on: bool },
    /// storyboard 视频层,实时生效(不解码不画;不重载)。
    SetVideo { on: bool },
    /// 超分模式(off / fsr / anime4k-a|b|c)+ Anime4K 质量档
    /// (s/m/l/vl/ul):全实时热切换(视频链重建 + BG 图集热换)。
    #[serde(rename_all = "camelCase")]
    SetUpscale { mode: String, quality: String },
    /// 渲染模式(设置 render_mode 的运行期形态,一次性下发):
    /// always 始终渲染 / autopause 全屏时不渲染画面 / fs_pause 全屏时
    /// 暂停播放(壁纸保留)/ fs_sleep 全屏时暂停并释放壁纸 / off 播放器
    /// 模式。切换**无缝**:不重载曲目,仅拆/建会话与暂停/恢复播放。
    SetRenderMode { mode: String },
    /// 帧率上限(0 = 跟随屏幕刷新率)。
    SetFps { fps: u32 },
    /// 日志记录开关(设置界面;子进程随命令开/关同一份日志文件)。
    SetLog { on: bool },
    /// 手动指定的 ffmpeg/ffprobe 完整路径(None = PATH 查找);下次载入生效。
    #[serde(rename_all = "camelCase")]
    SetFfmpegBins { ffmpeg: Option<String>, ffprobe: Option<String> },
    /// 内部命令:壁纸窗口被外部销毁后,延迟重挂桌面并恢复播放(父进程不发)。
    #[serde(skip_deserializing)]
    Reload,
    /// 内部命令:SoundTouch tempo 预处理完成(后台线程 → 事件循环,父进程
    /// 不发)。`path = None` = 预处理失败,回退原文件变调播放。
    #[serde(skip_deserializing)]
    AudioReady { speed: f32, path: Option<std::path::PathBuf>, fade_ms: f64 },
    /// 内部命令:PP 后台补算完成(后台线程 → 事件循环,父进程不发):
    /// 热注入带 PP/星级时间线的 GameData。`seq` = 补算发起时的加载序号,
    /// 与当前不符(已切歌/重载)即整体丢弃;`game = None` = 补算失败
    /// (保持无 PP 显示)。载荷不可序列化(仅进程内事件通道),serde skip。
    #[serde(skip)]
    PpReady { seq: u64, game: Option<PpGame> },
    Quit,
}

/// [`Command::PpReady`] 的载荷:Arc 共享的 GameData(无 Clone/Debug/
/// serde derive,这里手工实现 Clone/Debug 满足 Command 的 derive 约束;
/// serde 对所在变体整体 skip,永不走 stdin/stdout 的 JSON)。
pub struct PpGame(pub std::sync::Arc<osu_replay_render::game::GameData>);

impl Clone for PpGame {
    fn clone(&self) -> Self {
        PpGame(self.0.clone())
    }
}

impl std::fmt::Debug for PpGame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PpGame(..)")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// 壁纸窗口已创建并附加到桌面(WorkerW)。
    #[serde(rename_all = "camelCase")]
    Ready { width: u32, height: u32, refresh_hz: u32 },
    /// Probe 结果:可用难度(.osz 内 .osu 的相对路径)。
    #[serde(rename_all = "camelCase")]
    DiffList { path: String, diffs: Vec<String> },
    #[serde(rename_all = "camelCase")]
    Loaded {
        path: String,
        diff: Option<String>,
        duration_ms: f32,
        widescreen: bool,
        /// 是否找到了可播放的谱面音频。
        has_audio: bool,
        /// 本次加载携带的 mod 位(播放条 tag 显示)。
        #[serde(default)]
        mods: u32,
    },
    /// 心跳:每秒 + 每次状态变化后上报。
    #[serde(rename_all = "camelCase")]
    Status {
        t_ms: f32,
        duration_ms: f32,
        playing: bool,
        /// 实际播放速度(mods rate × 用户倍速)。
        speed: f32,
        /// 用户倍速(播放条变速按钮的值;DT/HT 之外的 UI 真相)。
        #[serde(default = "one")]
        user_speed: f32,
        looping: bool,
        /// 实际渲染帧率(过去一秒的渲染帧数)。
        fps: u32,
    },
    /// 播放到结尾且未开启循环。
    Ended,
    /// 壁纸窗口被外部销毁(explorer 重启等),子进程稍后自动重挂恢复。
    Detached,
    Unloaded,
    #[serde(rename_all = "camelCase")]
    Error { message: String },
    /// 子进程即将退出(收到 Quit 或 stdin EOF)。
    Exited,
}
