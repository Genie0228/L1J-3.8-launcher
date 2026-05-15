//! Spell DB reader — 從 3.8 客戶端記憶體建 `name → packed_skill_id` 對映表
//!
//! 結構（2026-04-28 ✅ 實機驗證）:
//! ```text
//! [SPELL_DB_PTR] (= 0x009A8ED4)
//!   └─→ heap 上的指標陣列 base
//!         base + packed*4
//!           └─→ spell record:
//!                 +0x00.. : Big5 技能名(null-terminated, 含 " (MP/range)" 字尾)
//!                 +0x10.. : metadata (icon/category 等,本模組不用)
//! ```
//!
//! `packed` 即是 `cast_magic(packed, 0)` 要餵的 packed_skill_id;
//! 不需要 `(spell_id<<3)|level` 編碼 — 它**就是** DB 的線性 index。
//!
//! 名稱配對流程:
//! 1. INI `[AllState]` 行 → 取 `name_part`(如「加速術」)
//! 2. spell_db 掃 0..[`MAX_SCAN`],每 entry 讀名稱、`strip_paren_suffix` 取乾淨名
//! 3. 第一個 base_name == name_part 的 packed 即為該 buff 的施法 ID
//!
//! 為什麼用 `name match` 而非 `state_id` 直接查表:
//! 早期嘗試找「state_id → packed」對映 ushort[],但 3.8 packer 把該表搬走/拆掉
//! 後找不到。每個技能在 spell DB 裡都有唯一名稱,改用 name match 等價且穩定。
//!
//! 跨重啟一致性:
//! `[SPELL_DB_PTR]` 是 .data 全域變數,值在每次遊戲 process 啟動後重新填(heap addr 會變),
//! 但**陣列佈局與 packed 索引不變** — 同一個技能在每個 session 的 packed 都一樣。
//! 所以 `SpellDb::build` 必須在 `G_GAME_STATE == 3` 後才呼叫(進場後才 alloc 完整)。

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use parking_lot::RwLock;
use windows::Win32::Foundation::HANDLE;

use crate::aux::address;
use crate::log_line;
use crate::memory::{read_bytes, read_u32};

/// 掃描上限 — 觀察 3.8 client 實際 entries 約 120,256 已含安全 buffer
const MAX_SCAN: u32 = 256;

/// 每個 spell record 名稱欄位最長讀多少 bytes(超過視為損毀,skip)
const NAME_MAX_BYTES: usize = 64;

pub(crate) fn decode_spell_name_bytes(raw: &[u8]) -> String {
    crate::legacy_text::decode_zstr(raw)
}

/// `name → packed_skill_id` 查詢表
#[derive(Default, Clone, Debug)]
pub struct SpellDb {
    map: HashMap<String, u32>,
    entries: usize,
}

impl SpellDb {
    /// 從遊戲記憶體建表 — caller 必須確保已進場(G_GAME_STATE == 3),否則 [SPELL_DB_PTR] 可能還沒填。
    pub fn build(h: HANDLE) -> Result<Self> {
        // 1. 讀 SPELL_DB_PTR 取得陣列 base
        let array_base = read_u32(h, address::SPELL_DB_PTR)
            .with_context(|| format!("讀 SPELL_DB_PTR @ 0x{:08X}", address::SPELL_DB_PTR))?;
        if array_base == 0 {
            bail!("SPELL_DB_PTR 為 NULL — 玩家可能尚未進場");
        }

        // 2. 一口氣讀整個指標陣列(MAX_SCAN * 4 bytes)
        let array_bytes = read_bytes(h, array_base, MAX_SCAN as usize * 4)
            .with_context(|| format!("讀 spell DB 陣列 @ 0x{array_base:08X}"))?;

        let mut map: HashMap<String, u32> = HashMap::new();
        let mut entries = 0;

        for packed in 0..MAX_SCAN {
            let off = packed as usize * 4;
            let ptr = u32::from_le_bytes([
                array_bytes[off],
                array_bytes[off + 1],
                array_bytes[off + 2],
                array_bytes[off + 3],
            ]);
            if ptr == 0 {
                continue; // 稀疏陣列,空格 skip
            }

            // 3. 讀 spell record 開頭 64 bytes,找 null terminator
            let raw = match read_bytes(h, ptr, NAME_MAX_BYTES) {
                Ok(b) => b,
                Err(_) => continue, // 指標壞掉,skip
            };
            let null_pos = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            if null_pos == 0 {
                continue; // 空字串
            }

            // 4. Big5 / GBK auto decode.
            let full_name = decode_spell_name_bytes(&raw[..null_pos]);

            // 5. 剝 " (MP/range)" 或 " (MP/range/level)" 字尾
            let base = strip_paren_suffix(&full_name);
            if base.is_empty() {
                continue;
            }

            entries += 1;
            // 同名取第一個出現的 packed(level 1 通常 index 較小,符合 INI 預期)
            map.entry(base.to_string()).or_insert(packed);
        }

        Ok(SpellDb { map, entries })
    }

