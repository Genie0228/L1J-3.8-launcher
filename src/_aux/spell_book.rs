//! Spell Book reader — 從玩家**已學會**的 spell list 建 `name → packed` 對映表
//!
//! 跟 [`crate::aux::spell_db::SpellDb`] 的差別:
//! - `SpellDb` 是「全 client 所有 level 的技能」,name 第一個出現的 packed 通常是 level 1
//! - `SpellBook` 是「玩家身上學的技能」,packed 對應**玩家實際擁有的 level**
//!
//! ## 為什麼需要
//!
//! `ForceSelfPacket` 路徑(體魄強健術 / 通暢氣脈術 等可指定他人的自身 buff)直接組
//! `C_SKILL` packet 送給 server。server 會驗證「玩家是否學會這個 packed」,如果用
//! `SpellDb` 拿到的 level 1 packed,玩家若只學高 level 版本 → server 拒絕 → 永遠循環。
//!
//! 從 spell_book 拿,packed 一定是玩家學的版本,server 必接受。
//!
//! ## 結構(2026-05-01 實機驗證)
//!
//! ```text
//! [SPELL_BOOK_PTR] (= 0x00C31324)
//!   └─→ spell_book object @ heap
//!         +0x00 (4B) = vftable_ptr (= 0x008EF26C)
//!         +0x2C (4B) = spell count (玩家學的技能數)
//!         +0x58 (4B) = spell array ptr (heap)
//!                       └─→ DWORD[count] of entry pointers
//!                             ├─ entry[0]:
//!                             │   +0x00 (4B) = vftable_ptr (= 0x008EF244)
//!                             │   +0x04 (4B) = packed_skill_id ★
//!                             │   +0x0C (4B) = name_ptr (Big5, " (mp/range[/level])" 字尾)
//!                             ├─ entry[1]: 同上
//!                             └─ ...
//! ```
//!
//! 反組譯來源:`spell_book::cast @ 0x73ECE0` 的查表迴圈
//! (0x73ED2C..0x73ED5E):
//! ```asm
//! mov edx, [rcx + 0x58]    ; spell array
//! mov ecx, [rdx + rax*4]   ; entry = array[i]
//! mov edx, [rcx + 4]       ; packed = entry+4
//! cmp edx, [rbp + 8]       ; == 要找的 packed?
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use parking_lot::RwLock;
use windows::Win32::Foundation::HANDLE;

use crate::aux::address;
use crate::aux::spell_db::{decode_spell_name_bytes, strip_paren_suffix};
use crate::log_line;
use crate::memory::{read_bytes, read_u32};

/// 名稱欄位最長讀多少 bytes
const NAME_MAX_BYTES: usize = 64;

/// `name → packed_skill_id`(玩家學的 level)
///
/// `book_ptr` 是 build 當下從 `[SPELL_BOOK_PTR]` 讀到的 spell_book 物件位址。
/// 換角時遊戲會重新分配 spell_book object,這個值就會變,用來偵測 cache stale。
#[derive(Default, Clone, Debug)]
pub struct SpellBook {
    pub(crate) map: HashMap<String, u32>,
    pub(crate) book_ptr: u32,
}

impl SpellBook {
    /// 從玩家 spell_book 建表 — caller 必須確保已進場(`G_GAME_STATE == 3`)。
    pub fn build(h: HANDLE) -> Result<Self> {
        let book_ptr = read_u32(h, address::SPELL_BOOK_PTR)
            .with_context(|| format!("讀 SPELL_BOOK_PTR @ 0x{:08X}", address::SPELL_BOOK_PTR))?;
        if book_ptr == 0 {
            bail!("SPELL_BOOK_PTR 為 NULL — 玩家可能尚未進場");
        }
        let count = read_u32(h, book_ptr + 0x2C)
            .with_context(|| format!("讀 spell_book.count @ 0x{:08X}", book_ptr + 0x2C))?;
        let array_ptr = read_u32(h, book_ptr + 0x58)
            .with_context(|| format!("讀 spell_book.array @ 0x{:08X}", book_ptr + 0x58))?;
        if count == 0 || array_ptr == 0 {
            bail!("spell_book 空(count={count}, array=0x{array_ptr:08X})");
        }
        // 防呆:count 太誇張代表結構讀錯
        if count > 1024 {
            bail!("spell_book.count={count} 看起來不合理,中斷");
        }

        let array_bytes = read_bytes(h, array_ptr, count as usize * 4)
            .with_context(|| format!("讀 spell array @ 0x{array_ptr:08X}"))?;

        let mut map: HashMap<String, u32> = HashMap::new();

        for i in 0..count as usize {
            let off = i * 4;
            let entry_ptr = u32::from_le_bytes([
                array_bytes[off],
                array_bytes[off + 1],
                array_bytes[off + 2],
                array_bytes[off + 3],
            ]);
            if entry_ptr == 0 {
                continue;
            }

            // 讀 entry 前 16 bytes — 取 packed (+0x04) 和 name_ptr (+0x0C)
            let entry_bytes = match read_bytes(h, entry_ptr, 16) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let packed = u32::from_le_bytes([
                entry_bytes[4],
                entry_bytes[5],
                entry_bytes[6],
                entry_bytes[7],
            ]);
            let name_ptr = u32::from_le_bytes([
                entry_bytes[12],
                entry_bytes[13],
                entry_bytes[14],
                entry_bytes[15],
            ]);
            if name_ptr == 0 {
                continue;
            }

            let raw = match read_bytes(h, name_ptr, NAME_MAX_BYTES) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let null_pos = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            if null_pos == 0 {
                continue;
            }
            let full = decode_spell_name_bytes(&raw[..null_pos]);
            let base = strip_paren_suffix(&full);
            if base.is_empty() {
                continue;
            }

            // 同名時保留**較大** packed(通常 = 較高 level — 玩家最後學的版本)
            map.entry(base.to_string())
                .and_modify(|p| {
                    if packed > *p {
                        *p = packed;
                    }
                })
                .or_insert(packed);
        }

        Ok(SpellBook { map, book_ptr })
    }

