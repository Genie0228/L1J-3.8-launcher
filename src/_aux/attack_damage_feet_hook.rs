//! 攻擊傷害顯示 — 怪物腳下版本(箭頭朝上)
//!
//! 啟用時:launcher 觸發的傷害氣泡(色碼 BGR565 紅 0xF800)會落在怪物腳下,
//! 並把氣泡的尾巴翻成從上方往下指(指向上面的怪物)。聊天泡泡 / 金幣
//! 顯示 / 系統訊息 不受影響(色碼過濾,紅色不會跟白色聊天撞色)。
//!
//! 四個 patch 點:
//! - 0x0042B9D2: LOCAL Y 錨點寫入(target == self 時走的分支)
//! - 0x0042B9FB: REMOTE Y 錨點寫入(target 為怪物時走的分支)
//! - 0x0042BAFB: 0x42AC80 回傳後 — 寫 +0x36A=4 翻箭頭
//! - 0x0042AE0C: 0x42AC80 內部 reset — 對紅色傷害氣泡跳過重設,讓 +0x36A=4 持續每幀
//!
//! 配合 attack_damage_hook 一起裝。單獨啟用沒意義(沒有紅色傷害氣泡來源)。

use anyhow::{bail, Context, Result};
use std::sync::Mutex;
use windows::Win32::Foundation::HANDLE;

use crate::logger::log_line;
use crate::memory;

const LOCAL_HOOK_ADDR: u32 = 0x0042_B9D2;
const REMOTE_HOOK_ADDR: u32 = 0x0042_B9FB;
const POST_AC80_ADDR: u32 = 0x0042_BAFB;
const AC80_RESET_ADDR: u32 = 0x0042_AE0C;

const LOCAL_HOOK_LEN: usize = 6;
const REMOTE_HOOK_LEN: usize = 6;
const POST_AC80_LEN: usize = 5;
const AC80_RESET_LEN: usize = 7;

const LOCAL_ORIGINAL: [u8; LOCAL_HOOK_LEN] = [0x89, 0x8A, 0x80, 0x03, 0x00, 0x00];
const REMOTE_ORIGINAL: [u8; REMOTE_HOOK_LEN] = [0x89, 0x81, 0x80, 0x03, 0x00, 0x00];
const POST_AC80_ORIGINAL: [u8; POST_AC80_LEN] = [0x0F, 0xB6, 0xC0, 0x85, 0xC0];
const AC80_RESET_ORIGINAL: [u8; AC80_RESET_LEN] =
    [0xC6, 0x81, 0x6A, 0x03, 0x00, 0x00, 0x00];

const RETURN_AFTER_POSITION: u32 = 0x0042_BA01;
const RETURN_AFTER_POST_AC80: u32 = 0x0042_BB00;
const RETURN_AFTER_AC80_RESET: u32 = 0x0042_AE13;

const DAMAGE_COLOR_LO: u8 = 0x00;
const DAMAGE_COLOR_HI: u8 = 0xF8;
const FEET_PAD: u8 = 0x20;

const CODECAVE_SIZE: usize = 0x400;

#[derive(Debug)]
struct HookState {
    codecave_addr: u32,
    local_patch: [u8; LOCAL_HOOK_LEN],
    remote_patch: [u8; REMOTE_HOOK_LEN],
    post_ac80_patch: [u8; POST_AC80_LEN],
    ac80_reset_patch: [u8; AC80_RESET_LEN],
}

static STATE: Mutex<Option<HookState>> = Mutex::new(None);

pub fn is_installed() -> bool {
    STATE
        .lock()
        .expect("attack_damage_feet STATE poisoned")
        .is_some()
}

