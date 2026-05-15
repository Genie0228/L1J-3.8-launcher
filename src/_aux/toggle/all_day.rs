//! All-day brightness patch.
//!
//! The visible world brightness is controlled by the light-level calculator at
//! `0x00786D70` in TW13081901.bin. It normally returns a value in the range
//! 5..15 from the current game-time object. For all-day, force the calculator to
//! return the maximum level. All-day also disables visible weather because rain,
//! snow, and fog reduce visibility even when the light level is forced high.

use anyhow::{bail, Context, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Memory::{VirtualFreeEx, MEM_RELEASE};
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeThread, WaitForSingleObject,
};

use crate::log_line;
use crate::memory::{alloc_exec, read_bytes, read_u32, scan_pattern, write_code};

use super::Toggle;

pub struct AllDay;

const SCAN_START: u32 = 0x00400000;
const SCAN_END: u32 = 0x00F10000;

const BRIGHTNESS_CALC_ADDR: u32 = 0x00786D70;
const WEATHER_RENDER_ADDR: u32 = 0x004ED890;
const DAYLIGHT_CHECK_ADDR: u32 = 0x00787040;
const PALETTE_DARKEN_LOAD_ARG_ADDR: u32 = 0x0057E6F7;
const PALETTE_DARKEN_CACHE_ARG_ADDR: u32 = 0x0057E707;
const PALETTE_DARK_LEVEL_ADDR: u32 = 0x00BDC9D0;
const PALETTE_TABLE_PTR_ADDR: u32 = 0x00BDC9D4;
const PALETTE_MAX_LIGHT_SRC_OFFSET: u32 = 0x150;
const PALETTE_ACTIVE_DST_OFFSET: u32 = 0x50;
const PALETTE_COPY_LEN: usize = 0x100;
const CAVE_DARK_FLAG_ADDR: u32 = 0x009ABCEF;
const CAVE_DARK_HIGH_MAP_SET_IMM_ADDR: u32 = 0x004EA514;
const CAVE_DARK_TILESET_SET_IMM_ADDR: u32 = 0x004EA551;
const CAVE_LIGHT_FORCE_IMM_ADDR: u32 = 0x004EA6D4;
const LIGHT_RECOMPUTE_SKIP_BRANCH_ADDR: u32 = 0x004EAD19;
const ENVIRONMENT_OVERLAY_BRANCH_ADDR: u32 = 0x004F0E92;
const FINAL_LIGHT_ARG_ADDR: u32 = 0x004F037C;

const BRIGHTNESS_CALC_SIG: [Option<u8>; 16] = [
    Some(0x55),
    Some(0x8B),
    Some(0xEC),
    Some(0x83),
    Some(0xEC),
    Some(0x24),
    Some(0x8B),
    Some(0x45),
    Some(0x08),
    Some(0x50),
    Some(0xE8),
    None,
    None,
    None,
    None,
    Some(0x83),
];

const BRIGHTNESS_CALC_ORIG: [u8; 16] = [
    0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x24, 0x8B, 0x45, 0x08, 0x50, 0xE8, 0x81, 0xA1, 0xE0, 0xFF, 0x83,
];

const BRIGHTNESS_CALC_ON: [u8; 16] = [
    0xB8, 0x0F, 0x00, 0x00, 0x00, 0xC3, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
];

const WEATHER_RENDER_SIG: [Option<u8>; 8] = [
    Some(0x55),
    Some(0x8B),
    Some(0xEC),
    Some(0x83),
    Some(0xEC),
    Some(0x24),
    Some(0x83),
    Some(0x3D),
];

const WEATHER_RENDER_PATCHED_SIG: [Option<u8>; 8] = [
    Some(0xC3),
    Some(0x8B),
    Some(0xEC),
    Some(0x83),
    Some(0xEC),
    Some(0x24),
    Some(0x83),
    Some(0x3D),
];

const WEATHER_RENDER_ORIG: [u8; 1] = [0x55];
const WEATHER_RENDER_ON: [u8; 1] = [0xC3];

const DAYLIGHT_CHECK_ORIG: [u8; 3] = [0x55, 0x8B, 0xEC];
const DAYLIGHT_CHECK_ON: [u8; 3] = [0xB0, 0x01, 0xC3];

