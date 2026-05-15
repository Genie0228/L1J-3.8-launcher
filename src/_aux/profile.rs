//! Per-character 設定持久化 — 進場讀檔、離場存檔。
//!
//! ## 設計
//!
//! 角色名透過指標鏈讀取:`[G_PLAYER_PTR] + 0x60` → name_struct_ptr → null-terminated 字串。
//! 實機驗證:0x00C2D2B8 → 0x285C4058 → +0x60 → 0x24E85F30 → "Sqw887979\0"。
//!
//! 設定存到 launcher.exe 旁邊的 `aux_settings/<charname>.json`,JSON 格式(serde)。
//! 進場(state 0→3)時 HOME 第一次按下 → 讀玩家名 → 載對應 json,沒檔案就用 default。
//! 離場(state 3→0)時 main.rs 偵測到 → save → 關窗 → 殺 scheduler。
//!
//! ## 為什麼不用 INI
//!
//! AuxSettings 結構複雜(嵌套 Vec、enum、固定陣列),手寫 INI parser 要 200+ 行。
//! serde_json 已有,derive 一次解決所有欄位 round-trip。檔名 `*.json`,使用者用記事本能看。
//!
//! ## 安全考量
//!
//! - 玩家名可能含特殊字元(`<>:"/\|?*`)— `sanitize_filename` 全部替換成 `_`
//! - 空字串 / 純空白 → 拒絕,return None(回退 default 設定)
//! - 讀檔失敗(corrupt / 版本不符)→ 用 default,不爆炸
//! - 寫檔失敗(磁碟滿、權限不足)→ log error,不阻塞 launcher

use std::path::PathBuf;

use anyhow::{Context, Result};
use windows::Win32::Foundation::HANDLE;

use crate::aux::address::G_PLAYER_PTR;
use crate::aux::runtime::AuxSettings;
use crate::log_line;
use crate::memory::{read_bytes, read_u32};

/// 玩家名最長 byte 數(Lineage 客戶端限 16 bytes 顯示,加 buffer 到 32 安全)
const NAME_MAX_BYTES: usize = 32;

/// 從遊戲記憶體讀目前進場角色的名字。
///
/// 失敗(指標 NULL、讀取失敗、空字串)→ None。caller 應 fallback default 設定。
///
/// 已驗證 offset chain:`[G_PLAYER_PTR] + 0x60` → name_struct → +0 (string)
pub fn read_player_name(h: HANDLE) -> Option<String> {
    let player_obj = read_u32(h, G_PLAYER_PTR).ok()?;
    if player_obj == 0 {
        return None;
    }
    let name_ptr = read_u32(h, player_obj + 0x60).ok()?;
    if name_ptr == 0 {
        return None;
    }
    let bytes = read_bytes(h, name_ptr, NAME_MAX_BYTES).ok()?;
    let null_pos = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    if null_pos == 0 {
        return None;
    }
    let raw = crate::legacy_text::decode_zstr(&bytes[..null_pos]);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// 設定檔資料夾(launcher.exe 旁邊的 `aux_settings/`),首次呼叫會 mkdir。
fn settings_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("取得 launcher.exe 路徑")?;
    let dir = exe
        .parent()
        .context("launcher.exe 沒有 parent dir")?
        .join("aux_settings");
    if !dir.exists() {
        std::fs::create_dir_all(&dir).with_context(|| format!("建立資料夾 {dir:?}"))?;
    }
    Ok(dir)
}

/// 把角色名變成檔名安全字串(替換 Windows 不允許的字元)
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

/// 角色設定檔的完整路徑
fn profile_path(name: &str) -> Result<PathBuf> {
    let safe = sanitize_filename(name);
    Ok(settings_dir()?.join(format!("{safe}.json")))
}

/// 讀指定角色的設定。檔案不存在或 parse 失敗 → 回 default(log warning)。
pub fn load(name: &str) -> AuxSettings {
    let path = match profile_path(name) {
        Ok(p) => p,
        Err(e) => {
            log_line!("[profile] 取得 {name} 路徑失敗,用預設: {e:#}");
            return AuxSettings::default();
        }
    };
    if !path.exists() {
        log_line!("[profile] {name} 無存檔(首次使用),套用預設設定");
        return AuxSettings {
            current_profile: name.to_string(),
            ..Default::default()
        };
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<AuxSettings>(&text) {
            Ok(mut s) => {
                s.current_profile = name.to_string();
                log_line!("[profile] 載入 {} ({} bytes)", path.display(), text.len());
                s
            }
            Err(e) => {
                log_line!("[profile] {} 解析失敗,改用預設: {e:#}", path.display());
                AuxSettings {
                    current_profile: name.to_string(),
                    ..Default::default()
                }
            }
        },
        Err(e) => {
            log_line!("[profile] 讀 {} 失敗: {e:#}", path.display());
            AuxSettings {
                current_profile: name.to_string(),
                ..Default::default()
            }
        }
    }
}

/// 把 settings 存到指定角色的 json 檔。失敗只 log,不 panic。
pub fn save(name: &str, settings: &AuxSettings) {
    let path = match profile_path(name) {
        Ok(p) => p,
        Err(e) => {
            log_line!("[profile] 存 {name} 取得路徑失敗: {e:#}");
            return;
        }
    };
    let text = match serde_json::to_string_pretty(settings) {
        Ok(t) => t,
        Err(e) => {
            log_line!("[profile] 序列化 {name} 失敗: {e:#}");
            return;
        }
    };
    if let Err(e) = std::fs::write(&path, &text) {
        log_line!("[profile] 寫 {} 失敗: {e:#}", path.display());
        return;
    }
    log_line!("[profile] 存檔 {} ({} bytes)", path.display(), text.len());
}

#[cfg(test)]
mod tests {
    use super::sanitize_filename;

    #[test]
    fn sanitize_keeps_normal_chars() {
        assert_eq!(sanitize_filename("Sqw887979"), "Sqw887979");
        assert_eq!(sanitize_filename("玩家A"), "玩家A");
    }

    #[test]
    fn sanitize_replaces_path_chars() {
        assert_eq!(sanitize_filename("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_filename("a:b?c*"), "a_b_c_");
        assert_eq!(sanitize_filename("a<b>c|d\""), "a_b_c_d_");
    }
}
