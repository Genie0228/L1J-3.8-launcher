//! 遊戲內 chat 注入 — 兩條路徑
//!
//! 路徑 A:**直接寫 buffer + bump index**(`push_chat_line`,僅供測試)
//!   - chat ring buffer @ 0x00996D00,150 entries × 0x126 bytes
//!   - 寫入索引 @ 0x00980EA0,(index + 1) % 150 自動 wrap
//!   - 不需 codecave / CreateRemoteThread,只 WriteProcessMemory
//!   - **缺陷**:bypass 0x00437500 ChatDispatch → 不觸發 0x00437D30
//!     ChatSideEffect → 玩家如果先前往上捲過,聊天框不再 auto-tail 到底
//!
//! 路徑 B:**CreateRemoteThread 呼叫 ChatDispatch(0x00437500) 並送 channel=-1**
//!   (`push_chat_via_dispatch`,正式啟動通知用)
//!   - channel=-1 路由至 0x004378A0(顯示)+ 0x00437D30(scroll/sound 副作用)
//!   - 同步 wait,啟動只呼叫一次,主執行緒此時尚未渲染聊天 UI,race 風險低
//!   - 用於 LinHelperZ-執行中 啟動字

use anyhow::{Context, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateRemoteThread, WaitForSingleObject};

use crate::memory;

const CHAT_BUFFER_BASE: u32 = 0x0099_6D00;
const CHAT_INDEX_ADDR: u32 = 0x0098_0EA0;
const CHAT_ENTRY_SIZE: u32 = 0x126;
const CHAT_RING_SIZE: u32 = 150;

const CHAT_DISPATCH_FN: u32 = 0x0043_7500;

/// RGB565 預設色(實測 2026-04-28 對應 \F0~\F4 palette)
pub mod color {
    pub const BLUE: u16 = 0x72DA; // \F0
    pub const YELLOW: u16 = 0xFEC9; // \F1
    pub const GREEN_PALETTE: u16 = 0x87CA; // \F2 — 偏淡綠
    pub const PURPLE: u16 = 0xF314; // \F3
    pub const WHITE: u16 = 0xFF96; // \F4
    pub const RED: u16 = 0xF800; // 純紅(R=31,G=0,B=0)
    pub const GREEN: u16 = 0x07E0; // 純綠(R=0,G=63,B=0)
}

/// 路徑 A — 直接寫 ring buffer。**不觸發 auto-tail 副作用**。
///
/// `text_bytes` 必須是 ASCII 或 **Big5 已編碼** byte sequence。
pub fn push_chat_line(h: HANDLE, text_bytes: &[u8], color: u16) -> Result<()> {
    let index = memory::read_u32(h, CHAT_INDEX_ADDR)?;
    if index >= CHAT_RING_SIZE {
        anyhow::bail!("chat index {} 超出 ring size {}", index, CHAT_RING_SIZE);
    }
    let slot_addr = CHAT_BUFFER_BASE + index * CHAT_ENTRY_SIZE;

    let mut entry = vec![0u8; CHAT_ENTRY_SIZE as usize];
    let text_len = text_bytes.len().min(95);
    entry[..text_len].copy_from_slice(&text_bytes[..text_len]);
    entry[0x60..0x62].copy_from_slice(&color.to_le_bytes());
    entry[0x62..0x64].copy_from_slice(&0xFFFF_u16.to_le_bytes());

    memory::write_code(h, slot_addr, &entry)?;

    let new_index = (index + 1) % CHAT_RING_SIZE;
    memory::write_code(h, CHAT_INDEX_ADDR, &new_index.to_le_bytes())?;

    Ok(())
}