const PALETTE_DARKEN_LOAD_ARG_ORIG: [u8; 3] = [0x8B, 0x45, 0x08];
const PALETTE_DARKEN_LOAD_ARG_ON: [u8; 3] = [0x31, 0xC0, 0x90];
const PALETTE_DARKEN_CACHE_ARG_ORIG: [u8; 3] = [0x8B, 0x4D, 0x08];
const PALETTE_DARKEN_CACHE_ARG_ON: [u8; 3] = [0x31, 0xC9, 0x90];
const CAVE_DARK_SET_ORIG: [u8; 1] = [0x01];
const CAVE_DARK_SET_ON: [u8; 1] = [0x00];
const CAVE_LIGHT_FORCE_ORIG: [u8; 1] = [0x01];
const CAVE_LIGHT_FORCE_ON: [u8; 1] = [0x0F];
const LIGHT_RECOMPUTE_SKIP_BRANCH_ORIG: [u8; 6] = [0x0F, 0x8D, 0x2B, 0x04, 0x00, 0x00];
const LIGHT_RECOMPUTE_SKIP_BRANCH_ON: [u8; 6] = [0x90, 0x90, 0x90, 0x90, 0x90, 0x90];
const ENVIRONMENT_OVERLAY_BRANCH_ORIG: [u8; 7] = [0x83, 0x3D, 0xF0, 0xC9, 0xBD, 0x00, 0x00];
const ENVIRONMENT_OVERLAY_BRANCH_ON: [u8; 7] = [0xE9, 0xA6, 0x00, 0x00, 0x00, 0x90, 0x90];
const FINAL_LIGHT_ARG_ORIG: [u8; 3] = [0x8B, 0x55, 0xAC];
const FINAL_LIGHT_ARG_ON: [u8; 3] = [0x6A, 0x0F, 0x5A];

const WEATHER_STATE_ADDRS: [u32; 3] = [
    0x00ABF324, // rain intensity
    0x00ABF328, // snow intensity
    0x00ABF8A4, // fog intensity
];

impl Toggle for AllDay {
    fn enable(&self, h: HANDLE) -> Result<()> {
        let brightness_addr = find_brightness_calc(h)?;
        let weather_addr = find_weather_render(h)?;
        let daylight_addr = find_daylight_check(h)?;

        apply_patch(
            h,
            brightness_addr,
            &BRIGHTNESS_CALC_ORIG,
            &BRIGHTNESS_CALC_ON,
        )?;

        if let Err(err) = apply_patch(h, weather_addr, &WEATHER_RENDER_ORIG, &WEATHER_RENDER_ON) {
            let _ = restore_patch(
                h,
                brightness_addr,
                &BRIGHTNESS_CALC_ORIG,
                &BRIGHTNESS_CALC_ON,
            );
            return Err(err);
        }

        if let Err(err) = apply_patch(h, daylight_addr, &DAYLIGHT_CHECK_ORIG, &DAYLIGHT_CHECK_ON) {
            let _ = restore_patch(h, weather_addr, &WEATHER_RENDER_ORIG, &WEATHER_RENDER_ON);
            let _ = restore_patch(
                h,
                brightness_addr,
                &BRIGHTNESS_CALC_ORIG,
                &BRIGHTNESS_CALC_ON,
            );
            return Err(err);
        }

        if let Err(err) = apply_palette_darken_patch(h) {
            let _ = restore_patch(h, daylight_addr, &DAYLIGHT_CHECK_ORIG, &DAYLIGHT_CHECK_ON);
            let _ = restore_patch(h, weather_addr, &WEATHER_RENDER_ORIG, &WEATHER_RENDER_ON);
            let _ = restore_patch(
                h,
                brightness_addr,
                &BRIGHTNESS_CALC_ORIG,
                &BRIGHTNESS_CALC_ON,
            );
            return Err(err);
        }

        if let Err(err) = apply_cave_dark_patch(h) {
            let _ = restore_palette_darken_patch(h);
            let _ = restore_patch(h, daylight_addr, &DAYLIGHT_CHECK_ORIG, &DAYLIGHT_CHECK_ON);
            let _ = restore_patch(h, weather_addr, &WEATHER_RENDER_ORIG, &WEATHER_RENDER_ON);
            let _ = restore_patch(
                h,
                brightness_addr,
                &BRIGHTNESS_CALC_ORIG,
                &BRIGHTNESS_CALC_ON,
            );
            return Err(err);
        }

        if let Err(err) = apply_environment_overlay_patch(h) {
            let _ = restore_cave_dark_patch(h);
            let _ = restore_palette_darken_patch(h);
            let _ = restore_patch(h, daylight_addr, &DAYLIGHT_CHECK_ORIG, &DAYLIGHT_CHECK_ON);
            let _ = restore_patch(h, weather_addr, &WEATHER_RENDER_ORIG, &WEATHER_RENDER_ON);
            let _ = restore_patch(
                h,
                brightness_addr,
                &BRIGHTNESS_CALC_ORIG,
                &BRIGHTNESS_CALC_ON,
            );
            return Err(err);
        }

        if let Err(err) = apply_final_light_patch(h) {
            let _ = restore_environment_overlay_patch(h);
            let _ = restore_cave_dark_patch(h);
            let _ = restore_palette_darken_patch(h);
            let _ = restore_patch(h, daylight_addr, &DAYLIGHT_CHECK_ORIG, &DAYLIGHT_CHECK_ON);
            let _ = restore_patch(h, weather_addr, &WEATHER_RENDER_ORIG, &WEATHER_RENDER_ON);
            let _ = restore_patch(
                h,
                brightness_addr,
                &BRIGHTNESS_CALC_ORIG,
                &BRIGHTNESS_CALC_ON,
            );
            return Err(err);
        }

        if let Err(err) = apply_light_recompute_patch(h) {
            let _ = restore_final_light_patch(h);
            let _ = restore_environment_overlay_patch(h);
            let _ = restore_cave_dark_patch(h);
            let _ = restore_palette_darken_patch(h);
            let _ = restore_patch(h, daylight_addr, &DAYLIGHT_CHECK_ORIG, &DAYLIGHT_CHECK_ON);
            let _ = restore_patch(h, weather_addr, &WEATHER_RENDER_ORIG, &WEATHER_RENDER_ON);
            let _ = restore_patch(
                h,
                brightness_addr,
                &BRIGHTNESS_CALC_ORIG,
                &BRIGHTNESS_CALC_ON,
            );
            return Err(err);
        }

        if let Err(err) = clear_weather_state(h) {
            let _ = restore_light_recompute_patch(h);
            let _ = restore_final_light_patch(h);
            let _ = restore_environment_overlay_patch(h);
            let _ = restore_cave_dark_patch(h);
            let _ = restore_palette_darken_patch(h);
            let _ = restore_patch(h, daylight_addr, &DAYLIGHT_CHECK_ORIG, &DAYLIGHT_CHECK_ON);
            let _ = restore_patch(h, weather_addr, &WEATHER_RENDER_ORIG, &WEATHER_RENDER_ON);
            let _ = restore_patch(
                h,
                brightness_addr,
                &BRIGHTNESS_CALC_ORIG,
                &BRIGHTNESS_CALC_ON,
            );
            return Err(err);
        }

        clear_cave_dark_state(h)?;
        let _ = reset_palette_darkening(h);
        Ok(())
    }

