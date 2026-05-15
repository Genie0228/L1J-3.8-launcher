//! Hook ImageElement_draw @ 0x0042F450 — per-frame PNG 渲染。
//!
//! # 架構
//!
//! 遊戲的 PNG draw 入口是 `ImageElement_draw(this)` (thiscall, 0 arg):
//! ```c
//! struct ImageElement {  // size >= 0x50
//!     u8  unk_00[4];
//!     int dst_x;          // +0x04
//!     int dst_y;          // +0x08
//!     u8  unk_0c[0x10];
//!     void* dst_surf_obj; // +0x1c — 螢幕 back buffer 物件
//!     u8  unk_20[3];
//!     u16 gfxid;          // +0x20 (alpha=0 mode)
//!     u8  unk_22[0xB];
//!     u8  alpha_flag;     // +0x2d  非零 → 走 alpha 路徑 0x759A10
//!     u8  unk_2e[0xE];
//!     u8  descriptor[0x10]; // +0x3c embedded dest 幾何
//!     u8  unk_4c[4];
//!     u16 gfxid_alpha;    // +0x4c (alpha=1 mode)
//! };
//! ```
//!
//! 我們在 0x42F450 的 prologue(7 bytes:`55 8B EC 51 89 4D FC` = push ebp/mov ebp,esp/
//! push ecx/mov [ebp-4],ecx)patch `E9 disp32 + 2×NOP`,跳進 cave。
//!
//! Cave 行為:
//! 1. `pushad/pushfd` — 保所有暫存器
//! 2. `lock inc INVOCATIONS` — diag 計數
//! 3. 若還沒偷,從原 ECX(this)讀 `+0x1c` (dst_surf_obj) 跟 `+0x3c..+0x4c` (descriptor),
//!    寫入 cave 全域,設 STOLEN_FLAG=1
//! 4. 若 DRAW_COUNT > 0,iterate DRAW_LIST:
//!    - 把 stolen 資料 + entry 的 gfxid/x/y 寫入 TEMPLATE
//!    - `mov ecx, TEMPLATE; call TRAMPOLINE`(TRAMPOLINE = 我們 cave 內的 prologue trampoline,
//!      bypass 自己的 hook → 不會 infinite loop)
//! 5. `popfd/popad` — 還原暫存器
//! 6. **掉進 TRAMPOLINE** — 跑原 prologue 7 bytes + jmp 0x42F457(原函式 body)
//!
//! TRAMPOLINE 同時被 cave 結尾 fall-through(原 call 路徑)跟 step 4 的 `call TRAMPOLINE`
//! (我們的遞迴 draw)使用。 雙用途,不需要兩份。

use anyhow::{bail, Context, Result};
use windows::Win32::Foundation::HANDLE;

use crate::logger::log_line;
use crate::{memory, process};

/// ImageElement_draw 入口。
const HOOK_ADDR: u32 = 0x0042F450;
/// 被覆寫的 prologue bytes。 為什麼是 7 byte:
/// `55 (push ebp)` + `8B EC (mov ebp, esp)` + `51 (push ecx)` + `89 4D FC (mov [ebp-4], ecx)`
/// = 1 + 2 + 1 + 3 = 7。 第一個能塞 `E9 disp32` (5 byte) 的乾淨 instruction boundary。
const ORIGINAL_PROLOGUE: [u8; 7] = [0x55, 0x8B, 0xEC, 0x51, 0x89, 0x4D, 0xFC];
const HOOK_LEN: usize = ORIGINAL_PROLOGUE.len();
/// 原 prologue 之後的地址 — TRAMPOLINE 最後跳這裡。
const RESUME_ADDR: u32 = HOOK_ADDR + HOOK_LEN as u32;

