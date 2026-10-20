//! 全局快捷键(Windows `RegisterHotKey`)。
//!
//! 设置里默认**不开启**、也没有任何默认组合;只有用户自己填写并启用后
//! 才会注册。注册/注销与 `WM_HOTKEY` 分发都在同一条专用线程上完成:
//! `RegisterHotKey(NULL, ..)` 把热键消息投递到**调用线程**的消息队列,
//! 所以这条线程必须自己泵消息;配置变更通过 mpsc 下发,并在设置命令里
//! 等一次确认(注册冲突等错误可以直接反馈到 UI)。
//!
//! 动作与托盘菜单一致:上一首 / 播放·暂停 / 下一首。

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

use tauri::AppHandle;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
    UnregisterHotKey, VIRTUAL_KEY, VK_BACK, VK_CAPITAL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE,
    VK_HOME, VK_INSERT, VK_LEFT, VK_MEDIA_NEXT_TRACK, VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK,
    VK_MEDIA_STOP, VK_NEXT, VK_NUMLOCK, VK_NUMPAD0, VK_OEM_1, VK_OEM_2, VK_OEM_3, VK_OEM_4,
    VK_OEM_5, VK_OEM_6, VK_OEM_7, VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS, VK_PAUSE,
    VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SCROLL, VK_SNAPSHOT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage, WM_HOTKEY,
};

use crate::settings::Settings;

/// 快捷键动作(注册 id 固定,便于注销/重新注册)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Prev,
    Toggle,
    Next,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Prev => "上一首",
            Action::Toggle => "播放/暂停",
            Action::Next => "下一首",
        }
    }

    fn id(self) -> i32 {
        match self {
            Action::Prev => 1,
            Action::Toggle => 2,
            Action::Next => 3,
        }
    }

    fn from_id(id: i32) -> Option<Self> {
        match id {
            1 => Some(Action::Prev),
            2 => Some(Action::Toggle),
            3 => Some(Action::Next),
            _ => None,
        }
    }
}

/// 解析后的一个组合键。
#[derive(Debug, Clone)]
pub struct Binding {
    pub action: Action,
    /// Win32 MOD_* 位(不含 MOD_NOREPEAT,注册时补上)。
    pub mods: u32,
    pub vk: u32,
    /// 规范化后的显示文本,如 `Ctrl+Alt+Right`。
    pub text: String,
}

impl Binding {
    fn same_combo(&self, other: &Binding) -> bool {
        self.mods == other.mods && self.vk == other.vk
    }
}

/// `set_hotkeys` 的解析结果(文本已规范化,供设置持久化/回显)。
pub struct ParsedConfig {
    pub bindings: Vec<Binding>,
    pub prev: Option<String>,
    pub toggle: Option<String>,
    pub next: Option<String>,
}

/// 托管到 Tauri 的快捷键管理器句柄。
#[derive(Clone)]
pub struct HotkeyManager {
    tx: Sender<Cmd>,
}

enum Cmd {
    Set {
        bindings: Vec<Binding>,
        ack: Sender<Result<(), String>>,
    },
}

/// 启动快捷键线程并按已保存的设置注册(启动即生效;默认关闭 = 空表)。
pub fn init(app: AppHandle, settings: &Settings) -> HotkeyManager {
    let (tx, rx) = mpsc::channel();
    let initial = if settings.hotkeys_enabled {
        bindings_from_settings(settings)
    } else {
        Vec::new()
    };
    let app2 = app.clone();
    let spawned = std::thread::Builder::new()
        .name("aria-hotkeys".into())
        .spawn(move || worker(rx, app2, initial));
    if let Err(e) = spawned {
        log::error!("[hotkeys] 启动快捷键线程失败: {e}");
    }
    HotkeyManager { tx }
}

impl HotkeyManager {
    /// 用新的组合集合替换当前注册(空表 = 全部注销)。同步等待线程确认,
    /// 冲突/非法组合等错误会带回来。
    pub fn register(&self, bindings: Vec<Binding>) -> Result<(), String> {
        let (ack, rx) = mpsc::channel();
        self.tx
            .send(Cmd::Set { bindings, ack })
            .map_err(|_| "快捷键服务未运行".to_string())?;
        rx.recv_timeout(Duration::from_secs(2))
            .map_err(|_| "快捷键设置超时".to_string())?
    }
}

