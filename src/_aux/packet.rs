//! 封包組裝與發送 — codecave stub 路徑(預留介面,實作待補)
//!
//! 為什麼設計成 codecave 而非 inline call:在遊戲進程內分配 codecave,內含
//! 「push args; call SendPacketData; ret」stub,後續以 WriteProcessMemory 寫參數 +
//! CreateRemoteThread(stub) 觸發。比起每次重新組 shellcode,固定 stub 位址讓
//! caller 只需要寫參數,執行速度快且記憶體足跡小。
//!
//! 階段 1:定義介面 + stub。真正實作走 drink_hook.rs 的 RemoteThread 路徑;
//! 此模組保留給未來需要 codecave persistence 的場景。

use anyhow::Result;
use windows::Win32::Foundation::HANDLE;

/// 發送原始封包（呼叫遊戲的 SendPacketData @ 0x00580E50）
///
/// 參數：
///   h        — 遊戲進程 handle
///   opcode   — 封包 opcode（C→S）
///   payload  — 封包 body（不含 length / opcode header）
#[allow(dead_code)]
pub fn send_packet(_h: HANDLE, _opcode: u8, _payload: &[u8]) -> Result<()> {
    // TODO 階段 4：建立 codecave、組裝 stub、CreateRemoteThread 觸發
    Ok(())
}

/// 使用物品（C_USE_ITEM，opcode 待從 memory/opcode_tables.md 確認）
#[allow(dead_code)]
pub fn use_item(_h: HANDLE, _item_obj_id: u32) -> Result<()> {
    Ok(())
}

/// 施放技能（C_SKILL，opcode 待確認）
#[allow(dead_code)]
pub fn cast_skill(_h: HANDLE, _skill_id: u16, _target_id: u32) -> Result<()> {
    Ok(())
}

/// 刪除物品（C_DELETE_ITEM）
#[allow(dead_code)]
pub fn delete_item(_h: HANDLE, _item_obj_id: u32) -> Result<()> {
    Ok(())
}