pub fn install(h: HANDLE) -> Result<()> {
    let mut guard = STATE.lock().expect("attack_damage_feet STATE poisoned");
    if guard.is_some() {
        return Ok(());
    }

    ensure_original(h, LOCAL_HOOK_ADDR, &LOCAL_ORIGINAL, "local")?;
    ensure_original(h, REMOTE_HOOK_ADDR, &REMOTE_ORIGINAL, "remote")?;
    ensure_original(h, POST_AC80_ADDR, &POST_AC80_ORIGINAL, "post_ac80")?;
    ensure_original(h, AC80_RESET_ADDR, &AC80_RESET_ORIGINAL, "ac80_reset")?;

    let cave = memory::alloc_exec(h, CODECAVE_SIZE)
        .context("[attack_damage_feet] alloc codecave")?;

    let layout = build_shellcode(cave);
    if layout.bytes.len() > CODECAVE_SIZE {
        bail!(
            "[attack_damage_feet] shellcode too large: {} > {}",
            layout.bytes.len(),
            CODECAVE_SIZE
        );
    }
    memory::write_code(h, cave, &layout.bytes)
        .context("[attack_damage_feet] write codecave")?;

    let local_patch = build_jmp_patch::<LOCAL_HOOK_LEN>(
        LOCAL_HOOK_ADDR,
        cave + layout.local_offset as u32,
    );
    memory::write_code(h, LOCAL_HOOK_ADDR, &local_patch)
        .context("[attack_damage_feet] patch local hook")?;

    let remote_patch = build_jmp_patch::<REMOTE_HOOK_LEN>(
        REMOTE_HOOK_ADDR,
        cave + layout.remote_offset as u32,
    );
    memory::write_code(h, REMOTE_HOOK_ADDR, &remote_patch)
        .context("[attack_damage_feet] patch remote hook")?;

    let post_ac80_patch = build_jmp_patch::<POST_AC80_LEN>(
        POST_AC80_ADDR,
        cave + layout.post_ac80_offset as u32,
    );
    memory::write_code(h, POST_AC80_ADDR, &post_ac80_patch)
        .context("[attack_damage_feet] patch post_ac80 hook")?;

    let ac80_reset_patch = build_jmp_patch::<AC80_RESET_LEN>(
        AC80_RESET_ADDR,
        cave + layout.ac80_reset_offset as u32,
    );
    memory::write_code(h, AC80_RESET_ADDR, &ac80_reset_patch)
        .context("[attack_damage_feet] patch ac80_reset hook")?;

    *guard = Some(HookState {
        codecave_addr: cave,
        local_patch,
        remote_patch,
        post_ac80_patch,
        ac80_reset_patch,
    });

    log_line!(
        "[attack_damage_feet] installed @ codecave 0x{cave:08X} (local=+0x{:X} remote=+0x{:X} post_ac80=+0x{:X} ac80_reset=+0x{:X})",
        layout.local_offset,
        layout.remote_offset,
        layout.post_ac80_offset,
        layout.ac80_reset_offset
    );
    Ok(())
}

pub fn uninstall(h: HANDLE) -> Result<()> {
    let mut guard = STATE.lock().expect("attack_damage_feet STATE poisoned");
    let Some(state) = guard.take() else {
        return Ok(());
    };

    restore(
        h,
        AC80_RESET_ADDR,
        &AC80_RESET_ORIGINAL,
        &state.ac80_reset_patch,
        "ac80_reset",
    )?;
    restore(
        h,
        POST_AC80_ADDR,
        &POST_AC80_ORIGINAL,
        &state.post_ac80_patch,
        "post_ac80",
    )?;
    restore(
        h,
        REMOTE_HOOK_ADDR,
        &REMOTE_ORIGINAL,
        &state.remote_patch,
        "remote",
    )?;
    restore(
        h,
        LOCAL_HOOK_ADDR,
        &LOCAL_ORIGINAL,
        &state.local_patch,
        "local",
    )?;

    log_line!(
        "[attack_damage_feet] uninstalled (codecave 0x{:08X} 保留未釋放)",
        state.codecave_addr
    );
    Ok(())
}

fn ensure_original<const N: usize>(
    h: HANDLE,
    addr: u32,
    original: &[u8; N],
    label: &str,
) -> Result<()> {
    let current = memory::read_bytes(h, addr, N)
        .with_context(|| format!("[attack_damage_feet] read {label} @ 0x{addr:08X}"))?;
    if current.as_slice() == original.as_slice() {
        Ok(())
    } else {
        bail!(
            "[attack_damage_feet] {label} bytes mismatch @ 0x{addr:08X}: {:02X?}",
            current
        )
    }
}