fn worker(rx: Receiver<Cmd>, app: AppHandle, initial: Vec<Binding>) {
    let mut current = Vec::new();
    if let Err(e) = register_all(&initial, &mut current) {
        log::warn!("[hotkeys] 启动注册失败: {e}");
    }
    loop {
        loop {
            match rx.try_recv() {
                Ok(Cmd::Set { bindings, ack }) => {
                    let res = apply(&bindings, &mut current);
                    if let Err(e) = &res {
                        log::warn!("[hotkeys] {e}");
                    }
                    let _ = ack.send(res);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    unregister_all(&current);
                    return;
                }
            }
        }
        pump(&app);
        // 20ms 轮询:WM_HOTKEY 通常在消息队列里立即被取走,延迟可忽略;
        // 也避免用 MsgWaitForMultipleObjects 额外同步事件句柄。
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 注销旧配置 → 注册新配置;新配置有任一失败则回滚到旧配置。
fn apply(next: &[Binding], current: &mut Vec<Binding>) -> Result<(), String> {
    unregister_all(current);
    let old = std::mem::take(current);
    let mut fresh = Vec::new();
    match register_all(next, &mut fresh) {
        Ok(()) => {
            *current = fresh;
            Ok(())
        }
        Err(e) => {
            unregister_all(&fresh);
            let mut back = Vec::new();
            if let Err(e2) = register_all(&old, &mut back) {
                log::warn!("[hotkeys] 回滚旧快捷键失败: {e2}");
            }
            *current = back;
            Err(e)
        }
    }
}

fn register_all(bindings: &[Binding], out: &mut Vec<Binding>) -> Result<(), String> {
    for b in bindings {
        let mods = HOT_KEY_MODIFIERS(b.mods | MOD_NOREPEAT.0);
        match unsafe { RegisterHotKey(None, b.action.id(), mods, b.vk) } {
            Ok(()) => out.push(b.clone()),
            Err(e) => {
                return Err(format!(
                    "{}快捷键 {} 注册失败:{}",
                    b.action.label(),
                    b.text,
                    friendly(&e)
                ));
            }
        }
    }
    Ok(())
}

fn unregister_all(bindings: &[Binding]) {
    for b in bindings {
        let _ = unsafe { UnregisterHotKey(None, b.action.id()) };
    }
}

fn friendly(e: &windows::core::Error) -> String {
    // ERROR_HOTKEY_ALREADY_REGISTERED = 1409(HRESULT 低 16 位)
    if (e.code().0 as u32 & 0xFFFF) == 1409 {
        return "该组合键已被其他程序占用".into();
    }
    let message = e.message();
    if message.is_empty() {
        format!("系统错误 {}", e.code().0)
    } else {
        message
    }
}

fn pump(app: &AppHandle) {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_HOTKEY {
                if let Some(action) = Action::from_id(msg.wParam.0 as i32) {
                    dispatch(app.clone(), action);
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn dispatch(app: AppHandle, action: Action) {
    // 切歌/重载可能走慢路径(解析曲库),不能阻塞快捷键线程的消息泵
    std::thread::spawn(move || match action {
        Action::Prev => {
            if let Err(e) = crate::ctl::playlist_prev(&app) {
                log::warn!("[hotkeys] 上一首失败: {e}");
            }
        }
        Action::Next => {
            if let Err(e) = crate::ctl::playlist_next(&app) {
                log::warn!("[hotkeys] 下一首失败: {e}");
            }
        }
        Action::Toggle => {
            let cmd = if crate::ctl::is_playing(&app) {
                crate::ipc::Command::Pause
            } else {
                crate::ipc::Command::Resume
            };
            let _ = crate::ctl::send_cmd(&app, cmd);
        }
    });
}

/// 已保存配置 → 组合列表(无效项记日志后跳过,不阻止启动)。
pub fn bindings_from_settings(s: &Settings) -> Vec<Binding> {
    let mut out = Vec::new();
    for (action, text) in [
        (Action::Prev, s.hotkey_prev.as_deref()),
        (Action::Toggle, s.hotkey_toggle.as_deref()),
        (Action::Next, s.hotkey_next.as_deref()),
    ] {
        let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) else {
            continue;
        };
        match parse_binding(action, text) {
            Ok(b) => out.push(b),
            Err(e) => log::warn!("[hotkeys] 忽略无效快捷键 {text:?}: {e}"),
        }
    }
    out
}

/// 解析 UI 提交的三条文本;空 = 不绑定。两个动作不能共用同一组合。
pub fn parse_inputs(
    prev: Option<&str>,
    toggle: Option<&str>,
    next: Option<&str>,
) -> Result<ParsedConfig, String> {
    let mut bindings: Vec<Binding> = Vec::new();
    let mut texts: [Option<String>; 3] = [None, None, None];
    for (i, (action, text)) in [
        (Action::Prev, prev),
        (Action::Toggle, toggle),
        (Action::Next, next),
    ]
    .into_iter()
    .enumerate()
    {
        let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) else {
            continue;
        };
        let binding = parse_binding(action, text)?;
        if bindings.iter().any(|b| b.same_combo(&binding)) {
            return Err(format!("两个动作不能使用同一个快捷键({})", binding.text));
        }
        texts[i] = Some(binding.text.clone());
        bindings.push(binding);
    }
    let [prev, toggle, next] = texts;
    Ok(ParsedConfig {
        bindings,
        prev,
        toggle,
        next,
    })
}

/// 解析单个组合,如 `Ctrl+Alt+Right` / `ctrl shift f5` / `Win+MediaNext`。
pub fn parse_binding(action: Action, text: &str) -> Result<Binding, String> {
    let mut mods = 0u32;
    let mut vk: Option<u32> = None;
    let mut key_label = String::new();
    // 兼容 `Ctrl+Alt+Right` 与 `Ctrl Alt Right` 两种写法
    let mut tokens: Vec<&str> = Vec::new();
    for part in text.split('+') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("快捷键格式不正确:{text}"));
        }
        tokens.extend(part.split_whitespace());
    }
    for token in tokens {
        let low = token.to_ascii_lowercase();
        let modifier = match low.as_str() {
            "ctrl" | "control" => Some(MOD_CONTROL.0),
            "alt" => Some(MOD_ALT.0),
            "shift" => Some(MOD_SHIFT.0),
            "win" | "super" | "meta" | "cmd" => Some(MOD_WIN.0),
            _ => None,
        };
        if let Some(m) = modifier {
            mods |= m;
            continue;
        }
        if vk.is_some() {
            return Err(format!("一个快捷键只能包含一个主键:{text}"));
        }
        let (code, label) = parse_key(token)?;
        vk = Some(code);
        key_label = label;
    }
    let Some(vk) = vk else {
        return Err(format!("快捷键缺少主键:{text}"));
    };
    if mods == 0 && !allows_bare(vk) {
        return Err("字母 / 数字 / 符号键至少需要一个修饰键(Ctrl / Alt / Shift / Win)".into());
    }
    let mut parts: Vec<&str> = Vec::new();
    if mods & MOD_CONTROL.0 != 0 {
        parts.push("Ctrl");
    }
    if mods & MOD_ALT.0 != 0 {
        parts.push("Alt");
    }
    if mods & MOD_SHIFT.0 != 0 {
        parts.push("Shift");
    }
    if mods & MOD_WIN.0 != 0 {
        parts.push("Win");
    }
    parts.push(&key_label);
    Ok(Binding {
        action,
        mods,
        vk,
        text: parts.join("+"),
    })
}

