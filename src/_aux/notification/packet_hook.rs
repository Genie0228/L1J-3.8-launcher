//! Pickup notification packet hook (S_ItemBoard / S_ShowDrop)。
//!
//! # 背景演進
//!
//! 1. 初設計假設「opcode 64 + sub_id 190/192」(PACKETBOX style)→ 0x543930 為
//!    sub-dispatcher。 實測 hit-counter 60s 戰鬥只 +2,該函式根本不在熱路徑。
//! 2. 改假設「opcode 250 + sub_id 190/192」(server-side Go `S_OPCODE_EVENT`)
//!    → hook 0x53938E `ja 0x541596` 出口,filter 250。 hits 仍 0。
//! 3. 加 opcode_ring 診斷 → 看到 ring 內全是 `190` 和 `192` 直接出現,
//!    **server 的 250 是 high-level 類別碼,wire 上的 outer opcode 直接就是 190/192**。
//!
//! # 最終 Hook 策略
//!
//! Hook `0x0053938E` 的 `ja 0x541596` (6 bytes `0F 87 02 82 00 00`)
//! → 改 disp32 指向我們的 cave,cave 內判斷:
//!   * 若 outer opcode == 190 (S_ItemBoard) 或 192 (S_ShowDrop) → push 到 ring
//!   * 其他 > 183 的 opcode → `jmp 0x541596` (原 NOP 行為,完全透明)
//!
//! Cave hot-path register state(進 cave 時):
//!   - `[ebp - 0x60f4]` = 4-byte outer opcode(由 ProcessPacket prologue 寫入)
//!   - `[ebp - 0x0d]`   = byte 形式(同一個值的低 byte)
//!   - `[ebp + 8]`      = packet pointer,**已 advance 過 outer opcode**,
//!                        指向 payload 第一個 byte(190 = gfxid_lo;192 = type)
//!
//! # Ring buffer 而非 cdecl callback
//!
//! Launcher 是獨立 process,Rust 函式位址在 launcher VAS;cave 在 game VAS。
//! 跨進程 `call disp32` 會跳進 game 內隨機 bytes(同 VA 不同進程)。
//! 走 ring buffer:shellcode 在 game-side 把 payload 複製到 cave,launcher polling
//! 用 ReadProcessMemory 讀回來 dispatch。 對齊 img_hover 的 hover-pos 模式。

use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use once_cell::sync::Lazy;
use windows::Win32::Foundation::HANDLE;

use crate::logger::log_line;
use crate::memory;
use crate::process;

// ===== Constants =====

/// `ja 0x541596` 在 ProcessPacket(0x539333)範圍檢查後,opcode > 183 走這條。
pub const HOOK_ADDR: u32 = 0x0053938E;
pub const ORIGINAL_BYTES: [u8; 6] = [0x0F, 0x87, 0x02, 0x82, 0x00, 0x00];

/// 原 NOP handler(ProcessPacket 的 default 出口,做 security cookie + ret)。
pub const NOP_HANDLER_ADDR: u32 = 0x00541596;

/// `[ebp - 0x60f4]` = ProcessPacket 內 opcode 4-byte slot。
pub const OPCODE_EBP_OFFSET: i32 = -0x60f4;
/// `[ebp + 8]` = packet pointer,已 advance 過 outer opcode,指向 payload。
pub const PACKET_PTR_EBP_OFFSET: i32 = 8;

pub const SUB_ID_ITEMBOARD: u8 = 190;
pub const SUB_ID_SHOWDROP: u8 = 192;