// === Cave layout ===
/// `lock inc` 計數,每次 hook fire +1。
pub const OFF_INVOCATIONS: u32 = 0x000;
/// 0 / 1 — 是否已偷下 dst_surf_obj + descriptor。
pub const OFF_STOLEN_FLAG: u32 = 0x004;
/// 偷下的 dst_surf_obj 指標。
pub const OFF_STOLEN_DST_SURF_OBJ: u32 = 0x008;
/// 偷下的 descriptor(16 bytes)。
pub const OFF_STOLEN_DESCRIPTOR: u32 = 0x00C;
/// launcher 寫入:本輪要畫幾筆。
pub const OFF_DRAW_COUNT: u32 = 0x040;
/// launcher 寫入:[gfxid:u16][pad:u16][x:i32][y:i32] × N,每筆 12 bytes。
pub const OFF_DRAW_LIST: u32 = 0x044;
/// 我們組裝的 ImageElement 模板,80 bytes。
pub const OFF_TEMPLATE: u32 = 0x200;
/// shellcode 起始(含 TRAMPOLINE 在尾)。
pub const OFF_SHELLCODE: u32 = 0x280;
/// 整 cave 大小。
pub const CAVE_SIZE: usize = 0x600;

/// 一筆 12-byte DrawCmd 在 DRAW_LIST 裡的大小。
pub const DRAW_ENTRY_BYTES: usize = 12;
/// DRAW_LIST 可用容量(到 TEMPLATE 之前)。
pub const MAX_DRAW_CMDS: usize = ((OFF_TEMPLATE - OFF_DRAW_LIST) as usize) / DRAW_ENTRY_BYTES;

pub struct ImageDrawHandle {
    pub cave: u32,
}

/// 安裝 hook。 失敗代表完全沒打通,主流程應該 log 但不 panic。
pub fn install(h: HANDLE, pid: u32) -> Result<ImageDrawHandle> {
    // 1) 驗 prologue
    let bytes = memory::read_bytes(h, HOOK_ADDR, HOOK_LEN)
        .context("讀 ImageElement_draw prologue 失敗")?;
    if bytes[..HOOK_LEN] != ORIGINAL_PROLOGUE {
        bail!(
            "ImageElement_draw prologue 不符: 實際 {:02X?} 預期 {:02X?}",
            &bytes[..HOOK_LEN],
            ORIGINAL_PROLOGUE
        );
    }

    // 2) 配 cave
    let cave = memory::alloc_exec(h, CAVE_SIZE).context("alloc image_draw cave 失敗")?;
    memory::write_code(h, cave, &vec![0u8; CAVE_SIZE]).context("zero cave 失敗")?;

    // 3) 寫 shellcode
    let shellcode = build_shellcode(cave);
    if shellcode.len() > (CAVE_SIZE - OFF_SHELLCODE as usize) {
        bail!(
            "shellcode {} bytes 放不進 cave 尾部 {} bytes",
            shellcode.len(),
            CAVE_SIZE - OFF_SHELLCODE as usize
        );
    }
    memory::write_code(h, cave + OFF_SHELLCODE, &shellcode)
        .context("寫 shellcode 失敗")?;

    // 4) Patch hook site:7 bytes = E9 disp32 + 2×NOP
    let mut patch = [0x90u8; 7];
    patch[0] = 0xE9;
    let disp = ((cave + OFF_SHELLCODE) as i64 - (HOOK_ADDR + 5) as i64) as i32;
    patch[1..5].copy_from_slice(&disp.to_le_bytes());

    let threads = process::suspend_threads(pid)?;
    let res = memory::write_code(h, HOOK_ADDR, &patch);
    process::resume_threads(threads);
    res.context("patch hook site 失敗")?;

    log_line!(
        "[image-draw-hook] installed @ cave=0x{cave:08X} shellcode=0x{:08X}",
        cave + OFF_SHELLCODE
    );
    Ok(ImageDrawHandle { cave })
}