/// 路徑 B — CreateRemoteThread 呼叫 ChatDispatch(0x00437500)。
///
/// 簽名(假設):`(char* text, WORD src_id, WORD color, int channel, int p5)` cdecl。
/// `channel = -1` 時函數內部分別呼叫 0x004378A0(顯示)+ 0x00437D30(副作用)。
///
/// 注意:
/// - 配 codecave 後**不釋放**(thread 退出後 ChatSideEffect 可能仍引用 text 字串)
/// - 啟動只呼叫一次,記憶體浪費可忽略
pub fn push_chat_via_dispatch(h: HANDLE, text_bytes: &[u8], src_id: u16, color: u16) -> Result<()> {
    let mut text_with_null: Vec<u8> = text_bytes.to_vec();
    text_with_null.push(0);
    let text_len = text_with_null.len();

    // text + shellcode (預估 ~32 bytes)
    let total = text_len + 64;
    let base = memory::alloc_exec(h, total)?;
    let text_addr = base;
    let sc_addr = base + text_len as u32;

    memory::write_code(h, text_addr, &text_with_null)?;

    // ChatDispatch(text, src_id, color, -1, 0)
    //   68 ?? ?? ?? ??       push 0                 ; p5
    //   6A FF                push -1                ; channel
    //   68 ?? ?? ?? ??       push <color as u32>
    //   68 ?? ?? ?? ??       push <src_id as u32>
    //   68 ?? ?? ?? ??       push <text_addr>
    //   B8 ?? ?? ?? ??       mov eax, 0x00437500
    //   FF D0                call eax
    //   83 C4 14             add esp, 0x14          ; cdecl 5 args cleanup
    //   33 C0                xor eax, eax
    //   C2 04 00             ret 4                  ; stdcall ThreadProc 收尾
    let mut sc: Vec<u8> = Vec::with_capacity(32);
    sc.push(0x68);
    sc.extend_from_slice(&0u32.to_le_bytes());
    sc.push(0x6A);
    sc.push(0xFF);
    sc.push(0x68);
    sc.extend_from_slice(&(color as u32).to_le_bytes());
    sc.push(0x68);
    sc.extend_from_slice(&(src_id as u32).to_le_bytes());
    sc.push(0x68);
    sc.extend_from_slice(&text_addr.to_le_bytes());
    sc.push(0xB8);
    sc.extend_from_slice(&CHAT_DISPATCH_FN.to_le_bytes());
    sc.push(0xFF);
    sc.push(0xD0);
    sc.push(0x83);
    sc.push(0xC4);
    sc.push(0x14);
    sc.push(0x33);
    sc.push(0xC0);
    sc.push(0xC2);
    sc.push(0x04);
    sc.push(0x00);

    memory::write_code(h, sc_addr, &sc)?;

    unsafe {
        let mut tid = 0u32;
        let thread_handle = CreateRemoteThread(
            h,
            None,
            0,
            Some(std::mem::transmute::<
                usize,
                unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
            >(sc_addr as usize)),
            None,
            0,
            Some(&mut tid),
        )
        .context("CreateRemoteThread(ChatDispatch)")?;

        let wait = WaitForSingleObject(thread_handle, 5000);
        let _ = CloseHandle(thread_handle);

        if wait != WAIT_OBJECT_0 {
            anyhow::bail!(
                "ChatDispatch shellcode 等待逾時 (wait={:?}, tid={})",
                wait,
                tid
            );
        }
    }

    Ok(())
}

/// 推 LinHelperZ 啟動訊息(綠字)。
///
/// 顯示文字: `LinHelperZ-執行中`。中文「執行中」以 Big5 hardcoded(B0F5 A6E6 A4A4)。
/// 走路徑 B(ChatDispatch + channel=-1)以保留 auto-tail 行為。
///
/// 色碼用 `\F2`(palette 綠 = 0x87CA 淡綠)前綴內嵌在 text 裡,由 AddChatLine
/// 函數開頭的 prefix parser 處理 — 不依賴 ChatDispatch 第幾個 arg 是 color。
pub fn push_lhx_started(h: HANDLE) -> Result<()> {
    push_chat_via_dispatch(
        h,
        b"\\F2LinHelperZ-\xB0\xF5\xA6\xE6\xA4\xA4",
        0xFFFF,
        color::GREEN,
    )
}