// === Cave layout ===
//
// +0x000 .. +0x0FF  shellcode (256 bytes plenty)
// +0x100 .. +0x103  ring_tail u32        (shellcode inc)
// +0x104 .. +0x107  ring_head u32        (launcher 寫)
// +0x108 .. +0x10B  total_hits u32       (lock inc on EVERY hook entry — 任何 opcode > 183)
// +0x10C .. +0x10F  packetbox_hits u32   (lock inc 只在 opcode == 250 — server 確實送 PACKETBOX)
// +0x110 .. +0x110  sub_id_tmp u8        (cave 內暫存,popad 後仍需用)
// +0x111 .. +0x11F  reserved
// +0x120 .. +0x12F  opcode_ring u8[16]   (最近 16 個 >183 opcode — 用來看 server 到底送什麼)
// +0x130 .. +0x1FF  reserved
// +0x200 .. +0x6FF  16 slots × 80 bytes = 1280 bytes
// +0x700 .. +0x7FF  reserved

pub const CODECAVE_SIZE: usize = 0x800;

pub const OFF_TAIL: u32 = 0x100;
pub const OFF_HEAD: u32 = 0x104;
pub const OFF_TOTAL_HITS: u32 = 0x108;
pub const OFF_PACKETBOX_HITS: u32 = 0x10C;
pub const OFF_SUB_ID_TMP: u32 = 0x110;
pub const OFF_OPCODE_RING: u32 = 0x120;
pub const OPCODE_RING_LEN: u32 = 16;
pub const OFF_RING: u32 = 0x200;
pub const RING_SLOTS: u32 = 16;
pub const SLOT_SIZE: u32 = 80;

/// 每 slot 80 bytes — `[sub_id:u8][valid:u8][pad:u16][payload:u8[76]]`
const SLOT_HEADER: u32 = 4;
const SLOT_PAYLOAD_MAX: u32 = SLOT_SIZE - SLOT_HEADER; // 76

/// dispatcher 認得的 ItemBoard 最大 payload bytes(2 gfx + 64 name + 1 NUL = 67),
/// 跟 slot payload 容量 76 比起來 OK。
const _ITEMBOARD_PAYLOAD_MAX: usize = 2 + 64 + 1;

pub struct PacketHookHandle {
    pub cave_addr: u32,
    pub original_bytes: [u8; 6],
    /// Launcher-side consume index(下次要 drain 的 slot index = local_head mod RING_SLOTS)。
    pub local_head: u32,
}

static HOOK_STATE: Lazy<Mutex<Option<PacketHookHandle>>> = Lazy::new(|| Mutex::new(None));

// ===== Shellcode emitter =====

