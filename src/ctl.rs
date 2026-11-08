//! 壁纸子进程控制(Tauri 已移除,只保留 GPUI 前端需要的最小接口)。

use std::io::Write;
use std::process::{Child, ChildStdin};

use crate::ipc::Command;

pub struct ChildCtl {
    pub child: Child,
    stdin: std::sync::Mutex<ChildStdin>,
}

impl ChildCtl {
    pub fn from_child(child: Child, stdin: ChildStdin) -> Self {
        Self { child, stdin: std::sync::Mutex::new(stdin) }
    }

    /// 直接下发拼好的 JSON 行(serde 默认值兜底)。
    pub fn send_raw(&self, line: &str) -> Result<(), String> {
        let mut w = self.stdin.lock().map_err(|e| e.to_string())?;
        w.write_all(line.as_bytes())
            .and_then(|_| w.write_all(b"\n"))
            .map_err(|e| e.to_string())
    }

    pub fn send(&self, cmd: &Command) -> Result<(), String> {
        let mut line = serde_json::to_string(cmd).map_err(|e| e.to_string())?;
        line.push('\n');
        let mut w = self.stdin.lock().map_err(|e| e.to_string())?;
        w.write_all(line.as_bytes()).map_err(|e| e.to_string())
    }

    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}