/// 更新 cave 內的 DRAW_LIST + DRAW_COUNT。 先寫 count=0 (atomically 中斷讀),
/// 再寫 entries,最後寫 final count。
pub fn update_list(h: HANDLE, cave: u32, serialized: &[u8]) -> Result<()> {
    if serialized.len() < 4 {
        return Ok(());
    }
    // serialized = [count:u32][entries...]
    let usable_entries = (serialized.len() - 4).min(MAX_DRAW_CMDS * DRAW_ENTRY_BYTES);

    // 1) 暫時把 count 寫 0
    memory::write_code(h, cave + OFF_DRAW_COUNT, &0u32.to_le_bytes())
        .context("zero DRAW_COUNT 失敗")?;
    // 2) 寫 entries
    if usable_entries > 0 {
        memory::write_code(
            h,
            cave + OFF_DRAW_LIST,
            &serialized[4..4 + usable_entries],
        )
        .context("寫 DRAW_LIST entries 失敗")?;
    }
    // 3) 最後寫 final count(可能被截斷)
    let final_count = (usable_entries / DRAW_ENTRY_BYTES) as u32;
    memory::write_code(h, cave + OFF_DRAW_COUNT, &final_count.to_le_bytes())
        .context("寫 DRAW_COUNT 失敗")?;
    Ok(())
}

/// 讀 diag:(invocations, stolen_flag, draw_count)。
pub fn read_diag(h: HANDLE, cave: u32) -> Option<(u32, u32, u32)> {
    let inv = read_u32(h, cave + OFF_INVOCATIONS)?;
    let stolen = read_u32(h, cave + OFF_STOLEN_FLAG)?;
    let count = read_u32(h, cave + OFF_DRAW_COUNT)?;
    Some((inv, stolen, count))
}

