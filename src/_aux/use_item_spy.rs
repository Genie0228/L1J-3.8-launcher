//! SendPacketData 偵察 hook — 抓 use_item 函數位址用
//!
//! 目的:在玩家手動使用物品時,記錄 SendPacketData 的呼叫者位址 + 封包內容,
//! 反推到遊戲內部的 use_item 函數,讓 drink_hook 可以用 RemoteThread 自動呼叫。
//!
//! 設計:
//! - 在 SendPacketData(0x580E50) 入口裝 5-byte JMP inline hook,
//!   跳到 codecave 內的 logger shellcode。
//! - codecave 結構:
//!   - 0x0000..0x1000  log buffer(64 entries × 64 bytes ring buffer)
//!   - 0x1000..0x1004  log_index(u32,單調遞增,讀取時 mod 64)
//!   - 0x1004..0x1010  保留(對齊)
//!   - 0x1010..       shellcode + saved orig 5 bytes + jmp back
//! - 每個 log entry(64 bytes):
//!   - +0   (4B) caller return address
//!   - +4   (4B) arg1: target_buf 全域位址
//!   - +8   (4B) arg2: opcode
//!   - +12  (4B) arg3
//!   - +16  (4B) arg4
//!   - +20  (4B) arg5
//!   - +24  (4B) arg6
//!   - +28  (32B) [target_buf] 前 32 bytes(實際封包內容)
//!   - +60  (4B) magic 0xFEEDFACE(read 端用來判斷 entry 是否寫滿)
//! - launcher 端 polling thread 每 200ms 讀 log_index,把新增的 entry decode 成 log_line!
//!
//! 用完拆 — install 函數會回傳 codecave 位址讓 main 之後可以 uninstall。

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows::Win32::Foundation::HANDLE;

use crate::aux::address::SEND_PACKET_DATA;
use crate::logger::log_line;
use crate::memory::{alloc_exec, read_bytes, read_u32, write_code};

const HOOK_LEN: usize = 5;
/// 原始 prologue 要相對搬移的最少 byte 數(SendPacketData @ 0x580E50 是 8 bytes:
/// `55 8B EC B8 0C 14 00 00` = push ebp + mov ebp,esp + mov eax,0x140C)。
/// 5-byte JMP 之後補 3 NOP(0x90)填空,讓 fall-through 不會踩中段指令。
const RELOC_LEN: usize = 8;
const ENTRY_SIZE: usize = 128;
const RING_LEN: usize = 64;
const LOG_BUF_OFF: u32 = 0x0000;
const LOG_IDX_OFF: u32 = 0x2000;
const SHELLCODE_OFF: u32 = 0x2020;
const ENTRY_MAGIC: u32 = 0xFEED_FACE;
const CAVE_SIZE: usize = 0x4000;

/// 已安裝的 spy hook 控制
pub struct SpyHandle {
    pub cave: u32,
    pub orig_bytes: [u8; RELOC_LEN],
    pub poll_cancel: Arc<AtomicBool>,
}