/// 单独按（无修饰键）也相对安全的键:功能键与媒体键。
fn allows_bare(vk: u32) -> bool {
    // F1–F24,以及媒体键(系统本来就把它们当播放控制)
    (0x70..=0x87).contains(&vk)
        || vk == VK_MEDIA_PLAY_PAUSE.0 as u32
        || vk == VK_MEDIA_NEXT_TRACK.0 as u32
        || vk == VK_MEDIA_PREV_TRACK.0 as u32
        || vk == VK_MEDIA_STOP.0 as u32
}

fn parse_key(token: &str) -> Result<(u32, String), String> {
    let low = token.to_ascii_lowercase();
    // F1–F24
    if let Some(n) = low.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Ok((0x70 + n - 1, format!("F{n}")));
        }
    }
    // Numpad0–9
    if let Some(n) = low
        .strip_prefix("numpad")
        .and_then(|n| n.parse::<u32>().ok())
    {
        if n <= 9 {
            return Ok((VK_NUMPAD0.0 as u32 + n, format!("Numpad{n}")));
        }
    }
    // 单字符:字母 / 数字 / 符号
    let chars: Vec<char> = low.chars().collect();
    if chars.len() == 1 {
        let c = chars[0];
        if c.is_ascii_alphabetic() {
            let up = c.to_ascii_uppercase();
            return Ok((up as u32, up.to_string()));
        }
        if c.is_ascii_digit() {
            return Ok((c as u32, c.to_string()));
        }
        let (code, label) = match c {
            '-' => (VK_OEM_MINUS, "Minus"),
            '=' => (VK_OEM_PLUS, "Plus"),
            ',' => (VK_OEM_COMMA, "Comma"),
            '.' => (VK_OEM_PERIOD, "Period"),
            '/' => (VK_OEM_2, "Slash"),
            '\\' => (VK_OEM_5, "Backslash"),
            ';' => (VK_OEM_1, "Semicolon"),
            '\'' => (VK_OEM_7, "Quote"),
            '`' => (VK_OEM_3, "Backquote"),
            '[' => (VK_OEM_4, "BracketLeft"),
            ']' => (VK_OEM_6, "BracketRight"),
            _ => return Err(format!("无法识别的按键:{token}")),
        };
        return Ok((code.0 as u32, label.to_string()));
    }
    let (code, label): (VIRTUAL_KEY, &str) = match low.as_str() {
        "left" | "arrowleft" => (VK_LEFT, "Left"),
        "right" | "arrowright" => (VK_RIGHT, "Right"),
        "up" | "arrowup" => (VK_UP, "Up"),
        "down" | "arrowdown" => (VK_DOWN, "Down"),
        "space" | "空格" => (VK_SPACE, "Space"),
        "enter" | "return" | "回车" => (VK_RETURN, "Enter"),
        "tab" => (VK_TAB, "Tab"),
        "backspace" => (VK_BACK, "Backspace"),
        "delete" | "del" => (VK_DELETE, "Delete"),
        "insert" | "ins" => (VK_INSERT, "Insert"),
        "home" => (VK_HOME, "Home"),
        "end" => (VK_END, "End"),
        "pageup" | "pgup" | "prior" => (VK_PRIOR, "PageUp"),
        "pagedown" | "pgdn" | "next" => (VK_NEXT, "PageDown"),
        "escape" | "esc" => (VK_ESCAPE, "Escape"),
        "pause" => (VK_PAUSE, "Pause"),
        "printscreen" | "prtsc" => (VK_SNAPSHOT, "PrintScreen"),
        "capslock" => (VK_CAPITAL, "CapsLock"),
        "numlock" => (VK_NUMLOCK, "NumLock"),
        "scrolllock" => (VK_SCROLL, "ScrollLock"),
        "minus" => (VK_OEM_MINUS, "Minus"),
        "plus" | "equal" => (VK_OEM_PLUS, "Plus"),
        "comma" => (VK_OEM_COMMA, "Comma"),
        "period" | "dot" => (VK_OEM_PERIOD, "Period"),
        "slash" => (VK_OEM_2, "Slash"),
        "backslash" => (VK_OEM_5, "Backslash"),
        "semicolon" => (VK_OEM_1, "Semicolon"),
        "quote" => (VK_OEM_7, "Quote"),
        "backquote" => (VK_OEM_3, "Backquote"),
        "bracketleft" => (VK_OEM_4, "BracketLeft"),
        "bracketright" => (VK_OEM_6, "BracketRight"),
        "mediaplaypause" | "playpause" | "play" => (VK_MEDIA_PLAY_PAUSE, "MediaPlayPause"),
        "medianext" | "nexttrack" => (VK_MEDIA_NEXT_TRACK, "MediaNext"),
        "mediaprev" | "prevtrack" => (VK_MEDIA_PREV_TRACK, "MediaPrev"),
        "mediastop" | "stop" => (VK_MEDIA_STOP, "MediaStop"),
        _ => return Err(format!("无法识别的按键:{token}")),
    };
    Ok((code.0 as u32, label.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_common_combos() {
        let b = parse_binding(Action::Next, "ctrl+alt+right").unwrap();
        assert_eq!(b.text, "Ctrl+Alt+Right");
        assert_eq!(b.mods, MOD_CONTROL.0 | MOD_ALT.0);
        assert_eq!(b.vk, VK_RIGHT.0 as u32);

        let b = parse_binding(Action::Toggle, "Ctrl Shift F5").unwrap();
        assert_eq!(b.text, "Ctrl+Shift+F5");
        assert_eq!(b.vk, 0x74);

        let b = parse_binding(Action::Prev, "Win+MediaPrev").unwrap();
        assert_eq!(b.text, "Win+MediaPrev");
    }

    #[test]
    fn rejects_unsafe_or_malformed() {
        assert!(parse_binding(Action::Next, "A").is_err(), "裸字母必须拒绝");
        assert!(
            parse_binding(Action::Next, "Ctrl+").is_err(),
            "缺主键必须拒绝"
        );
        assert!(
            parse_binding(Action::Next, "Ctrl+A+B").is_err(),
            "多个主键必须拒绝"
        );
        // 功能键/媒体键允许无修饰键
        assert!(parse_binding(Action::Next, "F8").is_ok());
        assert!(parse_binding(Action::Next, "MediaNext").is_ok());
    }

    #[test]
    fn duplicate_combos_rejected() {
        let err = parse_inputs(Some("Ctrl+Alt+Right"), Some("ctrl+alt+right"), None)
            .err()
            .expect("重复组合必须报错");
        assert!(err.contains("同一个快捷键"));
    }

    #[test]
    fn empty_fields_mean_no_binding() {
        let parsed = parse_inputs(Some("  "), None, Some("Ctrl+Alt+N")).unwrap();
        assert_eq!(parsed.bindings.len(), 1);
        assert_eq!(parsed.prev, None);
        assert_eq!(parsed.next.as_deref(), Some("Ctrl+Alt+N"));
    }
}