    /// 用 INI 寫的技能名稱查 packed_skill_id
    pub fn lookup(&self, name: &str) -> Option<u32> {
        self.map.get(name).copied()
    }

    /// 表內非空 entry 數(log 用)
    pub fn entries(&self) -> usize {
        self.entries
    }

    /// 不重複的技能名數
    pub fn unique_names(&self) -> usize {
        self.map.len()
    }

    /// 列出所有名稱(diagnostic 用,lookup miss 時 dump 候選)。
    pub fn names_iter(&self) -> impl Iterator<Item = &str> {
        self.map.keys().map(|s| s.as_str())
    }
}

/// 確保 `spell_db` cache 已建。spell_db 是 .data 全域,跨 session 重 build 沒意義
/// (同 process 內部局不變),因此只做 lazy build,不做 stale 偵測。
///
/// 取代原本只在 `buff_tick` 內 inline 的 lazy build — `dispatch_skill` 也呼叫,
/// 讓 timer / hotkey 等不經 buff_tick 的路徑也能正確 build。
///
/// 回傳 true 代表 cache 現在可用。
pub fn ensure_built(h: HANDLE, spell_db: &Arc<RwLock<Option<SpellDb>>>, tag: &str) -> bool {
    if spell_db.read().is_some() {
        return true;
    }
    match SpellDb::build(h) {
        Ok(db) => {
            log_line!(
                "[{tag}] spell DB 載入完成 — 共 {} 個 entries / {} 個唯一名稱",
                db.entries(),
                db.unique_names()
            );
            *spell_db.write() = Some(db);
            true
        }
        Err(e) => {
            log_line!("[{tag}] spell DB 建表失敗: {e:#}");
            false
        }
    }
}

/// 剝 spell record 名稱的 " (40/0)" / " (40/30/2)" 等字尾,回傳基底名
///
/// 規則:從尾端往回找最後一個 `(`,只剝「整段括號內僅含數字、`/`、空白」的尾巴。
/// 「(」前的空格可有可無 — 魔法系是「加速術 (40/0)」有空格,妖精物理系
/// (三重矢/集中射等)是「三重矢(15/0)」無空格,兩者都要接。
/// 名稱本身有括號但內容非數字(例如「怪怪 (測試)」)則保留不剝。
pub(crate) fn strip_paren_suffix(name: &str) -> &str {
    let trimmed = name.trim();
    let Some(open) = trimmed.rfind('(') else {
        return trimmed;
    };
    let Some(close_rel) = trimmed[open..].rfind(')') else {
        return trimmed;
    };
    let inner = &trimmed[open + 1..open + close_rel];
    if !inner.is_empty()
        && inner
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'/' || b == b' ')
    {
        return trimmed[..open].trim_end();
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_spell_name_bytes_accepts_simplified_gbk() {
        let name = "\u{52A0}\u{901F}\u{672F} (40/0)";
        let (bytes, _, had_errors) = encoding_rs::GBK.encode(name);
        assert!(!had_errors);

        assert_eq!(decode_spell_name_bytes(&bytes), name);
    }

    #[test]
    fn decode_spell_name_bytes_accepts_traditional_big5() {
        let name = "\u{52A0}\u{901F}\u{8853} (40/0)";
        let (bytes, _, had_errors) = encoding_rs::BIG5.encode(name);
        assert!(!had_errors);

        assert_eq!(decode_spell_name_bytes(&bytes), name);
    }

    #[test]
    fn strip_paren_suffix_basic() {
        assert_eq!(strip_paren_suffix("加速術 (40/0)"), "加速術");
        assert_eq!(strip_paren_suffix("魔法相剋術 (40/0/2)"), "魔法相剋術");
        assert_eq!(strip_paren_suffix("造痕術 (5/80/1)"), "造痕術");
        assert_eq!(strip_paren_suffix("初級瘟疫術 (4/0)"), "初級瘟疫術");
    }

    #[test]
    fn strip_paren_suffix_no_space_before_paren() {
        // 妖精物理系 spell record 沒空格 — 例如三重矢/集中射(packed=0x83/0x85)
        assert_eq!(strip_paren_suffix("三重矢(15/0)"), "三重矢");
        assert_eq!(strip_paren_suffix("集中射(10/0/1)"), "集中射");
        assert_eq!(strip_paren_suffix("理解屬性(10/0)"), "理解屬性");
    }

    #[test]
    fn strip_paren_suffix_no_suffix() {
        assert_eq!(strip_paren_suffix("加速術"), "加速術");
        assert_eq!(strip_paren_suffix(""), "");
    }

    #[test]
    fn strip_paren_suffix_non_numeric() {
        // 括號內含中文,不剝
        assert_eq!(strip_paren_suffix("怪怪 (測試)"), "怪怪 (測試)");
    }
}