fn restore<const N: usize>(
    h: HANDLE,
    addr: u32,
    original: &[u8; N],
    patch: &[u8; N],
    label: &str,
) -> Result<()> {
    let current = memory::read_bytes(h, addr, N)
        .with_context(|| format!("[attack_damage_feet] read {label} @ 0x{addr:08X}"))?;
    if current.as_slice() == original.as_slice() {
        return Ok(());
    }
    if current.as_slice() == patch.as_slice() {
        return memory::write_code(h, addr, original)
            .with_context(|| format!("[attack_damage_feet] restore {label} @ 0x{addr:08X}"));
    }
    bail!(
        "[attack_damage_feet] refuse to restore unexpected {label} bytes @ 0x{addr:08X}: {:02X?}",
        current
    )
}

fn build_jmp_patch<const N: usize>(hook_addr: u32, target: u32) -> [u8; N] {
    debug_assert!(N >= 5);
    let rel = (target as i64 - (hook_addr as i64 + 5)) as i32;
    let mut patch = [0x90u8; N];
    patch[0] = 0xE9;
    patch[1..5].copy_from_slice(&rel.to_le_bytes());
    patch
}

struct Layout {
    bytes: Vec<u8>,
    local_offset: usize,
    remote_offset: usize,
    post_ac80_offset: usize,
    ac80_reset_offset: usize,
}

fn build_shellcode(cave: u32) -> Layout {
    let mut bytes = Vec::<u8>::new();

    let remote_offset = bytes.len();
    emit_position_remote(&mut bytes, cave + remote_offset as u32);

    let local_offset = bytes.len();
    emit_position_local(&mut bytes, cave + local_offset as u32);

    let post_ac80_offset = bytes.len();
    emit_post_ac80(&mut bytes, cave + post_ac80_offset as u32);

    let ac80_reset_offset = bytes.len();
    emit_ac80_reset(&mut bytes, cave + ac80_reset_offset as u32);

    Layout {
        bytes,
        local_offset,
        remote_offset,
        post_ac80_offset,
        ac80_reset_offset,
    }
}

/// REMOTE 分支 codecave:replicate `mov [ecx+0x380], eax`,然後若色碼是
/// DAMAGE_TEXT_COLOR(BGR565 紅 0xF800)抵消 sprite_top 並加 FEET_PAD,讓氣泡
/// Y 錨點落在 entity_y + FEET_PAD(腳底下方)。
///
/// 色碼讀「函式參數 [ebp+0x10]」而不是「bubble+0x39C」:bubble 由 0x42B898 池化
/// 配置,+0x39C 在 0x42BADE 才寫入 — 但我們的 hook 在那之前跑,直接讀 +0x39C
/// 拿到上一個釋放氣泡的殘留色碼,殘留剛好命中就通過、不是就 fail,造成
/// 「打 10 下 2-3 下飛回頭上」的偶發 bug。函式參數一進來就是真值,可靠。
fn emit_position_remote(bytes: &mut Vec<u8>, segment_start: u32) {
    let start = bytes.len();
    bytes.extend_from_slice(&[0x89, 0x81, 0x80, 0x03, 0x00, 0x00]);
    bytes.extend_from_slice(&[0x66, 0x81, 0x7D, 0x10, DAMAGE_COLOR_LO, DAMAGE_COLOR_HI]);
    bytes.extend_from_slice(&[0x75, 0x2A]);
    bytes.extend_from_slice(&[0x8B, 0x91, 0x98, 0x03, 0x00, 0x00]);
    bytes.extend_from_slice(&[0x85, 0xD2, 0x74, 0x20]);
    bytes.extend_from_slice(&[0x0F, 0xBE, 0x42, 0x1D]);
    bytes.extend_from_slice(&[0x6B, 0xC0, 0x18]);
    bytes.extend_from_slice(&[0x8B, 0x92, 0x8C, 0x00, 0x00, 0x00]);
    bytes.extend_from_slice(&[0x85, 0xD2, 0x74, 0x0F]);
    bytes.extend_from_slice(&[0x8B, 0x54, 0x02, 0x14]);
    bytes.extend_from_slice(&[0xF7, 0xDA]);
    bytes.extend_from_slice(&[0x83, 0xC2, FEET_PAD]);
    bytes.extend_from_slice(&[0x01, 0x91, 0x80, 0x03, 0x00, 0x00]);
    let here = segment_start + (bytes.len() - start) as u32;
    emit_jmp32(bytes, here, RETURN_AFTER_POSITION);
}