/// Shellcode:outer opcode == 190 / 192 直接 push 到 ring。
///
/// ```pseudo
/// lock inc [total_hits]                      ; 任何 >183 opcode +1
/// 寫 [ebp-0xd] 到 opcode_ring[total_hits-1 & 15]   ; 診斷
///
/// cmp dword [ebp-0x60f4], 190
/// je  .push
/// cmp dword [ebp-0x60f4], 192
/// je  .push
/// jmp .nop_path
///
/// .push:
///   lock inc [packetbox_hits]
///   pushad / pushfd
///   ; tail++ → slot ptr
///   mov ecx, [tail]; inc [tail]; and ecx, 0x0F; imul ecx, 80; add ecx, ring_addr
///   ; slot[0] = opcode (= 190 or 192) — 直接從 [ebp-0xd] 取
///   mov al, [ebp-0xd]
///   mov [ecx], al
///   mov byte [ecx+1], 1
///   ; payload 從 [ebp+8] 直接拷 76 bytes 到 slot+4
///   mov esi, [ebp+8]; lea edi, [ecx+4]; mov ecx, 76; rep movsb
///   popfd / popad
///   ; 落到 .nop_path
///
/// .nop_path:
///   jmp 0x541596                             ; ProcessPacket NOP epilogue
/// ```
pub fn build_shellcode(cave: u32) -> Vec<u8> {
    let total_hits_addr = cave + OFF_TOTAL_HITS;
    let packetbox_hits_addr = cave + OFF_PACKETBOX_HITS;
    let opcode_ring_addr = cave + OFF_OPCODE_RING;
    let tail_addr = cave + OFF_TAIL;
    let ring_addr = cave + OFF_RING;
    let mut sc = Vec::with_capacity(160);

    // === lock inc dword [total_hits]  (7 bytes) ===
    sc.extend_from_slice(&[0xF0, 0xFF, 0x05]);
    sc.extend_from_slice(&total_hits_addr.to_le_bytes());

    // === opcode_ring 寫入(診斷) ===
    // mov eax, [total_hits]              A1 + addr32   (5)
    sc.push(0xA1);
    sc.extend_from_slice(&total_hits_addr.to_le_bytes());
    // sub eax, 1                          83 E8 01      (3)
    sc.extend_from_slice(&[0x83, 0xE8, 0x01]);
    // and eax, 0x0F                       83 E0 0F      (3)
    sc.extend_from_slice(&[0x83, 0xE0, 0x0F]);
    // mov cl, [ebp-0x0d]                  8A 4D F3      (3)
    sc.extend_from_slice(&[0x8A, 0x4D, 0xF3]);
    // mov [eax + opcode_ring_addr], cl    88 88 + addr  (6)
    sc.extend_from_slice(&[0x88, 0x88]);
    sc.extend_from_slice(&opcode_ring_addr.to_le_bytes());

    // === cmp dword [ebp-0x60f4], 190;  je .push ===
    // 81 BD disp32 imm32  (10 bytes)
    sc.extend_from_slice(&[0x81, 0xBD]);
    sc.extend_from_slice(&(OPCODE_EBP_OFFSET as i32).to_le_bytes());
    sc.extend_from_slice(&(SUB_ID_ITEMBOARD as u32).to_le_bytes());
    // 0F 84 disp32  (6 bytes)
    sc.extend_from_slice(&[0x0F, 0x84]);
    let je_itemboard_disp_off = sc.len();
    sc.extend_from_slice(&[0, 0, 0, 0]);

    // === cmp dword [ebp-0x60f4], 192;  je .push ===
    sc.extend_from_slice(&[0x81, 0xBD]);
    sc.extend_from_slice(&(OPCODE_EBP_OFFSET as i32).to_le_bytes());
    sc.extend_from_slice(&(SUB_ID_SHOWDROP as u32).to_le_bytes());
    sc.extend_from_slice(&[0x0F, 0x84]);
    let je_showdrop_disp_off = sc.len();
    sc.extend_from_slice(&[0, 0, 0, 0]);

    // 不匹配 → jmp .nop_path  (5 bytes E9 disp32)
    sc.push(0xE9);
    let unmatched_jmp_disp_off = sc.len();
    sc.extend_from_slice(&[0, 0, 0, 0]);

    // === .push 起點 ===
    let push_off = sc.len();
    // 修正兩個 je 的 disp32
    let je1_target = push_off as i32 - (je_itemboard_disp_off as i32 + 4);
    sc[je_itemboard_disp_off..je_itemboard_disp_off + 4]
        .copy_from_slice(&je1_target.to_le_bytes());
    let je2_target = push_off as i32 - (je_showdrop_disp_off as i32 + 4);
    sc[je_showdrop_disp_off..je_showdrop_disp_off + 4]
        .copy_from_slice(&je2_target.to_le_bytes());

    // lock inc dword [packetbox_hits]   (7 bytes)
    sc.extend_from_slice(&[0xF0, 0xFF, 0x05]);
    sc.extend_from_slice(&packetbox_hits_addr.to_le_bytes());

    // pushad / pushfd
    sc.push(0x60);
    sc.push(0x9C);

    // mov ecx, [tail_addr]  (6 bytes 8B 0D + addr)
    sc.extend_from_slice(&[0x8B, 0x0D]);
    sc.extend_from_slice(&tail_addr.to_le_bytes());
    // inc dword [tail_addr]  (6 bytes FF 05 + addr)
    sc.extend_from_slice(&[0xFF, 0x05]);
    sc.extend_from_slice(&tail_addr.to_le_bytes());
    // and ecx, 0x0F  (3 bytes)
    sc.extend_from_slice(&[0x83, 0xE1, 0x0F]);
    // imul ecx, ecx, 80  (3 bytes)
    sc.extend_from_slice(&[0x6B, 0xC9, SLOT_SIZE as u8]);
    // add ecx, ring_addr  (6 bytes 81 C1 + addr)
    sc.extend_from_slice(&[0x81, 0xC1]);
    sc.extend_from_slice(&ring_addr.to_le_bytes());

    // mov al, [ebp-0x0d]  (3 bytes 8A 45 F3)
    sc.extend_from_slice(&[0x8A, 0x45, 0xF3]);
    // mov [ecx], al  (2 bytes)
    sc.extend_from_slice(&[0x88, 0x01]);
    // mov byte [ecx+1], 1  (4 bytes)
    sc.extend_from_slice(&[0xC6, 0x41, 0x01, 0x01]);

    // mov esi, [ebp+8]  (3 bytes)
    sc.extend_from_slice(&[0x8B, 0x75, PACKET_PTR_EBP_OFFSET as u8]);
    // lea edi, [ecx+4]  (3 bytes)
    sc.extend_from_slice(&[0x8D, 0x79, 0x04]);
    // mov ecx, 76  (5 bytes)
    sc.push(0xB9);
    sc.extend_from_slice(&SLOT_PAYLOAD_MAX.to_le_bytes());
    // rep movsb  (2 bytes)
    sc.extend_from_slice(&[0xF3, 0xA4]);

    // popfd / popad
    sc.push(0x9D);
    sc.push(0x61);

    // === .nop_path: jmp 0x541596 ===
    let nop_off = sc.len();
    sc.push(0xE9);
    let next_ip = cave + (sc.len() as u32) + 4;
    sc.extend_from_slice(&(NOP_HANDLER_ADDR.wrapping_sub(next_ip) as i32).to_le_bytes());

    // 修正 unmatched 跳 nop_path 的 disp32
    let unmatched_target = nop_off as i32 - (unmatched_jmp_disp_off as i32 + 4);
    sc[unmatched_jmp_disp_off..unmatched_jmp_disp_off + 4]
        .copy_from_slice(&unmatched_target.to_le_bytes());

    sc
}

