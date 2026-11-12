//! 界面语言。只有两种:简体中文(`zh-CN`)和 English(`en`)。
//!
//! 句子不写在 Rust 里。`locales/zh-CN.txt` 和 `locales/en.txt` 用同一套 key,
//! 一行一条:`settings.theme = 主题`。加一种语言就是复制一份文件再翻译。
//! `catalogs_stay_in_sync` 会核对 key,以及 `{占位符}` 是否两边都有。
//! 调用点只写 `t!("settings.theme")`。
//!
//! 安装包把用户选的语言写到数据目录的 `install-language`(系统语言是对话框的默认项)。
//! 下次启动读走这个文件并写进 settings.json,之后以设置为准,直到用户在界面里改。

use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

pub const ZH_CN: &str = "zh-CN";
pub const EN: &str = "en";

const ZH_ID: u8 = 0;
const EN_ID: u8 = 1;

const ZH_TEXT: &str = include_str!("../locales/zh-CN.txt");
const EN_TEXT: &str = include_str!("../locales/en.txt");

static CURRENT: AtomicU8 = AtomicU8::new(ZH_ID);

pub fn set_current(code: &str) {
    let id = if canonical_language(code) == Some(EN) {
        EN_ID
    } else {
        ZH_ID
    };
    CURRENT.store(id, Ordering::Relaxed);
}

pub fn is_english() -> bool {
    CURRENT.load(Ordering::Relaxed) == EN_ID
}

/// 把各种写法收成 `en` / `zh-CN`。不认识的返回 None。
/// 只有简体中文这一种中文界面,繁体系统也落到简体。
pub fn canonical_language(code: &str) -> Option<&'static str> {
    let c = code
        .trim()
        .trim_matches('\u{feff}')
        .trim_matches('\0')
        .replace('_', "-");
    let lower = c.to_ascii_lowercase();
    if lower == "en" || lower.starts_with("en-") {
        Some(EN)
    } else if lower == "zh"
        || lower.starts_with("zh-")
        || lower == "chinese"
        || lower == "simpchinese"
        || lower == "tradchinese"
    {
        Some(ZH_CN)
    } else {
        None
    }
}

/// 系统界面语言。中文(简体或繁体)→ 简体中文,其余 → English。
pub fn system_language_code() -> &'static str {
    if system_prefers_chinese() { ZH_CN } else { EN }
}

#[cfg(windows)]
fn system_prefers_chinese() -> bool {
    // LANG_CHINESE = 0x04。UI 语言,不是区域格式。
    let lang = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    (lang & 0x3FF) == 0x04
}

#[cfg(not(windows))]
fn system_prefers_chinese() -> bool {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_ascii_lowercase();
            if v.is_empty() || v == "c" || v == "posix" {
                continue;
            }
            return v.starts_with("zh");
        }
    }
    false
}

/// 读走安装包留下的语言标记。文件是一次性的,读完就删。
pub fn take_install_file(dir: &Path) -> Option<&'static str> {
    let path = dir.join("install-language");
    let bytes = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    canonical_language(decode_lang_bytes(&bytes).trim())
}

/// 安装脚本在 Unicode NSIS 下如果用 FileWrite,会写成 UTF-16LE。
/// 我们自己的脚本按字节写 ASCII;两种都认。
fn decode_lang_bytes(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if bytes.len() >= 4
        && bytes.len().is_multiple_of(2)
        && bytes.iter().skip(1).step_by(2).all(|b| *b == 0)
    {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn catalogs() -> (&'static HashMap<String, String>, &'static HashMap<String, String>) {
    static ZH: OnceLock<HashMap<String, String>> = OnceLock::new();
    static EN: OnceLock<HashMap<String, String>> = OnceLock::new();
    let zh = ZH.get_or_init(|| parse_catalog(ZH_TEXT));
    let en = EN.get_or_init(|| parse_catalog(EN_TEXT));
    (zh, en)
}

fn parse_catalog(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(" = ") else {
            panic!("locales line {}: expected 'key = value'", i + 1);
        };
        if map.insert(key.to_string(), value.to_string()).is_some() {
            panic!("duplicate locale key {key}");
        }
    }
    map
}

/// 按当前语言取文案。缺英文时回落到中文,两边都没有就返回 key,方便发现漏翻。
pub fn t(id: &'static str) -> &'static str {
    let (zh, en) = catalogs();
    let map = if is_english() { en } else { zh };
    if let Some(s) = map.get(id) {
        return s.as_str();
    }
    if let Some(s) = zh.get(id) {
        return s.as_str();
    }
    id
}

#[macro_export]
macro_rules! t {
    ($id:literal) => {
        $crate::i18n::t($id)
    };
}

#[macro_export]
macro_rules! tf {
    ($id:literal $(, $key:ident = $val:expr)* $(,)?) => {{
        let mut __s = $crate::i18n::t($id).to_string();
        $(
            __s = __s.replace(concat!("{", stringify!($key), "}"), &($val).to_string());
        )*
        __s
    }};
}

#[cfg(test)]
fn placeholders(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = text[i + 1..].find('}') {
                let name = &text[i + 1..i + 1 + end];
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    out.insert(name.to_string());
                }
                i += end + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_accepts_common_tags() {
        assert_eq!(canonical_language("en"), Some(EN));
        assert_eq!(canonical_language("en-US"), Some(EN));
        assert_eq!(canonical_language("zh-CN"), Some(ZH_CN));
        assert_eq!(canonical_language("zh_TW"), Some(ZH_CN));
        assert_eq!(canonical_language("fr"), None);
        assert_eq!(canonical_language(""), None);
    }

    #[test]
    fn catalogs_stay_in_sync() {
        let zh = parse_catalog(ZH_TEXT);
        let en = parse_catalog(EN_TEXT);
        let mut zk: Vec<_> = zh.keys().cloned().collect();
        let mut ek: Vec<_> = en.keys().cloned().collect();
        zk.sort();
        ek.sort();
        assert_eq!(zk, ek, "zh-CN.txt 和 en.txt 的 key 必须一致");
        for key in &zk {
            assert!(!zh[key].is_empty(), "{key} 的中文是空的");
            assert!(!en[key].is_empty(), "{key} 的英文是空的");
            assert_eq!(
                placeholders(&zh[key]),
                placeholders(&en[key]),
                "{key} 的 {{占位符}} 两边不一致"
            );
        }
    }

    #[test]
    fn install_file_is_consumed_once() {
        let dir = std::env::temp_dir().join(format!("aria-lang-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("install-language"), b"en").unwrap();
        assert_eq!(take_install_file(&dir), Some(EN));
        assert!(!dir.join("install-language").exists());
        assert_eq!(take_install_file(&dir), None);
        let utf16: Vec<u8> = "zh-CN"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        std::fs::write(dir.join("install-language"), utf16).unwrap();
        assert_eq!(take_install_file(&dir), Some(ZH_CN));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
