//! 遊戲右鍵選單注入(Phase 2)
//!
//! 對齊使用者要求 — 右鍵物品時,在原生「鑑定 / 溶解」之後 append:
//! - `加到溶解名單` → push 進 `AuxSettings.dissolve_list`
//! - `加到刪除名單` → push 進 `AuxSettings.delete_list`
//! - `名稱複製` → 把物品名抓進系統剪貼簿(`arboard`)
//!
//! ## 機制(Task 9~12 填細節)
//!
//! launcher 端:
//! 1. `install()`:VirtualAllocEx codecave → 寫三個 click handler shellcode + ring buffer +
//!    label 字串 → inline hook 選單建構函式末段
//! 2. `poll_ring(h, settings)`:每 tick 從 ring_head 讀到 ring_tail,派發到對應 list 或剪貼簿
//!
//! shellcode click handler(每個 ~37 bytes):
//! 1. 從 RE 拿到的位址讀當下右鍵物品 obj_id
//! 2. 寫進 `ring_buffer[ring_tail % 16] = (action_tag, obj_id)`
//! 3. `ring_tail++` 後 ret
//!
//! ## 已知地址(Task 9 RE 確認後填)
//!
//! - 選單建構函式入口 + 末段 hook 點:`MENU_BUILDER_HOOK`
//! - 「目前右鍵物品」存放位址:`CURRENT_RIGHT_CLICK_ITEM`
//! - 遊戲既有 `add_menu_entry` 函式 + ABI:`ADD_MENU_ENTRY`

use anyhow::Result;
use parking_lot::RwLock;
use std::sync::Arc;
use windows::Win32::Foundation::HANDLE;

use crate::aux::runtime::AuxSettings;

/// Ring buffer entry(launcher 跟 shellcode 共享布局)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct RingEntry {
    action_tag: u32, // 1=Dissolve, 2=Delete, 3=CopyName
    item_obj_id: u32,
}

#[allow(dead_code)]
const RING_SIZE: usize = 16;

/// Phase 2 install — 接遊戲右鍵選單,注入三個 entry。
///
/// **TODO(Task 9~11):** RE 完成後填入 codecave/inline hook 邏輯。目前回 bail。
pub fn install(_h: HANDLE) -> Result<MenuInjectControl> {
    anyhow::bail!("Phase 2 menu inject 尚未實作 — 等 Task 9~11");
}

/// 注入後的 launcher 端 control(handle 回收用)
#[allow(dead_code)]
pub struct MenuInjectControl {
    /// codecave 起始位址(VirtualFree 用)
    pub codecave: u32,
    /// ring_head 在 codecave 內的 offset
    pub ring_head_offset: u32,
    /// ring_tail 在 codecave 內的 offset
    pub ring_tail_offset: u32,
    /// ring_buffer 在 codecave 內的 offset
    pub ring_buffer_offset: u32,
}

/// 從 ring buffer 拉取 pending entries,派發到對應 list 或剪貼簿。
///
/// **TODO(Task 12):** 真實實作 — 目前回 0 不做事。
pub fn poll_ring(
    _h: HANDLE,
    _ctrl: &MenuInjectControl,
    _settings: &Arc<RwLock<AuxSettings>>,
) -> Result<usize> {
    Ok(0)
}