    fn disable(&self, h: HANDLE) -> Result<()> {
        let brightness_addr = find_brightness_calc(h)?;
        let weather_addr = find_weather_render(h)?;
        let daylight_addr = find_daylight_check(h)?;

        restore_light_recompute_patch(h)?;
        restore_final_light_patch(h)?;
        restore_environment_overlay_patch(h)?;
        restore_cave_dark_patch(h)?;
        restore_palette_darken_patch(h)?;
        restore_patch(h, daylight_addr, &DAYLIGHT_CHECK_ORIG, &DAYLIGHT_CHECK_ON)?;
        restore_patch(h, weather_addr, &WEATHER_RENDER_ORIG, &WEATHER_RENDER_ON)?;
        restore_patch(
            h,
            brightness_addr,
            &BRIGHTNESS_CALC_ORIG,
            &BRIGHTNESS_CALC_ON,
        )?;

        // 8 個 code patch 已還原,但 enable 時主動寫的 state(cave_dark_flag、palette[0x50..])
        // 仍是「最亮」殘留 — game 不會主動重算,所以「在地監 disable」會看到地監仍亮。
        // 跑一段 shellcode 模擬 game 內部 0x004EA6C8 的「重算 cave_dark_flag + 重新呼叫
        // palette refresh」邏輯,讓 game 立刻依當前 map / tileset / brightness 套對的 palette。
        if let Err(e) = force_palette_refresh(h) {
            log_line!("[all_day] palette refresh failed (non-fatal): {e:#}");
        }
        Ok(())
    }

    fn is_safe(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "all_day"
    }
}

fn find_brightness_calc(h: HANDLE) -> Result<u32> {
    if bytes_match(h, BRIGHTNESS_CALC_ADDR, &BRIGHTNESS_CALC_ORIG)
        || bytes_match(h, BRIGHTNESS_CALC_ADDR, &BRIGHTNESS_CALC_ON)
    {
        return Ok(BRIGHTNESS_CALC_ADDR);
    }

    if let Some(addr) = scan_pattern(h, SCAN_START, SCAN_END, &BRIGHTNESS_CALC_SIG)? {
        return Ok(addr);
    }

    let patched_sig: Vec<Option<u8>> = BRIGHTNESS_CALC_ON.iter().copied().map(Some).collect();
    scan_pattern(h, SCAN_START, SCAN_END, &patched_sig)?
        .ok_or_else(|| anyhow::anyhow!("all_day brightness calculator AOB not found"))
}

