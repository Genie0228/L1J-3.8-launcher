//! 模擬按鍵到遊戲視窗 — `/KEY=Fn` 與 `/DKEY=Fn` 共用入口。
//!
//! 兩種傳遞策略:
//! 1. **PostMessage**(目前實作):送 WM_KEYDOWN/WM_KEYUP 到遊戲視窗 message queue,
//!    遊戲在自己的 message pump 內 dispatch — 不需要 launcher 是 foreground。
//!    缺點:DirectInput / 低層 hook 抓不到(但天堂 client 用 WM_KEYDOWN 流不影響)。
//! 2. **SendInput**(備案,未啟用):全域 input event,需要 launcher 視窗 foreground。
//!
//! `delayed=true` (`/DKEY`) 在 down 與 up 之間插 ~80ms,模擬玩家「按住」效果,
//! 用於需要長按才生效的指令(例如某些連發魔法)。
//!
//! 視窗標題用 `find_game_window()` 找,跟 launcher 主程式同樣方式。

use anyhow::{anyhow, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_KEYDOWN, WM_KEYUP};

/// `Fn` (n=1..12) 對應 Win32 VK code:VK_F1=0x70 ... VK_F12=0x7B
fn fkey_vk(n: u8) -> Option<u32> {
    if (1..=12).contains(&n) {
        Some(0x6F + n as u32) // VK_F1=0x70 即 0x6F+1
    } else {
        None
    }
}

/// 找 Lineage 客戶端視窗(同 launcher 主程式)
fn find_game_window() -> Result<HWND> {
    let title: Vec<u16> = "Lineage Windows Client (13081901)"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let hwnd = unsafe { FindWindowW(PCWSTR::null(), PCWSTR(title.as_ptr())) }
        .map_err(|e| anyhow!("FindWindowW 失敗: {e:#}"))?;
    if hwnd.0.is_null() {
        return Err(anyhow!("找不到 Lineage 視窗"));
    }
    Ok(hwnd)
}

/// 對遊戲視窗模擬一次 Fn 按鍵。
///
/// `n`:1..12;`delayed`:true → down 與 up 間 sleep 80ms。
///
/// 失敗條件:
/// - `n` 不在範圍 → 立即 Err
/// - 視窗找不到 → Err
/// - PostMessage 失敗(視窗已關閉/handle 失效)→ Err
pub fn press_fkey(n: u8, delayed: bool) -> Result<()> {
    let vk = fkey_vk(n).ok_or_else(|| anyhow!("F{} 超出範圍 (僅支援 F1..F12)", n))?;
    let hwnd = find_game_window()?;

    // 構造 lParam:bit0..15=repeat=1, bit16..23=scancode(F1=0x3B..F12=0x58),
    // bit24=extended(0), bit29=context(0), bit30=prev_state, bit31=transition
    let scancode_base: u32 = match n {
        1..=10 => 0x3B + (n as u32 - 1),
        11 => 0x57,
        12 => 0x58,
        _ => 0,
    };
    let lparam_down: usize = 1 | (scancode_base << 16) as usize;
    let lparam_up: usize = lparam_down | (1 << 30) | (1 << 31);

    unsafe {
        PostMessageW(
            Some(hwnd),
            WM_KEYDOWN,
            WPARAM(vk as usize),
            LPARAM(lparam_down as isize),
        )
        .map_err(|e| anyhow!("PostMessageW WM_KEYDOWN F{n} 失敗: {e:#}"))?;
    }

    if delayed {
        std::thread::sleep(std::time::Duration::from_millis(80));
    }

    unsafe {
        PostMessageW(
            Some(hwnd),
            WM_KEYUP,
            WPARAM(vk as usize),
            LPARAM(lparam_up as isize),
        )
        .map_err(|e| anyhow!("PostMessageW WM_KEYUP F{n} 失敗: {e:#}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fkey_vk_range() {
        assert_eq!(fkey_vk(1), Some(0x70));
        assert_eq!(fkey_vk(12), Some(0x7B));
        assert_eq!(fkey_vk(0), None);
        assert_eq!(fkey_vk(13), None);
    }
}