    /// 用 INI 寫的技能名稱查 packed_skill_id
    pub fn lookup(&self, name: &str) -> Option<u32> {
        self.map.get(name).copied()
    }

    /// 表內技能數
    pub fn unique_names(&self) -> usize {
        self.map.len()
    }

    /// 列出所有名稱(diagnostic 用,lookup miss 時 dump 候選)。
    pub fn names_iter(&self) -> impl Iterator<Item = &str> {
        self.map.keys().map(|s| s.as_str())
    }

    /// cache 是否對不上當下遊戲狀態(換角後 SPELL_BOOK_PTR 會變)。
    ///
    /// `current_book_ptr == 0` 也視為 stale — 表示遊戲還沒分配,build 也會 fail,
    /// 不如直接 invalidate 讓 caller 走 rebuild 路徑統一處理。
    pub fn is_stale_for(&self, current_book_ptr: u32) -> bool {
        current_book_ptr == 0 || self.book_ptr != current_book_ptr
    }
}

/// 讀目前遊戲全域的 SPELL_BOOK_PTR(= `[0x00C31324]`)。失敗回 0。
pub fn read_current_book_ptr(h: HANDLE) -> u32 {
    read_u32(h, address::SPELL_BOOK_PTR).unwrap_or(0)
}

/// 確保 `spell_book` cache 對應當下遊戲狀態:None / stale 都會 rebuild。
///
/// 回傳 true 代表 cache 現在 fresh 可用;false 代表 build 失敗(玩家未進場 / 結構讀錯)。
///
/// 取代原本 4 個 caller 各自寫的 `is_none() → SpellBook::build` 鏈,新增換角偵測。
/// `tag` 用於 log 來源區分(`hotkey` / `buff` / `status` / `drink` 等)。
pub fn ensure_fresh(
    h: HANDLE,
    spell_book: &Arc<RwLock<Option<SpellBook>>>,
    tag: &str,
) -> bool {
    let current_ptr = read_current_book_ptr(h);
    if let Some(book) = spell_book.read().as_ref() {
        if !book.is_stale_for(current_ptr) {
            return true;
        }
    }
    match SpellBook::build(h) {
        Ok(book) => {
            log_line!(
                "[{tag}] spell_book 載入完成 — 玩家學了 {} 個技能 (book_ptr=0x{:08X})",
                book.unique_names(),
                book.book_ptr
            );
            *spell_book.write() = Some(book);
            true
        }
        Err(e) => {
            log_line!("[{tag}] spell_book 建表失敗: {e:#}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_book(book_ptr: u32) -> SpellBook {
        let mut map = HashMap::new();
        map.insert("加速術".to_string(), 0xDEADBEEF);
        SpellBook { map, book_ptr }
    }

    #[test]
    fn fresh_cache_matching_ptr_is_not_stale() {
        let book = fake_book(0x12345678);
        assert!(!book.is_stale_for(0x12345678));
    }

    #[test]
    fn cache_with_different_ptr_is_stale() {
        // 模擬換角:遊戲為新角色重新分配 spell_book object,位址變了
        let book = fake_book(0x12345678);
        assert!(book.is_stale_for(0xAABBCCDD));
    }

    #[test]
    fn zero_current_ptr_treated_as_stale() {
        // 玩家退選角 / 連線斷,SPELL_BOOK_PTR 還沒重填
        // → 視為 stale(rebuild 也會 fail,但統一走 rebuild 路徑由 build() 報錯)
        let book = fake_book(0x12345678);
        assert!(book.is_stale_for(0));
    }

    #[test]
    fn default_book_with_zero_ptr_is_stale_against_real_ptr() {
        // Default::default() 拿到的 SpellBook book_ptr=0,任何非零當前 ptr 都該視為 stale
        let book = SpellBook::default();
        assert!(book.is_stale_for(0x12345678));
    }
}