fn find_weather_render(h: HANDLE) -> Result<u32> {
    if bytes_match(h, WEATHER_RENDER_ADDR, &WEATHER_RENDER_ORIG)
        || bytes_match(h, WEATHER_RENDER_ADDR, &WEATHER_RENDER_ON)
    {
        return Ok(WEATHER_RENDER_ADDR);
    }

    if let Some(addr) = scan_pattern(h, SCAN_START, SCAN_END, &WEATHER_RENDER_SIG)? {
        return Ok(addr);
    }

    scan_pattern(h, SCAN_START, SCAN_END, &WEATHER_RENDER_PATCHED_SIG)?
        .ok_or_else(|| anyhow::anyhow!("all_day weather renderer AOB not found"))
}

fn find_daylight_check(h: HANDLE) -> Result<u32> {
    if bytes_match(h, DAYLIGHT_CHECK_ADDR, &DAYLIGHT_CHECK_ORIG)
        || bytes_match(h, DAYLIGHT_CHECK_ADDR, &DAYLIGHT_CHECK_ON)
    {
        return Ok(DAYLIGHT_CHECK_ADDR);
    }

    bail!("all_day daylight check target mismatch @ 0x{DAYLIGHT_CHECK_ADDR:08X}")
}

fn clear_weather_state(h: HANDLE) -> Result<()> {
    for addr in WEATHER_STATE_ADDRS {
        write_code(h, addr, &[0, 0, 0, 0])?;
    }
    Ok(())
}

fn clear_cave_dark_state(h: HANDLE) -> Result<()> {
    write_code(h, CAVE_DARK_FLAG_ADDR, &[0])
}

fn apply_cave_dark_patch(h: HANDLE) -> Result<()> {
    apply_patch(
        h,
        CAVE_DARK_HIGH_MAP_SET_IMM_ADDR,
        &CAVE_DARK_SET_ORIG,
        &CAVE_DARK_SET_ON,
    )?;

    if let Err(err) = apply_patch(
        h,
        CAVE_DARK_TILESET_SET_IMM_ADDR,
        &CAVE_DARK_SET_ORIG,
        &CAVE_DARK_SET_ON,
    ) {
        let _ = restore_patch(
            h,
            CAVE_DARK_HIGH_MAP_SET_IMM_ADDR,
            &CAVE_DARK_SET_ORIG,
            &CAVE_DARK_SET_ON,
        );
        return Err(err);
    }

    if let Err(err) = apply_patch(
        h,
        CAVE_LIGHT_FORCE_IMM_ADDR,
        &CAVE_LIGHT_FORCE_ORIG,
        &CAVE_LIGHT_FORCE_ON,
    ) {
        let _ = restore_patch(
            h,
            CAVE_DARK_TILESET_SET_IMM_ADDR,
            &CAVE_DARK_SET_ORIG,
            &CAVE_DARK_SET_ON,
        );
        let _ = restore_patch(
            h,
            CAVE_DARK_HIGH_MAP_SET_IMM_ADDR,
            &CAVE_DARK_SET_ORIG,
            &CAVE_DARK_SET_ON,
        );
        return Err(err);
    }

    Ok(())
}

fn restore_cave_dark_patch(h: HANDLE) -> Result<()> {
    restore_patch(
        h,
        CAVE_LIGHT_FORCE_IMM_ADDR,
        &CAVE_LIGHT_FORCE_ORIG,
        &CAVE_LIGHT_FORCE_ON,
    )?;
    restore_patch(
        h,
        CAVE_DARK_TILESET_SET_IMM_ADDR,
        &CAVE_DARK_SET_ORIG,
        &CAVE_DARK_SET_ON,
    )?;
    restore_patch(
        h,
        CAVE_DARK_HIGH_MAP_SET_IMM_ADDR,
        &CAVE_DARK_SET_ORIG,
        &CAVE_DARK_SET_ON,
    )
}

fn apply_light_recompute_patch(h: HANDLE) -> Result<()> {
    apply_patch(
        h,
        LIGHT_RECOMPUTE_SKIP_BRANCH_ADDR,
        &LIGHT_RECOMPUTE_SKIP_BRANCH_ORIG,
        &LIGHT_RECOMPUTE_SKIP_BRANCH_ON,
    )
}

fn restore_light_recompute_patch(h: HANDLE) -> Result<()> {
    restore_patch(
        h,
        LIGHT_RECOMPUTE_SKIP_BRANCH_ADDR,
        &LIGHT_RECOMPUTE_SKIP_BRANCH_ORIG,
        &LIGHT_RECOMPUTE_SKIP_BRANCH_ON,
    )
}

fn apply_environment_overlay_patch(h: HANDLE) -> Result<()> {
    apply_patch(
        h,
        ENVIRONMENT_OVERLAY_BRANCH_ADDR,
        &ENVIRONMENT_OVERLAY_BRANCH_ORIG,
        &ENVIRONMENT_OVERLAY_BRANCH_ON,
    )
}

