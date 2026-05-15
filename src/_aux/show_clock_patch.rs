//! 顯示遊戲時鐘
//!
//! 3.8 的底部遊戲時間本來已支援繪製,但進入繪製前會檢查 UI 物件的
//! hover/visibility byte:
//!
//! ```text
//! 0x0078AD47  mov eax, [ebp-0x68]
//! 0x0078AD4A  movzx ecx, byte ptr [eax+0x48]
//! 0x0078AD4E  test ecx, ecx
//! 0x0078AD50  je 0x0078ADF7
//! ```
//!
//! `0x0078AD50` 的 JE 會在未 hover 時跳過 `%02d:%02d` 格式化與文字繪製。
//! 將此 6 bytes NOP 掉後,時間會每 frame 常態顯示。

use anyhow::{bail, Context, Result};
use std::sync::Mutex;
use windows::Win32::Foundation::HANDLE;

use crate::logger::log_line;
use crate::memory;

const CLOCK_GATE_ADDR: u32 = 0x0078AD50;
const PATCH_LEN: usize = 6;
const ORIGINAL_BYTES: [u8; PATCH_LEN] = [0x0F, 0x84, 0xA1, 0x00, 0x00, 0x00];
const PATCHED_BYTES: [u8; PATCH_LEN] = [0x90; PATCH_LEN];

#[derive(Debug, Eq, PartialEq)]
enum GateBytes {
    Original,
    Patched,
    Unexpected,
}

static STATE: Mutex<bool> = Mutex::new(false);

pub fn is_installed() -> bool {
    *STATE.lock().expect("show_clock STATE poisoned")
}

pub fn install(h: HANDLE) -> Result<()> {
    let mut guard = STATE.lock().expect("show_clock STATE poisoned");
    if *guard {
        return Ok(());
    }

    let current = read_gate_bytes(h)?;
    match classify_gate_bytes(&current) {
        GateBytes::Patched => {
            *guard = true;
            log_line!("[show_clock] 已偵測到既有 patch @ 0x{CLOCK_GATE_ADDR:08X}");
            return Ok(());
        }
        GateBytes::Original => {}
        GateBytes::Unexpected => bail!(
            "[show_clock] 0x{CLOCK_GATE_ADDR:08X} bytes 不符合預期: {:02X?}",
            current
        ),
    }

    memory::write_code(h, CLOCK_GATE_ADDR, &PATCHED_BYTES)
        .with_context(|| format!("[show_clock] 寫入 NOP patch 0x{CLOCK_GATE_ADDR:08X} 失敗"))?;

    *guard = true;
    log_line!(
        "[show_clock] 已安裝: 0x{CLOCK_GATE_ADDR:08X} {:02X?} -> {:02X?}",
        ORIGINAL_BYTES,
        PATCHED_BYTES
    );
    Ok(())
}

pub fn uninstall(h: HANDLE) -> Result<()> {
    let mut guard = STATE.lock().expect("show_clock STATE poisoned");
    if !*guard {
        return Ok(());
    }

    let current = read_gate_bytes(h)?;
    match classify_gate_bytes(&current) {
        GateBytes::Original => {
            *guard = false;
            log_line!("[show_clock] 已是原始 bytes @ 0x{CLOCK_GATE_ADDR:08X}");
            return Ok(());
        }
        GateBytes::Patched => {}
        GateBytes::Unexpected => bail!(
            "[show_clock] 0x{CLOCK_GATE_ADDR:08X} bytes 已被其他 patch 修改,拒絕還原: {:02X?}",
            current
        ),
    }

    memory::write_code(h, CLOCK_GATE_ADDR, &ORIGINAL_BYTES)
        .with_context(|| format!("[show_clock] 還原 0x{CLOCK_GATE_ADDR:08X} 失敗"))?;

    *guard = false;
    log_line!(
        "[show_clock] 已卸載: 0x{CLOCK_GATE_ADDR:08X} {:02X?} -> {:02X?}",
        PATCHED_BYTES,
        ORIGINAL_BYTES
    );
    Ok(())
}

fn read_gate_bytes(h: HANDLE) -> Result<Vec<u8>> {
    memory::read_bytes(h, CLOCK_GATE_ADDR, PATCH_LEN)
        .with_context(|| format!("[show_clock] 讀取 0x{CLOCK_GATE_ADDR:08X} 失敗"))
}

fn classify_gate_bytes(bytes: &[u8]) -> GateBytes {
    if bytes == ORIGINAL_BYTES {
        GateBytes::Original
    } else if bytes == PATCHED_BYTES {
        GateBytes::Patched
    } else {
        GateBytes::Unexpected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_original_gate_bytes() {
        assert_eq!(classify_gate_bytes(&ORIGINAL_BYTES), GateBytes::Original);
    }

    #[test]
    fn classifies_patched_gate_bytes() {
        assert_eq!(classify_gate_bytes(&PATCHED_BYTES), GateBytes::Patched);
    }

    #[test]
    fn classifies_unknown_gate_bytes() {
        assert_eq!(
            classify_gate_bytes(&[0x0F, 0x85, 0xA1, 0x00, 0x00, 0x00]),
            GateBytes::Unexpected
        );
        assert_eq!(classify_gate_bytes(&[0x90, 0x90]), GateBytes::Unexpected);
    }
}
