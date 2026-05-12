//! 除錯日誌 — 寫到 %LOCALAPPDATA%\Lineage38Launcher\dgvoodoo\ime_debug.log
//!
//! DLL 在遊戲 process 內,沒 console 也沒 stdout,只能寫檔案。

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;
use std::time::SystemTime;

static LOCK: Mutex<()> = Mutex::new(());

fn log_path() -> Option<std::path::PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok()?;
    Some(
        std::path::PathBuf::from(local)
            .join("Lineage38Launcher")
            .join("dgvoodoo")
            .join("ime_debug.log"),
    )
}

pub fn log(msg: &str) {
    let _g = LOCK.lock();
    if let Some(p) = log_path() {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&p) {
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = writeln!(f, "[{now}] {msg}");
        }
    }
}

#[macro_export]
macro_rules! dbg_log {
    ($($arg:tt)*) => {
        $crate::dbg::log(&format!($($arg)*))
    };
}