fn restore_environment_overlay_patch(h: HANDLE) -> Result<()> {
    restore_patch(
        h,
        ENVIRONMENT_OVERLAY_BRANCH_ADDR,
        &ENVIRONMENT_OVERLAY_BRANCH_ORIG,
        &ENVIRONMENT_OVERLAY_BRANCH_ON,
    )
}

fn apply_final_light_patch(h: HANDLE) -> Result<()> {
    apply_patch(
        h,
        FINAL_LIGHT_ARG_ADDR,
        &FINAL_LIGHT_ARG_ORIG,
        &FINAL_LIGHT_ARG_ON,
    )
}

fn restore_final_light_patch(h: HANDLE) -> Result<()> {
    restore_patch(
        h,
        FINAL_LIGHT_ARG_ADDR,
        &FINAL_LIGHT_ARG_ORIG,
        &FINAL_LIGHT_ARG_ON,
    )
}

fn apply_palette_darken_patch(h: HANDLE) -> Result<()> {
    apply_patch(
        h,
        PALETTE_DARKEN_LOAD_ARG_ADDR,
        &PALETTE_DARKEN_LOAD_ARG_ORIG,
        &PALETTE_DARKEN_LOAD_ARG_ON,
    )?;

    if let Err(err) = apply_patch(
        h,
        PALETTE_DARKEN_CACHE_ARG_ADDR,
        &PALETTE_DARKEN_CACHE_ARG_ORIG,
        &PALETTE_DARKEN_CACHE_ARG_ON,
    ) {
        let _ = restore_patch(
            h,
            PALETTE_DARKEN_LOAD_ARG_ADDR,
            &PALETTE_DARKEN_LOAD_ARG_ORIG,
            &PALETTE_DARKEN_LOAD_ARG_ON,
        );
        return Err(err);
    }

    Ok(())
}

fn restore_palette_darken_patch(h: HANDLE) -> Result<()> {
    restore_patch(
        h,
        PALETTE_DARKEN_CACHE_ARG_ADDR,
        &PALETTE_DARKEN_CACHE_ARG_ORIG,
        &PALETTE_DARKEN_CACHE_ARG_ON,
    )?;
    restore_patch(
        h,
        PALETTE_DARKEN_LOAD_ARG_ADDR,
        &PALETTE_DARKEN_LOAD_ARG_ORIG,
        &PALETTE_DARKEN_LOAD_ARG_ON,
    )
}

fn reset_palette_darkening(h: HANDLE) -> Result<()> {
    let table_ptr = read_u32(h, PALETTE_TABLE_PTR_ADDR)?;
    if table_ptr == 0 {
        return Ok(());
    }

    let max_light_palette = read_bytes(
        h,
        table_ptr + PALETTE_MAX_LIGHT_SRC_OFFSET,
        PALETTE_COPY_LEN,
    )?;
    write_code(h, table_ptr + PALETTE_ACTIVE_DST_OFFSET, &max_light_palette)?;
    write_code(h, PALETTE_DARK_LEVEL_ADDR, &0u32.to_le_bytes())
}

fn bytes_match(h: HANDLE, addr: u32, expected: &[u8]) -> bool {
    read_bytes(h, addr, expected.len()).is_ok_and(|current| current == expected)
}

fn apply_patch(h: HANDLE, addr: u32, original: &[u8], patched: &[u8]) -> Result<()> {
    let current = read_bytes(h, addr, original.len())?;
    if current == patched {
        return Ok(());
    }
    if current != original {
        bail!(
            "all_day patch target mismatch @ 0x{addr:08X}: {:02X?}",
            current
        );
    }
    write_code(h, addr, patched)
}

fn restore_patch(h: HANDLE, addr: u32, original: &[u8], patched: &[u8]) -> Result<()> {
    let current = read_bytes(h, addr, original.len())?;
    if current == original {
        return Ok(());
    }
    if current != patched {
        bail!(
            "all_day restore target mismatch @ 0x{addr:08X}: {:02X?}",
            current
        );
    }
    write_code(h, addr, original)
}

