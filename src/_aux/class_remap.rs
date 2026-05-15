//! Per-class state_id remap — 3.8 客戶端 byte_table 對部分 buff 做職業視角重新編號
//!
//! ## 為什麼存在
//!
//! `apply_buff @ 0x4EE400` 寫 `byte_table[0xABF4C8 + state_id] = 1`。
//! 約 10 個 packet 路徑在 server 送同一封包時,client 依玩家職業
//! (`[0xC31544]`)套不同 state_id。例如「行走加速」buff 的 sub_type=4 packet:
//!
//! ```text
//! cmp class, 2  → state_id = 24   (戰士)
//! cmp class, 3  → state_id = 42   (法師)
//! else          → state_id = 37   (其他)
//! ```
//!
//! launcher INI 設定把 state_id 寫死(例如 `Item44=37_行走加速/M`),
//! 此值對「其他職業」直接適用,但對戰士 / 法師會永遠讀到 byte=0 → launcher 誤判
//! 「buff 不在身上」→ 無限重發。此模組就是處理這個 class-specific remap。
//!
//! ## 對映表來源
//!
//! 2026-05-01 靜態盤點 `apply_buff` 全部 522 個 caller,搜尋 `cmp [0xC31544], N`
//! gating pattern,輸出 `auto-analyzer/class_state_remap.json`。
//! 只有少數職業 / state_id 需要 remap;絕大多數 buff 是 class-agnostic。

use windows::Win32::Foundation::HANDLE;

/// 讀玩家職業 byte。失敗時 fallback 0(等同沒 remap)。
pub fn read_class(h: HANDLE) -> u8 {
    crate::memory::read_bytes(h, crate::aux::address::G_CLASS, 1)
        .ok()
        .and_then(|v| v.first().copied())
        .unwrap_or(0)
}

/// 把 INI state_id 轉成本職業實際 byte_table index。沒對映的職業 / id 直接回傳原值。
pub fn remap(class: u8, ini_id: i32) -> i32 {
    match (class, ini_id) {
        // 戰士 (class=2)
        (2, 37) => 24,
        // 法師 (class=3)
        (3, 37) => 42,
        // 妖精 (class=4)
        (4, 14) => 31,
        (4, 15) => 30,
        (4, 16) => 32,
        (4, 17) => 29,
        (4, 140) => 121,
        // 幻術師 (class=6)
        (6, 16) => 116,
        _ => ini_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn law_class_walk_haste_remaps() {
        assert_eq!(remap(3, 37), 42); // 法師: 行走加速
        assert_eq!(remap(2, 37), 24); // 戰士: 行走加速
        assert_eq!(remap(0, 37), 37); // 其他: 不變
    }

    #[test]
    fn agnostic_state_ids_pass_through() {
        // 靈魂昇華 (44) 不在 remap 表 → identity
        assert_eq!(remap(3, 44), 44);
        assert_eq!(remap(2, 10), 10);
    }

    #[test]
    fn elf_remaps() {
        assert_eq!(remap(4, 14), 31);
        assert_eq!(remap(4, 15), 30);
        assert_eq!(remap(4, 16), 32);
        assert_eq!(remap(4, 17), 29);
        assert_eq!(remap(4, 140), 121);
    }
}
