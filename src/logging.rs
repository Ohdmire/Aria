//! 运行期可开关的文件日志(设置界面"日志记录"开关 + 导出)。
//!
//! 自定义 log::Log 后端:始终写 stderr(开发态可见,替代 env_logger),
//! 开关打开时同时追加写 `%APPDATA%\com.ohdmire.aria\log\aria.log`。
//! 设置进程与壁纸渲染子进程都装同一实现、追加同一文件(单行单次
//! write,跨进程基本不交错),按消息内容区分来源。
//!
//! 子进程不初始化 Tauri,拿不到 app_data_dir,故路径与 tauri.conf.json
//! 的 identifier 手工保持一致。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

struct Sink {
    file: Mutex<Option<File>>,
}

static SINK: OnceLock<Sink> = OnceLock::new();
static MAX_LEVEL: OnceLock<log::LevelFilter> = OnceLock::new();
/// RUST_LOG 是否显式设置(是 = 按它全量放行,排障用;否 = 按来源分级)。
static RUST_LOG_EXPLICIT: OnceLock<bool> = OnceLock::new();

/// 本项目 crate 前缀:这些来源记 Info 及以上;其余第三方(wgpu/naga/
/// symphonia/kira…)只记 Warn 及以上 —— wgpu 系在 Info 级会倾倒
/// DownlevelCapabilities 全文与 Naga 生成的 shader 源码,淹没日志。
const OWNED: [&str; 6] = [
    "aria",
    "osu_replay_render",
    "osu_storyboard_render",
    "osu_parse",
    "realm_db_reader",
    "osu_db",
];

/// 高噪声图形栈:只留 Error —— wgpu 启动时把 DownlevelCapabilities
/// 全文与 D3D12 调试层采样器提示都打在 WARN 级,对现场诊断无用。
const NOISY: [&str; 4] = ["wgpu", "wgpu_core", "wgpu_hal", "naga"];

fn level_for(target: &str) -> log::LevelFilter {
    if RUST_LOG_EXPLICIT.get().copied().unwrap_or(false) {
        return MAX_LEVEL.get().copied().unwrap_or(log::LevelFilter::Info);
    }
    // 前缀匹配不分配:target == p 或 p 后紧跟 "::"
    let matches = |list: &[&str]| {
        list.iter().any(|p| {
            target.starts_with(p)
                && (target.len() == p.len() || target.as_bytes().get(p.len()) == Some(&b':'))
        })
    };
    if matches(&NOISY) {
        log::LevelFilter::Error
    } else if matches(&OWNED) {
        log::LevelFilter::Info
    } else {
        log::LevelFilter::Warn
    }
}

/// 安装全局 logger(父/子进程各调一次;重复调用无害)。
pub fn init() -> Result<(), log::SetLoggerError> {
    // 纯级别形式的 RUST_LOG(如 debug/trace)沿用,其余按来源分级
    let parsed = std::env::var("RUST_LOG")
        .ok()
        .and_then(|s| s.parse::<log::LevelFilter>().ok());
    let _ = RUST_LOG_EXPLICIT.set(parsed.is_some());
    let level = parsed.unwrap_or(log::LevelFilter::Trace);
    let _ = MAX_LEVEL.set(level);
    // 实际过滤在 Logger::enabled 里按 target 分级;set_max_level 只需
    // 不低于任何可能放行的级别
    log::set_max_level(log::LevelFilter::Trace);
    log::set_boxed_logger(Box::new(Logger))
}

struct Logger;

impl log::Log for Logger {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        meta.level() <= level_for(meta.target())
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "[{} {:5} {}] {}",
            timestamp(),
            record.level(),
            record.target(),
            record.args()
        );
        eprintln!("{line}");
        if let Some(sink) = SINK.get() {
            if let Ok(mut guard) = sink.file.lock() {
                if let Some(f) = guard.as_mut() {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
    }

    fn flush(&self) {
        if let Some(sink) = SINK.get() {
            if let Ok(mut guard) = sink.file.lock() {
                if let Some(f) = guard.as_mut() {
                    let _ = f.flush();
                }
            }
        }
    }
}

/// 日志文件路径(%APPDATA%\<identifier>\log\aria.log)。
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var("APPDATA").ok()?;
    Some(
        PathBuf::from(base)
            .join("com.ohdmire.aria")
            .join("log")
            .join("aria.log"),
    )
}

/// 开/关文件记录(开 = 打开追加句柄并建目录;关 = 关闭句柄,stderr 照写)。
pub fn set_enabled(on: bool) {
    set_file(on.then(default_path).flatten());
    log::info!("日志记录已{}", if on { "开启" } else { "关闭" });
}

fn set_file(path: Option<PathBuf>) {
    let sink = SINK.get_or_init(|| Sink {
        file: Mutex::new(None),
    });
    if let Ok(mut guard) = sink.file.lock() {
        *guard = path.and_then(|p| {
            std::fs::create_dir_all(p.parent()?).ok()?;
            OpenOptions::new().create(true).append(true).open(p).ok()
        });
    }
}

/// 导出用:读取当前日志文件(先 flush)。
pub fn read_log() -> Option<String> {
    flush();
    std::fs::read_to_string(default_path()?).ok()
}

fn flush() {
    if let Some(sink) = SINK.get() {
        if let Ok(mut guard) = sink.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.flush();
            }
        }
    }
}

/// UTC 时间戳(env_logger 同款观感):自纪元秒数换算 civil 日期
/// (Howard Hinnant 算法),不引时间库。
fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, day) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        d.subsec_millis()
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, day)
}