// ─── Palette refresh ────────────────────────────────────────────
//
// `enable` 結尾寫了 3 個 in-game state(weather=0、cave_dark_flag=0、palette[0x50..]
// =max_light)讓畫面立即看起來最亮。disable 還原 8 個 code patch 後,這些 state 不會
// 自動還原 — 所以「在地監 disable」會看到地監仍是亮的。
//
// 解法:跑一段 shellcode 模擬 game 內部 0x004EA6C8 開始的 routine —
// 1. 重算 cave_dark_flag(看 [0x965b60]=map_id 跟 [0x9655F0..] tileset 表)
// 2. 依新 cave_dark_flag 呼叫 palette_obj.method:
//    - cave: `palette_obj.method(1)` (force darken=1)
//    - 一般: `palette_obj.method(brightness_calc(game_time, 0, 0, 0))`
//
// 對照 RE 結果(實機 disasm 0x004EA470/0x004EA6B9):
// ```
// 0x004EA50E: mov byte [0x9abcef], 1     ; cave_dark = 1 (map>=0x4000)
// 0x004EA517: mov byte [0x9abcef], 0     ; cave_dark = 0 (else)
// 0x004EA54B: mov byte [0x9abcef], 1     ; cave_dark = 1 (tileset 在表)
// 0x004EA6C8: movsx edx, [0x9abcef]      ; 讀 cave_dark
// 0x004EA6D5: mov ecx, 0xbdc7a4          ; thiscall palette_obj
// 0x004EA6DA: call 0x579e10              ; palette_obj.method(1)  [cave 路徑]
// 0x004EA6E7: mov eax, [0xc31e7c]        ; game_time
// 0x004EA6ED: call 0x786d70              ; brightness_calc(game_time,0,0,0)
// 0x004EA6FB: call 0x579e10              ; palette_obj.method(brightness)
// ```

const PALETTE_OBJ_THIS: u32 = 0x00BDC7A4;
const PALETTE_OBJ_REFRESH: u32 = 0x00579E10;
const BRIGHTNESS_CALC_ENTRY: u32 = 0x00786D70;
const GAME_TIME_GLOBAL: u32 = 0x00C31E7C;
const MAP_ID_GLOBAL: u32 = 0x00965B60;
const TILESET_ID_GLOBAL: u32 = 0x00965B64;
const TILESET_TABLE_BASE: u32 = 0x009655F0;
const TILESET_TABLE_LEN: u32 = 0x15A;
const CAVE_DARK_FLAG: u32 = CAVE_DARK_FLAG_ADDR;

const SHELLCODE_ALLOC_SIZE: usize = 256;
const REFRESH_THREAD_WAIT_MS: u32 = 5_000;

/// alloc + 寫 shellcode + CreateRemoteThread + Wait + Free,讓遊戲在自己 process 裡
/// 跑「重算 cave_dark_flag + 重 refresh palette」的等價邏輯。
fn force_palette_refresh(h: HANDLE) -> Result<()> {
    let sc = build_palette_refresh_shellcode();
    let remote_addr = alloc_exec(h, SHELLCODE_ALLOC_SIZE)
        .with_context(|| "VirtualAllocEx for palette refresh shellcode")?;

    let result = (|| -> Result<()> {
        write_code(h, remote_addr, &sc)
            .with_context(|| format!("WriteProcessMemory @ 0x{remote_addr:08X}"))?;

        let mut tid = 0u32;
        let thread_handle = unsafe {
            CreateRemoteThread(
                h,
                None,
                0,
                Some(std::mem::transmute(remote_addr as usize)),
                None,
                0,
                Some(&mut tid),
            )
        }
        .with_context(|| "CreateRemoteThread for palette refresh")?;

        let wait_result =
            unsafe { WaitForSingleObject(thread_handle, REFRESH_THREAD_WAIT_MS) };
        let mut exit_code: u32 = 0;
        let _ = unsafe { GetExitCodeThread(thread_handle, &mut exit_code) };
        unsafe { CloseHandle(thread_handle).ok() };

        if wait_result == WAIT_TIMEOUT {
            bail!(
                "palette refresh shellcode timeout {} ms (tid={tid})",
                REFRESH_THREAD_WAIT_MS
            );
        }
        if wait_result != WAIT_OBJECT_0 {
            bail!(
                "palette refresh shellcode 等待非預期 (wait={wait_result:?}, tid={tid}, exit=0x{exit_code:08X})"
            );
        }
        log_line!("[all_day] palette refresh thread tid={tid} exit=0x{exit_code:08X}");
        Ok(())
    })();

    unsafe {
        let _ = VirtualFreeEx(h, remote_addr as *mut _, 0, MEM_RELEASE);
    }

    result
}