/// 檢查 N bytes 內是否含相對跳轉 — 這些抄到 codecave 執行會跳到錯誤位址。
///
/// 回傳 `Some(原因)` 表示不安全;`None` 表示可以 reloc。
fn check_relocation_safe(bytes: &[u8]) -> Option<String> {
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0xE8 | 0xE9 => {
                return Some(format!("byte {i} = 0x{b:02X} (call/jmp rel32)"));
            }
            0xEB => return Some(format!("byte {i} = 0xEB (jmp rel8)")),
            0xE0..=0xE3 => return Some(format!("byte {i} = 0x{b:02X} (loop/jecxz)")),
            0x70..=0x7F => return Some(format!("byte {i} = 0x{b:02X} (jcc rel8)")),
            0x0F => {
                // 0F 80..8F = jcc rel32(條件近跳)
                if i + 1 < bytes.len() && (0x80..=0x8F).contains(&bytes[i + 1]) {
                    return Some(format!("byte {i} = 0F {:02X} (jcc rel32)", bytes[i + 1]));
                }
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// 安裝 SendPacketData spy hook + spawn polling thread。
///
/// 回傳 [`SpyHandle`],拆 hook 用 [`uninstall_send_packet_spy`]。
pub fn install_send_packet_spy(h: HANDLE) -> Result<SpyHandle> {
    let target = SEND_PACKET_DATA;

    // 1. 備份原 RELOC_LEN bytes(8 bytes:涵蓋完整 prologue `push ebp; mov ebp,esp; mov eax, 0x140C`)
    let orig_vec = read_bytes(h, target, RELOC_LEN)?;
    let mut orig_bytes = [0u8; RELOC_LEN];
    orig_bytes.copy_from_slice(&orig_vec);
    let hex: String = orig_bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ");
    log_line!("[spy] SendPacketData 原 {RELOC_LEN} bytes: {hex}");

    // 2. 防呆:檢查 8 bytes 內是否含 relative jump/call(會 reloc 失敗 → game crash)
    if let Some(reason) = check_relocation_safe(&orig_bytes) {
        anyhow::bail!("SendPacketData 前 {RELOC_LEN} bytes 不可重新搬移: {reason}");
    }

    // 3. 分配 codecave
    let cave = alloc_exec(h, CAVE_SIZE)?;
    log_line!("[spy] codecave: 0x{cave:08X} (size={CAVE_SIZE:#x})");

    // 3. 初始化 codecave: log buffer 與 idx 全部歸零
    let zeros = vec![0u8; CAVE_SIZE];
    write_code(h, cave, &zeros)?;

    // 4. 組裝 shellcode 並寫入
    let shellcode = build_spy_shellcode(cave, &orig_bytes);
    let shellcode_addr = cave + SHELLCODE_OFF;
    write_code(h, shellcode_addr, &shellcode)?;

    // 5. 寫 5-byte JMP + (RELOC_LEN-5) NOP 填空(讓 fall-through 不會踩中段指令)
    let mut hook = [0x90u8; RELOC_LEN]; // 預設全 NOP
    hook[0] = 0xE9;
    let rel = shellcode_addr.wrapping_sub(target + 5) as i32;
    hook[1..5].copy_from_slice(&rel.to_le_bytes());
    write_code(h, target, &hook)?;
    log_line!(
        "[spy] SendPacketData @ 0x{target:08X} → JMP 0x{shellcode_addr:08X} (+{} NOP) 安裝完成",
        RELOC_LEN - 5
    );

    // 6. spawn polling thread,每 200ms 讀 ring buffer 印 log
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_thr = cancel.clone();
    let h_raw = h.0 as usize;
    std::thread::spawn(move || {
        let h = HANDLE(h_raw as *mut _);
        spy_poll_loop(h, cave, cancel_thr);
    });

    Ok(SpyHandle {
        cave,
        orig_bytes,
        poll_cancel: cancel,
    })
}

/// 組合 spy shellcode(完整 inline hook stub)
///
/// 最終配置:
/// ```text
/// pushad                       ; 60
/// pushfd                       ; 9C
/// ; 取 entry 寫入位址
/// mov  eax, ds:[cave + LOG_IDX_OFF]
/// and  eax, 0x3F               ; idx mod 64
/// shl  eax, 6                  ; * 64 = entry offset
/// add  eax, cave + LOG_BUF_OFF ; entry_ptr 進 eax
/// mov  edi, eax                ; edi = dst
/// ; 從 [esp+0x24] 開始有 7 個 dword(retn + 6 args),拷 28 bytes
/// lea  esi, [esp+0x24]
/// mov  ecx, 7
/// rep  movsd
/// ; 拷 [arg1] 前 32 bytes(arg1 = packet buf 位址,在 esp+0x28)
/// mov  esi, [esp+0x28]
/// test esi, esi
/// jz   skip_buf
/// mov  ecx, 8
/// rep  movsd
/// jmp  set_magic
/// skip_buf:
/// add  edi, 32
/// set_magic:
/// mov  dword [edi], 0xFEEDFACE
/// ; idx++(用 lock 確保多 thread 安全)
/// lock inc dword [cave + LOG_IDX_OFF]
/// popfd
/// popad
/// ; 執行原 5 bytes(prologue)
/// <orig_bytes>
/// ; jmp 0x580E55
/// jmp <SEND_PACKET_DATA + 5>
/// ```
fn build_spy_shellcode(cave: u32, orig: &[u8; RELOC_LEN]) -> Vec<u8> {
    let log_buf_addr = cave + LOG_BUF_OFF;
    let log_idx_addr = cave + LOG_IDX_OFF;
    let shellcode_addr = cave + SHELLCODE_OFF;
    let mut sc = Vec::with_capacity(128);

    // pushad
    sc.push(0x60);
    // pushfd
    sc.push(0x9C);

    // mov eax, ds:[log_idx]
    sc.push(0xA1);
    sc.extend_from_slice(&log_idx_addr.to_le_bytes());

    // and eax, 0x3F
    sc.extend_from_slice(&[0x83, 0xE0, 0x3F]);

    // shl eax, 7  (entry_offset = idx * 128)
    sc.extend_from_slice(&[0xC1, 0xE0, 0x07]);

    // add eax, log_buf_addr
    sc.extend_from_slice(&[0x05]);
    sc.extend_from_slice(&log_buf_addr.to_le_bytes());

    // mov edi, eax
    sc.extend_from_slice(&[0x89, 0xC7]);

    // lea esi, [esp+0x24]   (cdecl 6 args + retn,我們已 push 0x24=36 bytes(pushad 32 + pushfd 4))
    sc.extend_from_slice(&[0x8D, 0x74, 0x24, 0x24]);

    // mov ecx, 11
    sc.extend_from_slice(&[0xB9, 0x0B, 0x00, 0x00, 0x00]);

    // rep movsd  (拷 7 dword = 28 bytes:retn+6args)
    sc.extend_from_slice(&[0xF3, 0xA5]);

    // mov esi, [esp+0x28]   (arg1 值 = packet buf 位址)
    sc.extend_from_slice(&[0x8B, 0x74, 0x24, 0x28]);

    // 多重防呆:test null + 範圍檢查(必須在 0x00400000..0x10000000 才允許讀)
    // 任一失敗就跳 skip_buf,只填 32 byte 0,避免讀到無效位址 crash
    //
    // movsd_block:  cmp1(6B) + jb(2B) + cmp2(6B) + jae(2B) + mov ecx 8(5B) + rep movsd(2B) + jmp short(2B) = 25B
    // skip_buf_block: add edi,32(3B)

    // test esi, esi
    sc.extend_from_slice(&[0x85, 0xF6]);
    // jz +25 (skip_buf)
    sc.extend_from_slice(&[0x74, 0x19]);

    // cmp esi, 0x00400000
    sc.extend_from_slice(&[0x81, 0xFE, 0x00, 0x00, 0x40, 0x00]);
    // jb +17 (skip_buf)
    sc.extend_from_slice(&[0x72, 0x11]);

    // cmp esi, 0x10000000
    sc.extend_from_slice(&[0x81, 0xFE, 0x00, 0x00, 0x00, 0x10]);
    // jae +9 (skip_buf)
    sc.extend_from_slice(&[0x73, 0x09]);

    // (movsd_block) mov ecx, 8
    sc.extend_from_slice(&[0xB9, 0x08, 0x00, 0x00, 0x00]);
    // rep movsd  (拷 32 bytes 封包內容)
    sc.extend_from_slice(&[0xF3, 0xA5]);
    // jmp short +3 跳過 skip_buf_block
    sc.extend_from_slice(&[0xEB, 0x03]);

    // (skip_buf_block) add edi, 32
    sc.extend_from_slice(&[0x83, 0xC7, 0x20]);

    // Pad to entry trailer; magic lives at entry + 124.
    sc.extend_from_slice(&[0x83, 0xC7, 0x30]);

    // mov dword [edi], 0xFEEDFACE
    sc.extend_from_slice(&[0xC7, 0x07]);
    sc.extend_from_slice(&ENTRY_MAGIC.to_le_bytes());

    // lock inc dword [log_idx]
    sc.extend_from_slice(&[0xF0, 0xFF, 0x05]);
    sc.extend_from_slice(&log_idx_addr.to_le_bytes());

    // popfd / popad
    sc.push(0x9D);
    sc.push(0x61);

    // 原 RELOC_LEN bytes(8 bytes 完整 prologue)
    sc.extend_from_slice(orig);

    // jmp <SEND_PACKET_DATA + RELOC_LEN>(跳回原 prologue 結束之後的下一條指令)
    sc.push(0xE9);
    let cur_addr = shellcode_addr + sc.len() as u32 + 4;
    let target = SEND_PACKET_DATA + RELOC_LEN as u32;
    let rel = target.wrapping_sub(cur_addr) as i32;
    sc.extend_from_slice(&rel.to_le_bytes());

    sc
}

/// 拆 spy hook(還原 SendPacketData 原 8 bytes + 通知 polling thread 結束)。
#[allow(dead_code)]
pub fn uninstall_send_packet_spy(h: HANDLE, handle: &SpyHandle) -> Result<()> {
    handle.poll_cancel.store(true, Ordering::Relaxed);
    write_code(h, SEND_PACKET_DATA, &handle.orig_bytes)?;
    log_line!("[spy] SendPacketData hook 已拆除(還原 {} bytes)", RELOC_LEN);
    Ok(())
}

/// Polling thread 主迴圈 — 每 200ms 讀 log_idx 與新增的 entry,印到 log。
fn spy_poll_loop(h: HANDLE, cave: u32, cancel: Arc<AtomicBool>) {
    let log_idx_addr = cave + LOG_IDX_OFF;
    let log_buf_addr = cave + LOG_BUF_OFF;
    let mut last_idx: u32 = 0;

    log_line!("[spy] polling thread 啟動");

    while !cancel.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));

        let cur_idx = match read_u32(h, log_idx_addr) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if cur_idx == last_idx {
            continue;
        }

        // 從 last_idx ~ cur_idx 讀新 entry。最多回讀 RING_LEN 個避免 overflow
        let new_count = cur_idx.wrapping_sub(last_idx);
        let take = new_count.min(RING_LEN as u32);
        let start = cur_idx.wrapping_sub(take);

        for i in 0..take {
            let abs = start.wrapping_add(i);
            let slot = (abs as usize) % RING_LEN;
            let entry_addr = log_buf_addr + (slot * ENTRY_SIZE) as u32;
            if let Ok(buf) = read_bytes(h, entry_addr, ENTRY_SIZE) {
                let magic = u32::from_le_bytes([
                    buf[ENTRY_SIZE - 4],
                    buf[ENTRY_SIZE - 3],
                    buf[ENTRY_SIZE - 2],
                    buf[ENTRY_SIZE - 1],
                ]);
                if magic != ENTRY_MAGIC {
                    continue;
                }
                let ret_addr = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let arg1 = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                let arg2 = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
                let arg3 = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
                let arg4 = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
                let arg5 = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
                let arg6 = u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]);
                let arg7 = u32::from_le_bytes([buf[28], buf[29], buf[30], buf[31]]);
                let arg8 = u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]);
                let arg9 = u32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]);
                let arg10 = u32::from_le_bytes([buf[40], buf[41], buf[42], buf[43]]);
                let payload = &buf[44..76];
                let payload_hex: String = payload
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                log_line!(
                    "[spy] #{abs} ret=0x{ret_addr:08X} buf=0x{arg1:08X} op=0x{arg2:08X} a3={arg3}/0x{arg3:08X} a4={arg4}/0x{arg4:08X} a5={arg5}/0x{arg5:08X} a6={arg6}/0x{arg6:08X} a7={arg7}/0x{arg7:08X} a8={arg8}/0x{arg8:08X} a9={arg9}/0x{arg9:08X} a10={arg10}/0x{arg10:08X} | {payload_hex}"
                );
            }
        }

        last_idx = cur_idx;
    }

    log_line!("[spy] polling thread 結束");
}
