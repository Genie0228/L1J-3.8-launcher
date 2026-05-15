//! 中毒偵測 — 直接讀 `player+0x20` bit 5
//!
//! ### 為什麼這樣做(2026-05-02 真實怪物毒驗證)
//!
//! 之前試的路徑全失敗:
//!   - byte_table[391]:永遠是 0,中毒不寫此 byte
//!   - byte_table[452]:只覆蓋沉默毒,不覆蓋傷害毒
//!   - PoisonHandler @ 0x52DA80:是 dead code(opcode 161 從沒被叫)
//!   - apply_buff @ 0x4EE400:對 GM `.poison 1` 完全不被叫
//!
//! 真正的機關是「**GM `.poison 1` 是不完整的測試指令**」 — 它純粹觸發 server 端
//! HP-damage timer,**不走完整的 status packet 路徑**。
//!
//! 用真實怪物毒測,snapshot diff 在 player struct 找到:
//!   `player+0x20` = 0x00 → 0x20(bit 5 設置)
//!   `player+0x25` = 0x00 → 0x01(中毒種類 byte,1 = 傷害毒)
//!   `player+0x44..0x4B` = 0 → damage source 座標
//!
//! ### 偵測公式
//!
//!   `is_damage_poisoned = (read_u32(player_ptr + 0x20) & 0x20) != 0`
//!
//! 達到的效果:純記憶體讀取,無 hook、無 patch,反偵測零風險;LHX 視窗的解毒
//! 自動化憑此判斷是否要送解毒物品 packet。
//!
//! ### 卡司特毒 / 麻痺毒
//!
//! 待真實怪物觸發後 snapshot diff 找對應 bit。`player+0x25` 的 byte 值很可能
//! 編碼了種類(1 = damage,2 = paralyze, 3 = kasuto?),但目前只驗證 1 = damage。

use anyhow::Result;
use windows::Win32::Foundation::HANDLE;

use crate::aux::address::{G_HASTE_BUFF_TABLE, G_PLAYER_PTR};
use crate::logger::log_line;
use crate::memory::{read_bytes, read_u32};

/// 傷害毒 bit 位於 player+0x20 的 bit 5(= 0x20)
const PLAYER_STATUS_OFFSET: u32 = 0x20;
const POISON_BIT_DAMAGE: u32 = 0x20;

/// `player+0x25` byte 編碼中毒種類(1 = 傷害毒;其他種類 byte 值待驗證)
const PLAYER_POISON_KIND_OFFSET: u32 = 0x25;
const KIND_DAMAGE: u8 = 1;

/// 卡司特毒(= 沉默毒)用 `byte_table[452]`,不是 `player+0x20`。
///
/// 2026-05-01 GM `.poison silence` 觸發 + snapshot diff 驗證 byte_table[452] 由 0→1。
/// 卡司特毒是中文俗稱(來自卡司特族怪物),正式效果就是「不能施法」沉默。
const STATE_SILENCE: u32 = 452;

/// install stub — 不再做事。保留簽名讓 main.rs 不用改。
///
/// 早期版本嘗試過:
///   - inline hook PoisonHandler 的 cmp 後寫 cmd byte(2026-05-01,proven dead code)
///   - inline hook apply_buff/clear_state 入口寫自家 byte_table(2026-05-02 中午,
///     觀察到 GM `.poison 1` 不走 apply_buff,改放棄)
///
/// 最終 2026-05-02 真實怪物毒 snapshot diff 確認 `player+0x20 bit 5` 是中毒 flag,
/// 直接讀就好,不需要 hook。
pub fn install_poison_hook(_h: HANDLE, _pid: u32) -> Result<()> {
    log_line!("[poison] 偵測來源:player+0x20 bit 5(2026-05-02 真實怪物毒驗證)");
    Ok(())
}

/// 讀 player+offset 一個 byte
fn read_player_byte(h: HANDLE, offset: u32) -> Option<u8> {
    let player_ptr = read_u32(h, G_PLAYER_PTR).ok()?;
    if player_ptr == 0 {
        return None;
    }
    read_bytes(h, player_ptr + offset, 1).ok()?.first().copied()
}

/// 讀 player+offset 一個 dword
fn read_player_u32(h: HANDLE, offset: u32) -> Option<u32> {
    let player_ptr = read_u32(h, G_PLAYER_PTR).ok()?;
    if player_ptr == 0 {
        return None;
    }
    read_u32(h, player_ptr + offset).ok()
}

/// 是否中傷害毒(綠色色調毒) — `player+0x20 bit 5`(★★★★)
pub fn is_damage_poisoned(h: HANDLE) -> bool {
    let Some(status) = read_player_u32(h, PLAYER_STATUS_OFFSET) else {
        return false;
    };
    if status & POISON_BIT_DAMAGE == 0 {
        return false;
    }
    // 額外比對 +0x25 byte 編碼的中毒種類,過濾誤報。byte=1 才是傷害毒。
    matches!(read_player_byte(h, PLAYER_POISON_KIND_OFFSET), Some(k) if k == KIND_DAMAGE)
}

/// 是否中麻痺毒。3.8 對映 bit / byte 待真實怪物觸發後驗證,目前固定 false。
#[allow(dead_code)]
pub fn is_paralyze_poisoned(_h: HANDLE) -> bool {
    false
}

/// 是否中卡司特毒。3.8 對映 bit / byte 待真實怪物觸發後驗證,目前固定 false。
#[allow(dead_code)]
pub fn is_kasuto_poisoned(_h: HANDLE) -> bool {
    false
}