/// LOCAL 分支 codecave:edx/ecx 角色互換的等價邏輯。色碼同樣讀 [ebp+0x10]。
fn emit_position_local(bytes: &mut Vec<u8>, segment_start: u32) {
    let start = bytes.len();
    bytes.extend_from_slice(&[0x89, 0x8A, 0x80, 0x03, 0x00, 0x00]);
    bytes.extend_from_slice(&[0x66, 0x81, 0x7D, 0x10, DAMAGE_COLOR_LO, DAMAGE_COLOR_HI]);
    bytes.extend_from_slice(&[0x75, 0x2A]);
    bytes.extend_from_slice(&[0x8B, 0x8A, 0x98, 0x03, 0x00, 0x00]);
    bytes.extend_from_slice(&[0x85, 0xC9, 0x74, 0x20]);
    bytes.extend_from_slice(&[0x0F, 0xBE, 0x41, 0x1D]);
    bytes.extend_from_slice(&[0x6B, 0xC0, 0x18]);
    bytes.extend_from_slice(&[0x8B, 0x89, 0x8C, 0x00, 0x00, 0x00]);
    bytes.extend_from_slice(&[0x85, 0xC9, 0x74, 0x0F]);
    bytes.extend_from_slice(&[0x8B, 0x4C, 0x01, 0x14]);
    bytes.extend_from_slice(&[0xF7, 0xD9]);
    bytes.extend_from_slice(&[0x83, 0xC1, FEET_PAD]);
    bytes.extend_from_slice(&[0x01, 0x8A, 0x80, 0x03, 0x00, 0x00]);
    let here = segment_start + (bytes.len() - start) as u32;
    emit_jmp32(bytes, here, RETURN_AFTER_POSITION);
}

/// post-AC80 codecave:覆寫 `movzx eax,al; test eax,eax`,加上對紅色氣泡寫
/// `+0x36A=4`(翻轉箭頭尾巴朝上),再 replicate 原本的 movzx + test 並接回 je。
fn emit_post_ac80(bytes: &mut Vec<u8>, segment_start: u32) {
    let start = bytes.len();
    bytes.extend_from_slice(&[0x8B, 0x55, 0xE0]);
    bytes.extend_from_slice(&[
        0x66, 0x81, 0xBA, 0x9C, 0x03, 0x00, 0x00, DAMAGE_COLOR_LO, DAMAGE_COLOR_HI,
    ]);
    bytes.extend_from_slice(&[0x75, 0x07]);
    bytes.extend_from_slice(&[0xC6, 0x82, 0x6A, 0x03, 0x00, 0x00, 0x04]);
    bytes.extend_from_slice(&[0x0F, 0xB6, 0xC0]);
    bytes.extend_from_slice(&[0x85, 0xC0]);
    let here = segment_start + (bytes.len() - start) as u32;
    emit_jmp32(bytes, here, RETURN_AFTER_POST_AC80);
}

/// 0x42AC80 內部 reset codecave:取代 `mov byte [ecx+0x36A], 0`,對紅色傷害氣泡
/// 跳過重設(保留 +0x36A=4),其他氣泡走原本邏輯。讓箭頭翻轉每幀都不會被覆蓋。
fn emit_ac80_reset(bytes: &mut Vec<u8>, segment_start: u32) {
    let start = bytes.len();
    bytes.extend_from_slice(&[
        0x66, 0x81, 0xB9, 0x9C, 0x03, 0x00, 0x00, DAMAGE_COLOR_LO, DAMAGE_COLOR_HI,
    ]);
    bytes.extend_from_slice(&[0x74, 0x07]);
    bytes.extend_from_slice(&[0xC6, 0x81, 0x6A, 0x03, 0x00, 0x00, 0x00]);
    let here = segment_start + (bytes.len() - start) as u32;
    emit_jmp32(bytes, here, RETURN_AFTER_AC80_RESET);
}