fn read_u32(h: HANDLE, addr: u32) -> Option<u32> {
    let b = memory::read_bytes(h, addr, 4).ok()?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// 組 shellcode。
///
/// 進入時:
/// - ESP 指向 game 的 return addr(call 0x42F450 push 的)
/// - ECX = this (game 傳的 ImageElement*)
///
/// pushad 後 stack 順序(由低 → 高,top of stack 在最低):
/// `[EDI][ESI][EBP][ESP_orig][EBX][EDX][ECX][EAX]`
/// 再 pushfd:`[EFLAGS][EDI]...[EAX]`
///
/// 原 ECX 位置:`[esp + 4 + 6*4]` = `[esp + 28]` (4 for EFLAGS, 6*4 to skip EDI/ESI/EBP/ESP_orig/EBX/EDX)
fn build_shellcode(cave: u32) -> Vec<u8> {
    let mut sc = Vec::with_capacity(512);
    let a_inv = cave + OFF_INVOCATIONS;
    let a_stolen_flag = cave + OFF_STOLEN_FLAG;
    let a_stolen_obj = cave + OFF_STOLEN_DST_SURF_OBJ;
    let a_stolen_desc = cave + OFF_STOLEN_DESCRIPTOR;
    let a_count = cave + OFF_DRAW_COUNT;
    let a_list = cave + OFF_DRAW_LIST;
    let a_template = cave + OFF_TEMPLATE;

    // pushad (60) ; pushfd (9C)
    sc.push(0x60);
    sc.push(0x9C);

    // lock inc dword [a_inv]   F0 FF 05 <addr>
    sc.extend_from_slice(&[0xF0, 0xFF, 0x05]);
    sc.extend_from_slice(&a_inv.to_le_bytes());

    // === 偷 dst_surf_obj + descriptor (僅第一次) ===
    // cmp dword [a_stolen_flag], 0    83 3D <addr> 00
    sc.extend_from_slice(&[0x83, 0x3D]);
    sc.extend_from_slice(&a_stolen_flag.to_le_bytes());
    sc.push(0x00);
    // jne .post_steal (rel8)
    sc.push(0x75);
    let jne_steal_pos = sc.len();
    sc.push(0); // patch later

    // mov ebx, [esp + 28]   8B 5C 24 1C   (原 ECX from pushad)
    sc.extend_from_slice(&[0x8B, 0x5C, 0x24, 0x1C]);

    // mov eax, [ebx + 0x1c]    8B 43 1C
    sc.extend_from_slice(&[0x8B, 0x43, 0x1C]);
    // mov [a_stolen_obj], eax  A3 <addr>
    sc.push(0xA3);
    sc.extend_from_slice(&a_stolen_obj.to_le_bytes());

    // Copy 16 bytes from [ebx + 0x3c..+0x4c] to [a_stolen_desc..+16]
    for i in 0u32..4 {
        let off = 0x3C + i * 4;
        // mov eax, [ebx + off]   8B 43 <off-u8>
        sc.extend_from_slice(&[0x8B, 0x43, off as u8]);
        // mov [a_stolen_desc + i*4], eax   A3 <addr>
        sc.push(0xA3);
        sc.extend_from_slice(&(a_stolen_desc + i * 4).to_le_bytes());
    }

    // mov dword [a_stolen_flag], 1    C7 05 <addr> 01 00 00 00
    sc.extend_from_slice(&[0xC7, 0x05]);
    sc.extend_from_slice(&a_stolen_flag.to_le_bytes());
    sc.extend_from_slice(&1u32.to_le_bytes());

    // .post_steal:
    let post_steal = sc.len();
    sc[jne_steal_pos] = (post_steal - jne_steal_pos - 1) as u8;

    // === iterate DRAW_LIST ===
    // mov edi, [a_count]    8B 3D <addr>
    sc.extend_from_slice(&[0x8B, 0x3D]);
    sc.extend_from_slice(&a_count.to_le_bytes());

    // test edi, edi    85 FF
    sc.extend_from_slice(&[0x85, 0xFF]);
    // jz .draw_done (rel32 — 為了保險用 long form)
    sc.extend_from_slice(&[0x0F, 0x84]);
    let jz_done_pos = sc.len();
    sc.extend_from_slice(&0i32.to_le_bytes());

    // mov esi, a_list   BE <addr>
    sc.push(0xBE);
    sc.extend_from_slice(&a_list.to_le_bytes());

    let loop_start = sc.len();

    // --- 組裝 template ---
    // mov eax, [a_stolen_obj]   A1 <addr>
    sc.push(0xA1);
    sc.extend_from_slice(&a_stolen_obj.to_le_bytes());
    // mov [a_template + 0x1c], eax    A3 <addr>
    sc.push(0xA3);
    sc.extend_from_slice(&(a_template + 0x1C).to_le_bytes());

    // Copy descriptor 16 bytes
    for i in 0u32..4 {
        // mov eax, [a_stolen_desc + i*4]
        sc.push(0xA1);
        sc.extend_from_slice(&(a_stolen_desc + i * 4).to_le_bytes());
        // mov [a_template + 0x3C + i*4], eax
        sc.push(0xA3);
        sc.extend_from_slice(&(a_template + 0x3C + i * 4).to_le_bytes());
    }

    // mov byte [a_template + 0x2D], 0    C6 05 <addr> 00   (alpha_flag=0)
    sc.extend_from_slice(&[0xC6, 0x05]);
    sc.extend_from_slice(&(a_template + 0x2D).to_le_bytes());
    sc.push(0x00);

    // movzx eax, word [esi]    0F B7 06   (gfxid)
    sc.extend_from_slice(&[0x0F, 0xB7, 0x06]);
    // mov [a_template + 0x20], eax   A3 <addr>  (寫 32-bit;+0x22 是 pad)
    sc.push(0xA3);
    sc.extend_from_slice(&(a_template + 0x20).to_le_bytes());

    // mov eax, [esi + 4]    8B 46 04   (dst_x)
    sc.extend_from_slice(&[0x8B, 0x46, 0x04]);
    // mov [a_template + 0x04], eax
    sc.push(0xA3);
    sc.extend_from_slice(&(a_template + 0x04).to_le_bytes());

    // mov eax, [esi + 8]    8B 46 08   (dst_y)
    sc.extend_from_slice(&[0x8B, 0x46, 0x08]);
    // mov [a_template + 0x08], eax
    sc.push(0xA3);
    sc.extend_from_slice(&(a_template + 0x08).to_le_bytes());

    // mov ecx, a_template   B9 <addr>
    sc.push(0xB9);
    sc.extend_from_slice(&a_template.to_le_bytes());

    // call TRAMPOLINE (E8 disp32) — patch later
    sc.push(0xE8);
    let call_tramp_pos = sc.len();
    sc.extend_from_slice(&0i32.to_le_bytes());

    // add esi, 12    83 C6 0C
    sc.extend_from_slice(&[0x83, 0xC6, 0x0C]);
    // dec edi    4F
    sc.push(0x4F);
    // jnz .loop_start (rel32)
    sc.extend_from_slice(&[0x0F, 0x85]);
    let jnz_pos = sc.len();
    let jnz_disp = (loop_start as i32) - ((jnz_pos + 4) as i32);
    sc.extend_from_slice(&jnz_disp.to_le_bytes());

    // .draw_done:
    let draw_done = sc.len();
    let jz_disp = (draw_done as i32) - ((jz_done_pos + 4) as i32);
    sc[jz_done_pos..jz_done_pos + 4].copy_from_slice(&jz_disp.to_le_bytes());

    // popfd / popad
    sc.push(0x9D);
    sc.push(0x61);

    // === TRAMPOLINE ===
    // 雙用途:
    //   1. cave 結尾(原 call path),popad 後 ECX = 原 this
    //   2. 我們的 draw loop `call TRAMPOLINE`,ECX = a_template
    let trampoline_off = sc.len();

    // push ebp     55
    sc.push(0x55);
    // mov ebp, esp     8B EC
    sc.extend_from_slice(&[0x8B, 0xEC]);
    // push ecx     51
    sc.push(0x51);
    // mov [ebp - 4], ecx     89 4D FC
    sc.extend_from_slice(&[0x89, 0x4D, 0xFC]);

    // jmp RESUME_ADDR (E9 disp32)
    sc.push(0xE9);
    let jmp_pos = sc.len();
    let from_ip = cave + OFF_SHELLCODE + jmp_pos as u32 + 4;
    let jmp_disp = (RESUME_ADDR as i64 - from_ip as i64) as i32;
    sc.extend_from_slice(&jmp_disp.to_le_bytes());

    // Patch call TRAMPOLINE 的 disp32
    let tramp_runtime = cave + OFF_SHELLCODE + trampoline_off as u32;
    let call_from_ip = cave + OFF_SHELLCODE + call_tramp_pos as u32 + 4;
    let call_disp = (tramp_runtime as i64 - call_from_ip as i64) as i32;
    sc[call_tramp_pos..call_tramp_pos + 4].copy_from_slice(&call_disp.to_le_bytes());

    sc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shellcode_emits_under_cave_size() {
        let sc = build_shellcode(0x10000000);
        let cave_room = CAVE_SIZE - OFF_SHELLCODE as usize;
        assert!(
            sc.len() <= cave_room,
            "shellcode {} > 可用空間 {}",
            sc.len(),
            cave_room
        );
    }

    #[test]
    fn shellcode_starts_with_pushad_pushfd() {
        let sc = build_shellcode(0x10000000);
        assert_eq!(sc[0], 0x60);
        assert_eq!(sc[1], 0x9C);
    }

    #[test]
    fn shellcode_ends_with_trampoline_jmp() {
        let sc = build_shellcode(0x10000000);
        // 最後 5 bytes: E9 disp32
        let n = sc.len();
        assert_eq!(sc[n - 5], 0xE9);
        let disp = i32::from_le_bytes([sc[n - 4], sc[n - 3], sc[n - 2], sc[n - 1]]);
        // 還原 disp 算 target:from_ip = cave + OFF_SHELLCODE + (n - 5 + 5) = cave + OFF_SHELLCODE + n
        let cave = 0x10000000u32;
        let from_ip = cave + OFF_SHELLCODE + n as u32;
        let target = (from_ip as i64 + disp as i64) as u32;
        assert_eq!(target, RESUME_ADDR);
    }

    #[test]
    fn trampoline_bytes_match_original_prologue() {
        let sc = build_shellcode(0x10000000);
        let n = sc.len();
        // trampoline 是最後 12 bytes: 55 8B EC 51 89 4D FC E9 disp32
        let tramp_start = n - 12;
        let expected: [u8; 7] = ORIGINAL_PROLOGUE;
        assert_eq!(&sc[tramp_start..tramp_start + 7], &expected);
    }

    #[test]
    fn max_draw_cmds_reasonable() {
        // 至少能放 10 toast × 2 PNG + 5 float = 25 entries
        assert!(MAX_DRAW_CMDS >= 25);
    }
}