/// 組 111-byte palette refresh shellcode。對應 RE 出來的 layout(byte offsets 標在註解):
fn build_palette_refresh_shellcode() -> Vec<u8> {
    let mut sc = Vec::with_capacity(112);
    // [0]   pushad
    sc.push(0x60);
    // [1]   mov byte [CAVE_DARK_FLAG], 0
    sc.extend_from_slice(&[0xC6, 0x05]);
    sc.extend_from_slice(&CAVE_DARK_FLAG.to_le_bytes());
    sc.push(0x00);
    // [8]   mov eax, [MAP_ID_GLOBAL]
    sc.push(0xA1);
    sc.extend_from_slice(&MAP_ID_GLOBAL.to_le_bytes());
    // [13]  cmp eax, 0x4000
    sc.extend_from_slice(&[0x3D, 0x00, 0x40, 0x00, 0x00]);
    // [18]  jge +0x14 → .cave (offset 40)
    sc.extend_from_slice(&[0x7D, 0x14]);
    // [20]  mov eax, [TILESET_ID_GLOBAL]
    sc.push(0xA1);
    sc.extend_from_slice(&TILESET_ID_GLOBAL.to_le_bytes());
    // [25]  mov edi, TILESET_TABLE_BASE
    sc.push(0xBF);
    sc.extend_from_slice(&TILESET_TABLE_BASE.to_le_bytes());
    // [30]  mov ecx, TILESET_TABLE_LEN
    sc.push(0xB9);
    sc.extend_from_slice(&TILESET_TABLE_LEN.to_le_bytes());
    // [35]  cld
    sc.push(0xFC);
    // [36]  repne scasd
    sc.extend_from_slice(&[0xF2, 0xAF]);
    // [38]  jne +7 → .refresh (offset 47),沒找到就保持 cave_dark=0
    sc.extend_from_slice(&[0x75, 0x07]);
    // [40]  .cave: mov byte [CAVE_DARK_FLAG], 1
    sc.extend_from_slice(&[0xC6, 0x05]);
    sc.extend_from_slice(&CAVE_DARK_FLAG.to_le_bytes());
    sc.push(0x01);
    // [47]  .refresh: movsx edx, byte [CAVE_DARK_FLAG]
    sc.extend_from_slice(&[0x0F, 0xBE, 0x15]);
    sc.extend_from_slice(&CAVE_DARK_FLAG.to_le_bytes());
    // [54]  test edx, edx
    sc.extend_from_slice(&[0x85, 0xD2]);
    // [56]  je +0x10 → .normal (offset 74)
    sc.extend_from_slice(&[0x74, 0x10]);
    // [58]  push 1
    sc.extend_from_slice(&[0x6A, 0x01]);
    // [60]  mov ecx, PALETTE_OBJ_THIS
    sc.push(0xB9);
    sc.extend_from_slice(&PALETTE_OBJ_THIS.to_le_bytes());
    // [65]  mov eax, PALETTE_OBJ_REFRESH
    sc.push(0xB8);
    sc.extend_from_slice(&PALETTE_OBJ_REFRESH.to_le_bytes());
    // [70]  call eax (thiscall, callee cleans 1 arg)
    sc.extend_from_slice(&[0xFF, 0xD0]);
    // [72]  jmp +0x23 → .end (offset 109)
    sc.extend_from_slice(&[0xEB, 0x23]);
    // [74]  .normal: push 0; push 0; push 0
    sc.extend_from_slice(&[0x6A, 0x00, 0x6A, 0x00, 0x6A, 0x00]);
    // [80]  mov eax, [GAME_TIME_GLOBAL]
    sc.push(0xA1);
    sc.extend_from_slice(&GAME_TIME_GLOBAL.to_le_bytes());
    // [85]  push eax
    sc.push(0x50);
    // [86]  mov eax, BRIGHTNESS_CALC_ENTRY
    sc.push(0xB8);
    sc.extend_from_slice(&BRIGHTNESS_CALC_ENTRY.to_le_bytes());
    // [91]  call eax (cdecl 4 args)
    sc.extend_from_slice(&[0xFF, 0xD0]);
    // [93]  add esp, 0x10
    sc.extend_from_slice(&[0x83, 0xC4, 0x10]);
    // [96]  push eax (brightness result)
    sc.push(0x50);
    // [97]  mov ecx, PALETTE_OBJ_THIS
    sc.push(0xB9);
    sc.extend_from_slice(&PALETTE_OBJ_THIS.to_le_bytes());
    // [102] mov eax, PALETTE_OBJ_REFRESH
    sc.push(0xB8);
    sc.extend_from_slice(&PALETTE_OBJ_REFRESH.to_le_bytes());
    // [107] call eax
    sc.extend_from_slice(&[0xFF, 0xD0]);
    // [109] .end: popad
    sc.push(0x61);
    // [110] ret
    sc.push(0xC3);
    debug_assert_eq!(sc.len(), 111, "palette refresh shellcode 應為 111 bytes");
    sc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cave_light_patch_changes_forced_indoor_level_to_max() {
        assert_eq!(CAVE_LIGHT_FORCE_IMM_ADDR, 0x004EA6D4);
        assert_eq!(CAVE_LIGHT_FORCE_ORIG, [0x01]);
        assert_eq!(CAVE_LIGHT_FORCE_ON, [0x0F]);
    }

    #[test]
    fn light_recompute_patch_keeps_max_light_from_skipping_refresh() {
        assert_eq!(LIGHT_RECOMPUTE_SKIP_BRANCH_ADDR, 0x004EAD19);
        assert_eq!(
            LIGHT_RECOMPUTE_SKIP_BRANCH_ORIG,
            [0x0F, 0x8D, 0x2B, 0x04, 0x00, 0x00]
        );
        assert_eq!(
            LIGHT_RECOMPUTE_SKIP_BRANCH_ON,
            [0x90, 0x90, 0x90, 0x90, 0x90, 0x90]
        );
    }

    #[test]
    fn environment_overlay_patch_skips_dark_scene_layer() {
        assert_eq!(ENVIRONMENT_OVERLAY_BRANCH_ADDR, 0x004F0E92);
        assert_eq!(
            ENVIRONMENT_OVERLAY_BRANCH_ORIG,
            [0x83, 0x3D, 0xF0, 0xC9, 0xBD, 0x00, 0x00]
        );
        assert_eq!(
            ENVIRONMENT_OVERLAY_BRANCH_ON,
            [0xE9, 0xA6, 0x00, 0x00, 0x00, 0x90, 0x90]
        );
    }

    #[test]
    fn final_light_patch_forces_max_light_argument() {
        assert_eq!(FINAL_LIGHT_ARG_ADDR, 0x004F037C);
        assert_eq!(FINAL_LIGHT_ARG_ORIG, [0x8B, 0x55, 0xAC]);
        assert_eq!(FINAL_LIGHT_ARG_ON, [0x6A, 0x0F, 0x5A]);
    }

    #[test]
    fn palette_refresh_shellcode_size_111_bytes() {
        let sc = build_palette_refresh_shellcode();
        assert_eq!(sc.len(), 111);
    }

    #[test]
    fn palette_refresh_shellcode_starts_with_pushad_and_ends_with_popad_ret() {
        let sc = build_palette_refresh_shellcode();
        assert_eq!(sc[0], 0x60, "首位 byte 必為 pushad");
        assert_eq!(&sc[109..111], &[0x61, 0xC3], "結尾必為 popad; ret");
    }

    #[test]
    fn palette_refresh_shellcode_jge_jumps_to_cave_branch() {
        // jge +0x14 在 offset 18,目標應該是 offset 40 (.cave)
        let sc = build_palette_refresh_shellcode();
        assert_eq!(&sc[18..20], &[0x7D, 0x14], "jge to .cave");
        // .cave: mov byte [CAVE_DARK_FLAG], 1 — 應在 offset 40
        assert_eq!(&sc[40..42], &[0xC6, 0x05], "expect mov byte at .cave");
        let imm_at_47 = sc[46];
        assert_eq!(imm_at_47, 0x01, ".cave 寫 cave_dark=1");
    }

    #[test]
    fn palette_refresh_shellcode_je_jumps_to_normal_branch() {
        // je +0x10 在 offset 56,目標應該是 offset 74 (.normal)
        let sc = build_palette_refresh_shellcode();
        assert_eq!(&sc[56..58], &[0x74, 0x10], "je to .normal");
        // .normal: push 0; push 0; push 0 — offset 74..80
        assert_eq!(&sc[74..80], &[0x6A, 0x00, 0x6A, 0x00, 0x6A, 0x00]);
    }

    #[test]
    fn palette_refresh_shellcode_jmp_skips_to_end() {
        // jmp +0x23 在 offset 72,目標 offset 109(popad)
        let sc = build_palette_refresh_shellcode();
        assert_eq!(&sc[72..74], &[0xEB, 0x23], "jmp to .end");
        assert_eq!(sc[109], 0x61, ".end starts with popad");
    }

    #[test]
    fn palette_refresh_shellcode_embeds_correct_addresses() {
        let sc = build_palette_refresh_shellcode();
        // mov ecx, PALETTE_OBJ_THIS @ offset 60(cave 路徑)
        assert_eq!(sc[60], 0xB9, "mov ecx, imm32");
        let this_at_61 = u32::from_le_bytes([sc[61], sc[62], sc[63], sc[64]]);
        assert_eq!(this_at_61, PALETTE_OBJ_THIS);
        // mov eax, PALETTE_OBJ_REFRESH @ offset 65
        assert_eq!(sc[65], 0xB8);
        let refresh_at_66 = u32::from_le_bytes([sc[66], sc[67], sc[68], sc[69]]);
        assert_eq!(refresh_at_66, PALETTE_OBJ_REFRESH);
        // mov eax, BRIGHTNESS_CALC_ENTRY @ offset 86
        assert_eq!(sc[86], 0xB8);
        let bcalc = u32::from_le_bytes([sc[87], sc[88], sc[89], sc[90]]);
        assert_eq!(bcalc, BRIGHTNESS_CALC_ENTRY);
    }
}