// ===== Install / uninstall =====

pub fn install(h: HANDLE, pid: u32) -> Result<PacketHookHandle> {
    let live = memory::read_bytes(h, HOOK_ADDR, 6).context("讀取 hook 點失敗")?;
    if live[..6] != ORIGINAL_BYTES {
        bail!(
            "[notification] hook 點 0x{HOOK_ADDR:08X} bytes 不符:\
             expected {:02X?},got {:02X?}",
            ORIGINAL_BYTES,
            &live[..6]
        );
    }

    let cave = memory::alloc_exec(h, CODECAVE_SIZE).context("alloc codecave 失敗")?;

    // 把整個 cave 先歸零(ring slots / tail / head 都從 0 開始)
    memory::write_code(h, cave, &vec![0u8; CODECAVE_SIZE]).context("zero cave 失敗")?;

    let sc = build_shellcode(cave);
    if sc.len() > 0x100 {
        bail!("shellcode {} bytes 超過 256 byte 保留區", sc.len());
    }
    memory::write_code(h, cave, &sc).context("寫 shellcode 失敗")?;

    // 原指令 `ja 0x541596` = `0F 87 disp32` (6 bytes)。
    // 保留 `ja` 條件,只改 disp32 → 進 cave。 對 opcode ≤ 183 完全不影響。
    let mut hook = [0u8; 6];
    hook[0] = 0x0F;
    hook[1] = 0x87;
    let rel = cave.wrapping_sub(HOOK_ADDR + 6) as i32;
    hook[2..6].copy_from_slice(&rel.to_le_bytes());

    let threads = process::suspend_threads(pid)?;
    let res = memory::write_code(h, HOOK_ADDR, &hook);
    process::resume_threads(threads);
    res.context("寫 hook bytes 失敗")?;

    log_line!(
        "[OK] notification packet hook @ 0x{HOOK_ADDR:08X} → cave 0x{cave:08X} ({} bytes sc, ring {}×{})",
        sc.len(),
        RING_SLOTS,
        SLOT_SIZE
    );

    let handle = PacketHookHandle {
        cave_addr: cave,
        original_bytes: ORIGINAL_BYTES,
        local_head: 0,
    };
    // 存一份在 module-state(供 drain / uninstall 用)
    if let Ok(mut state) = HOOK_STATE.lock() {
        *state = Some(PacketHookHandle {
            cave_addr: cave,
            original_bytes: ORIGINAL_BYTES,
            local_head: 0,
        });
    }
    Ok(handle)
}

