//! 對話框文字滿版 patch — 實測 2026-05-09
//!
//! `AddChatLine` (`0x004378A0`) 內的 wrap byte cap (`mov [ebp-4], 0x37` @ `0x004378BE`)
//! 控制每筆 chat ring entry 最多儲存 byte 數,renderer 將每個 entry 畫一行。
//! patch 立即數從 `0x37` (55 bytes) 改成 `0x44` (68 bytes),讓視覺單行寬度貼齊
//! 對話框可見邊界,消除右側約 35% 空白。
//!
//! - 高解析度模式:wrap = 0x44 = 68 bytes ≈ 34 全形字
//! - 低解析度模式:`0x4378C5-DD` 條件 `sub eax, 0xE` → wrap = 0x36 = 54 bytes ≈ 27 全形字
//!
//! 安全邊界:chat ring entry 結構 +0x00..+0x5F = text(96 bytes),`0x60` 起為
//! color/src 欄位。 wrap ≤ 0x60 不會踩 metadata。 `0x44` 留 28 bytes 餘裕。

use anyhow::{Context, Result};
use windows::Win32::Foundation::HANDLE;

use crate::logger::log_line;
use crate::memory;

/// `mov dword ptr [ebp-4], 0x37` 指令的 32-bit 立即數位址
/// (指令位址 0x004378BE + 3-byte opcode)
const CHAT_WRAP_IMM_ADDR: u32 = 0x0043_78C1;

/// 原 wrap 值 — 0x37 = 55 bytes
const CHAT_WRAP_OLD_VALUE: u32 = 0x37;

/// 新 wrap 值 — 0x44 = 68 bytes(實測貼齊對話框右邊界)
const CHAT_WRAP_NEW_VALUE: u32 = 0x44;

/// 啟動時安裝對話框文字滿版 patch。
///
/// 失敗不中斷啟動 — 此功能屬「nice-to-have 顯示體驗」,失敗 log warning 後 caller 應
/// 繼續其他啟動步驟。
pub fn install_chat_width_patch(h: HANDLE) -> Result<()> {
    // 寫入前先讀回原值確認位置正確(防止位址改版時誤改其他常數)
    let current =
        memory::read_u32(h, CHAT_WRAP_IMM_ADDR).context("讀取 chat wrap immediate 失敗")?;

    if current != CHAT_WRAP_OLD_VALUE && current != CHAT_WRAP_NEW_VALUE {
        log_line!(
            "[ChatWidth] 跳過 — 0x{CHAT_WRAP_IMM_ADDR:08X} 不是預期值(讀到 0x{current:X},預期 0x{CHAT_WRAP_OLD_VALUE:X} 或已 patch 的 0x{CHAT_WRAP_NEW_VALUE:X})"
        );
        return Ok(());
    }

    if current == CHAT_WRAP_NEW_VALUE {
        log_line!(
            "[ChatWidth] 已 patch 過(0x{CHAT_WRAP_IMM_ADDR:08X} = 0x{current:X}),略過重複寫入"
        );
        return Ok(());
    }

    let new_bytes = CHAT_WRAP_NEW_VALUE.to_le_bytes();
    memory::write_code(h, CHAT_WRAP_IMM_ADDR, &new_bytes)
        .context("寫入 chat wrap immediate 失敗")?;

    let after = memory::read_u32(h, CHAT_WRAP_IMM_ADDR).context("讀回 chat wrap immediate 失敗")?;

    if after != CHAT_WRAP_NEW_VALUE {
        log_line!(
            "[ChatWidth] 警告:寫入後讀回不一致 @ 0x{CHAT_WRAP_IMM_ADDR:08X}:讀到 0x{after:X},預期 0x{CHAT_WRAP_NEW_VALUE:X}"
        );
    } else {
        log_line!(
            "[ChatWidth] 對話框寬度 patch 完成:0x{CHAT_WRAP_IMM_ADDR:08X} 0x{CHAT_WRAP_OLD_VALUE:X} → 0x{CHAT_WRAP_NEW_VALUE:X}"
        );
    }

    Ok(())
}