fn emit_jmp32(bytes: &mut Vec<u8>, here: u32, target: u32) {
    let next_ip = here + 5;
    let rel = (target as i64 - next_ip as i64) as i32;
    bytes.push(0xE9);
    bytes.extend_from_slice(&rel.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_jmp_patch_short_jump_within_module() {
        let patch: [u8; 6] = build_jmp_patch(0x0042_B9FB, 0x0042_BA01);
        // jmp rel32 forward: rel = 0x42BA01 - (0x42B9FB + 5) = 1
        assert_eq!(patch, [0xE9, 0x01, 0x00, 0x00, 0x00, 0x90]);
    }

    #[test]
    fn shellcode_segments_are_within_codecave_budget() {
        let layout = build_shellcode(0x0050_0000);
        assert!(layout.bytes.len() <= CODECAVE_SIZE);
        // four segments (REMOTE + LOCAL + post_ac80 + ac80_reset) ≈ 61+61+31+23 = 176
        assert!(layout.bytes.len() < 256);
    }

    #[test]
    fn shellcode_segments_have_expected_offsets() {
        let layout = build_shellcode(0x0050_0000);
        assert_eq!(layout.remote_offset, 0);
        assert_eq!(layout.local_offset, 61);
        assert_eq!(layout.post_ac80_offset, 122);
        assert_eq!(layout.ac80_reset_offset, 153);
    }

    #[test]
    fn remote_segment_returns_to_position_continuation() {
        let cave = 0x0050_0000_u32;
        let layout = build_shellcode(cave);
        // REMOTE segment trailing jmp at offset 0x38 (segment shrunk 3 bytes after using [ebp+0x10] cmp)
        let jmp_offset = layout.remote_offset + 0x38;
        assert_eq!(layout.bytes[jmp_offset], 0xE9);
        let rel_bytes: [u8; 4] = layout.bytes[jmp_offset + 1..jmp_offset + 5]
            .try_into()
            .unwrap();
        let rel = i32::from_le_bytes(rel_bytes);
        let here = cave + jmp_offset as u32;
        let computed = (here as i64 + 5 + rel as i64) as u32;
        assert_eq!(computed, RETURN_AFTER_POSITION);
    }

    #[test]
    fn ac80_reset_segment_returns_to_post_reset_continuation() {
        let cave = 0x0050_0000_u32;
        let layout = build_shellcode(cave);
        let seg_start = layout.ac80_reset_offset;
        // jmp at end of segment (last 5 bytes)
        let jmp_offset = layout.bytes.len() - 5;
        assert_eq!(layout.bytes[jmp_offset], 0xE9);
        let rel_bytes: [u8; 4] = layout.bytes[jmp_offset + 1..jmp_offset + 5]
            .try_into()
            .unwrap();
        let rel = i32::from_le_bytes(rel_bytes);
        let here = cave + jmp_offset as u32;
        let computed = (here as i64 + 5 + rel as i64) as u32;
        assert_eq!(computed, RETURN_AFTER_AC80_RESET);
        let _ = seg_start; // silence unused warning when not asserted
    }

    #[test]
    fn post_ac80_segment_skip_jne_disp_targets_movzx() {
        // jne short to skip the mov byte +0x36A, 4 — should land on movzx eax, al (3 bytes)
        let cave = 0x0050_0000_u32;
        let layout = build_shellcode(cave);
        let seg = layout.post_ac80_offset;
        // segment: [mov edx (3)][cmp word (9)][jne 75 ?? (2)][mov byte (7)][movzx (3)]...
        let jne_disp_offset = seg + 3 + 9 + 1; // +1 to skip the 0x75 opcode
        assert_eq!(layout.bytes[seg + 3 + 9], 0x75);
        assert_eq!(layout.bytes[jne_disp_offset], 0x07); // skip past 7-byte mov byte
    }
}