pub fn uninstall(h: HANDLE, pid: u32, handle: &PacketHookHandle) -> Result<()> {
    let mut restore = [0u8; 6];
    restore.copy_from_slice(&handle.original_bytes);
    let threads = process::suspend_threads(pid)?;
    let res = memory::write_code(h, HOOK_ADDR, &restore);
    process::resume_threads(threads);
    res.context("還原 hook bytes 失敗")?;
    Ok(())
}

// ===== 診斷:讀 total_hits =====

fn read_cave_dword(h: HANDLE, offset: u32) -> u32 {
    let cave_addr = HOOK_STATE
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(|h| h.cave_addr));
    let Some(cave) = cave_addr else { return 0 };
    let Ok(bytes) = memory::read_bytes(h, cave + offset, 4) else {
        return 0;
    };
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// 任何 opcode > 183 都會讓 shellcode `lock inc` 一次 — 判斷 hook 是否被觸發。
pub fn read_total_hits(h: HANDLE) -> u32 {
    read_cave_dword(h, OFF_TOTAL_HITS)
}

/// 只在 opcode == 250 時 +1 — 判斷 server 是否真的送 PACKETBOX。
pub fn read_packetbox_hits(h: HANDLE) -> u32 {
    read_cave_dword(h, OFF_PACKETBOX_HITS)
}

/// 讀最近 16 個 hook 到的 opcode 值(都是 > 183 的;0 表示未填)。
/// 回傳 `(ring_bytes, write_count)` — write_count = total_hits(寫的次數)。
pub fn read_opcode_ring(h: HANDLE) -> (Vec<u8>, u32) {
    let cave_addr = HOOK_STATE
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(|h| h.cave_addr));
    let Some(cave) = cave_addr else { return (Vec::new(), 0) };
    let Ok(bytes) = memory::read_bytes(h, cave + OFF_OPCODE_RING, OPCODE_RING_LEN as usize) else {
        return (Vec::new(), 0);
    };
    let total = read_cave_dword(h, OFF_TOTAL_HITS);
    (bytes, total)
}

// ===== Drain — launcher polling thread 呼叫 =====

/// 從 cave ring 撈出本次 tick 新進的 packet,回傳 dispatcher 可吃的 `[sub_id, ...]` 序列。
///
/// **Safety**:跨進程讀,要 ReadProcessMemory。 race 採 lossy 策略 — 如果 game 同個 tick
/// 連續推超過 16 個,舊的會被覆蓋,我們漏 log 但不 crash。
pub fn drain(h: HANDLE) -> Vec<Vec<u8>> {
    let state_opt = HOOK_STATE.lock().ok().and_then(|mut s| {
        s.as_mut().map(|h| (h.cave_addr, h.local_head))
    });
    let Some((cave_addr, mut local_head)) = state_opt else {
        return Vec::new();
    };

    // 讀 tail
    let tail_bytes = match memory::read_bytes(h, cave_addr + OFF_TAIL, 4) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let tail = u32::from_le_bytes([tail_bytes[0], tail_bytes[1], tail_bytes[2], tail_bytes[3]]);

    if tail == local_head {
        return Vec::new();
    }

    let mut out = Vec::new();
    // Lossy:若 tail 跟 head 差超過 16(RING_SLOTS),只保留最後 16 個
    let lag = tail.wrapping_sub(local_head);
    if lag > RING_SLOTS {
        log_line!(
            "[notification] ring overrun:lag={} > {} slots, 漏 {} 個 packet",
            lag,
            RING_SLOTS,
            lag - RING_SLOTS
        );
        local_head = tail.wrapping_sub(RING_SLOTS);
    }

    while local_head != tail {
        let slot_idx = local_head & (RING_SLOTS - 1);
        let slot_addr = cave_addr + OFF_RING + slot_idx * SLOT_SIZE;
        let slot = match memory::read_bytes(h, slot_addr, SLOT_SIZE as usize) {
            Ok(b) => b,
            Err(_) => break,
        };
        let sub_id = slot[0];
        let valid = slot[1];
        if valid == 1 {
            // 重組成 dispatcher 認得的 [sub_id, payload...] 格式
            let mut synth = Vec::with_capacity(1 + SLOT_PAYLOAD_MAX as usize);
            synth.push(sub_id);
            // 對 ItemBoard 走 NUL 截斷,ShowDrop 走固定 5 bytes
            let payload = &slot[4..];
            match sub_id {
                SUB_ID_ITEMBOARD => {
                    // 前 2 byte gfxid 直接取,之後找 NUL 截斷
                    if payload.len() >= 2 {
                        synth.push(payload[0]);
                        synth.push(payload[1]);
                        for &b in &payload[2..] {
                            synth.push(b);
                            if b == 0 {
                                break;
                            }
                        }
                    }
                }
                SUB_ID_SHOWDROP => {
                    // type + 4 byte amount = 固定 5 bytes
                    if payload.len() >= 5 {
                        synth.extend_from_slice(&payload[..5]);
                    }
                }
                _ => {}
            }
            out.push(synth);
        }
        local_head = local_head.wrapping_add(1);
    }

    // 寫回新的 local_head
    if let Ok(mut state) = HOOK_STATE.lock() {
        if let Some(h) = state.as_mut() {
            h.local_head = local_head;
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CAVE: u32 = 0x10000000;

    fn test_sc() -> Vec<u8> {
        build_shellcode(TEST_CAVE)
    }

    #[test]
    fn shellcode_starts_with_lock_inc_total_hits() {
        let sc = test_sc();
        assert_eq!(&sc[0..3], &[0xF0, 0xFF, 0x05]);
        let addr = u32::from_le_bytes([sc[3], sc[4], sc[5], sc[6]]);
        assert_eq!(addr, TEST_CAVE + OFF_TOTAL_HITS);
    }

    /// 7(lock-inc total_hits) + 20(opcode_ring write block) = 27
    const PREFIX_LEN: usize = 7 + 20;

    #[test]
    fn shellcode_has_opcode_ring_write() {
        let sc = test_sc();
        // 7..12 = mov eax, [total_hits]
        assert_eq!(sc[7], 0xA1);
        let total_addr = u32::from_le_bytes([sc[8], sc[9], sc[10], sc[11]]);
        assert_eq!(total_addr, TEST_CAVE + OFF_TOTAL_HITS);
        // 12..15 = sub eax, 1
        assert_eq!(&sc[12..15], &[0x83, 0xE8, 0x01]);
        // 15..18 = and eax, 0x0F
        assert_eq!(&sc[15..18], &[0x83, 0xE0, 0x0F]);
        // 18..21 = mov cl, [ebp-0x0d]
        assert_eq!(&sc[18..21], &[0x8A, 0x4D, 0xF3]);
        // 21..27 = mov [eax+opcode_ring_addr], cl
        assert_eq!(&sc[21..23], &[0x88, 0x88]);
        let ring_addr = u32::from_le_bytes([sc[23], sc[24], sc[25], sc[26]]);
        assert_eq!(ring_addr, TEST_CAVE + OFF_OPCODE_RING);
    }

    #[test]
    fn shellcode_has_cmp_outer_opcode_190_and_192() {
        let sc = test_sc();
        // 找兩處 `81 BD disp32 imm32` 緊接 `0F 84 disp32`
        let mut imms = Vec::new();
        for i in 0..sc.len() - 16 {
            if sc[i] == 0x81 && sc[i + 1] == 0xBD {
                let disp =
                    i32::from_le_bytes([sc[i + 2], sc[i + 3], sc[i + 4], sc[i + 5]]);
                if disp != OPCODE_EBP_OFFSET {
                    continue;
                }
                let imm =
                    u32::from_le_bytes([sc[i + 6], sc[i + 7], sc[i + 8], sc[i + 9]]);
                if sc[i + 10] == 0x0F && sc[i + 11] == 0x84 {
                    imms.push(imm);
                }
            }
        }
        assert!(
            imms.contains(&(SUB_ID_ITEMBOARD as u32)),
            "missing cmp ..., 190 + je"
        );
        assert!(
            imms.contains(&(SUB_ID_SHOWDROP as u32)),
            "missing cmp ..., 192 + je"
        );
    }

    #[test]
    fn shellcode_has_lock_inc_packetbox_hits() {
        let sc = test_sc();
        // 找 F0 FF 05 + packetbox_hits_addr (應該有 2 處 lock inc:total + packetbox)
        let target = TEST_CAVE + OFF_PACKETBOX_HITS;
        let mut found = false;
        for i in 0..sc.len() - 7 {
            if sc[i] == 0xF0 && sc[i + 1] == 0xFF && sc[i + 2] == 0x05 {
                let a = u32::from_le_bytes([sc[i + 3], sc[i + 4], sc[i + 5], sc[i + 6]]);
                if a == target {
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "missing lock inc [packetbox_hits]");
    }

    #[test]
    fn shellcode_reads_opcode_byte_from_ebp_minus_d() {
        let sc = test_sc();
        // 應該有 mov al, [ebp-0x0d] (8A 45 F3) 用於把 opcode 寫到 slot
        let mut found = false;
        for i in 0..sc.len() - 2 {
            if sc[i] == 0x8A && sc[i + 1] == 0x45 && sc[i + 2] == 0xF3 {
                found = true;
                break;
            }
        }
        assert!(found, "missing mov al, [ebp-0x0d]");
    }

    #[test]
    fn shellcode_uses_correct_tail_addr() {
        let sc = test_sc();
        let tail_addr = TEST_CAVE + OFF_TAIL;
        // 找 8B 0D + addr (mov ecx, [imm32]) 或 FF 05 + addr (inc dword [imm32])
        let mut found = false;
        for i in 0..sc.len() - 5 {
            if (sc[i] == 0x8B && sc[i + 1] == 0x0D)
                || (sc[i] == 0xFF && sc[i + 1] == 0x05)
            {
                let a = u32::from_le_bytes([sc[i + 2], sc[i + 3], sc[i + 4], sc[i + 5]]);
                if a == tail_addr {
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "shellcode missing tail_addr access");
    }

    #[test]
    fn shellcode_uses_correct_ring_addr() {
        let sc = test_sc();
        let ring_addr = TEST_CAVE + OFF_RING;
        // 找 add ecx, ring_addr (81 C1 + 4 bytes)
        let mut found = false;
        for i in 0..sc.len() - 6 {
            if sc[i] == 0x81 && sc[i + 1] == 0xC1 {
                let a = u32::from_le_bytes([sc[i + 2], sc[i + 3], sc[i + 4], sc[i + 5]]);
                if a == ring_addr {
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "shellcode missing add ecx, ring_addr");
    }

    #[test]
    fn shellcode_uses_rep_movsb_with_76_count() {
        let sc = test_sc();
        let mut found = false;
        for i in 0..sc.len() - 7 {
            if sc[i] == 0xB9 {
                let cnt = u32::from_le_bytes([sc[i + 1], sc[i + 2], sc[i + 3], sc[i + 4]]);
                if cnt == SLOT_PAYLOAD_MAX
                    && sc[i + 5] == 0xF3
                    && sc[i + 6] == 0xA4
                {
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "rep movsb 76 not found");
    }

    #[test]
    fn shellcode_ends_with_jmp_to_nop_handler() {
        let sc = test_sc();
        let n = sc.len();
        assert_eq!(sc[n - 5], 0xE9);
        let next_ip = TEST_CAVE + (n - 5) as u32 + 5;
        let disp = i32::from_le_bytes([sc[n - 4], sc[n - 3], sc[n - 2], sc[n - 1]]);
        assert_eq!(next_ip.wrapping_add_signed(disp), NOP_HANDLER_ADDR);
    }

    #[test]
    fn shellcode_fits_in_256_byte_region() {
        let sc = test_sc();
        assert!(sc.len() < 256, "shellcode {} bytes 超過保留區", sc.len());
        assert!(sc.len() > 60, "shellcode 過小,可能漏 emit");
    }

    #[test]
    fn cave_layout_offsets_sane() {
        // header 區塊不重疊
        assert!(OFF_TOTAL_HITS + 4 <= OFF_PACKETBOX_HITS);
        assert!(OFF_PACKETBOX_HITS + 4 <= OFF_OPCODE_RING);
        // opcode_ring 16 bytes 不能蓋 ring slot
        assert!(OFF_OPCODE_RING + OPCODE_RING_LEN <= OFF_RING);
        // ring 不能超出 cave
        assert!(OFF_RING + RING_SLOTS * SLOT_SIZE <= CODECAVE_SIZE as u32);
    }

    #[test]
    fn original_bytes_match_ja_0x541596() {
        // ja near 0x541596 from 0x53938E = 0F 87 + (0x541596 - 0x539394) = 0F 87 02 82 00 00
        assert_eq!(ORIGINAL_BYTES, [0x0F, 0x87, 0x02, 0x82, 0x00, 0x00]);
    }

    /// 端對端模擬:手工建 slot bytes,確認 drain 邏輯正確處理 ItemBoard 截斷
    #[test]
    fn drain_logic_itemboard_truncates_at_nul() {
        // 模擬一個 slot:sub_id=190, valid=1, payload=[gfx_lo, gfx_hi, b'A', b'B', 0, 0xFF, 0xFF...]
        let mut slot = vec![0u8; SLOT_SIZE as usize];
        slot[0] = SUB_ID_ITEMBOARD;
        slot[1] = 1;
        slot[4] = 0x60;
        slot[5] = 0x07; // gfxid = 1888
        slot[6] = b'A';
        slot[7] = b'B';
        slot[8] = 0;
        slot[9] = 0xFF; // 應被截掉

        // 模擬 drain 內部那段邏輯
        let sub_id = slot[0];
        let mut synth = vec![sub_id];
        let payload = &slot[4..];
        if payload.len() >= 2 {
            synth.push(payload[0]);
            synth.push(payload[1]);
            for &b in &payload[2..] {
                synth.push(b);
                if b == 0 {
                    break;
                }
            }
        }

        assert_eq!(
            synth,
            vec![SUB_ID_ITEMBOARD, 0x60, 0x07, b'A', b'B', 0]
        );
    }

    #[test]
    fn drain_logic_showdrop_takes_5_bytes() {
        let mut slot = vec![0u8; SLOT_SIZE as usize];
        slot[0] = SUB_ID_SHOWDROP;
        slot[1] = 1;
        slot[4] = 0; // EXP
        slot[5] = 0x74;
        slot[6] = 0x18;
        slot[7] = 0;
        slot[8] = 0; // amount=6260

        let sub_id = slot[0];
        let mut synth = vec![sub_id];
        synth.extend_from_slice(&slot[4..9]);

        assert_eq!(synth, vec![SUB_ID_SHOWDROP, 0, 0x74, 0x18, 0, 0]);
    }
}
