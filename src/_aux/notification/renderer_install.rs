//! Renderer install — piggyback img_hover's per-frame cave。
//!
//! # 架構
//!
//! img_hover 已 hook `0x491174` 的 `call <draw_fn>`,讓 cave 跑 hover 工作後 tail-call
//! 原 draw_fn `0x00759BF0`。 cave 內部有 3 個 exit 都是 `jmp 0x00759BF0`。
//!
//! 我們**不改 img_hover cave 的邏輯**,只把那 3 個 exit 的 disp32 改向我們的 wrapper:
//!
//! ```text
//! img_hover_cave:
//!   ... existing work ...
//!   jmp WRAPPER_CAVE          ← 改這裡的 disp32
//!
//! WRAPPER_CAVE:
//!   call NOTIF_DRAW_CAVE       ; 我們的 draw loop
//!   jmp 0x00759BF0             ; 原 draw_fn (img_hover 原本要 tail-call 的目標)
//!
//! NOTIF_DRAW_CAVE:
//!   build_draw_loop_shellcode(LIST_CAVE)
//!
//! LIST_CAVE:
//!   [count: u32 LE][DrawCmd × N]  ← polling thread 寫
//! ```
//!
//! ABI:NOTIF_DRAW_CAVE 用 pushad/pushfd 保 game 全 regs,popad/popfd/ret 還原。
//! 0x759BF0 是 thiscall(ECX = this),不受影響。

use anyhow::{bail, Context, Result};
use windows::Win32::Foundation::HANDLE;

use crate::logger::log_line;
use crate::memory;
use crate::process;

use super::renderer;

/// img_hover 的 hook 點(`call <draw_fn>`)。
const IMG_HOVER_HOOK_SITE: u32 = 0x00491174;
/// img_hover 分配的 cave 大小(從 img_hover.rs:136 的 `cave_size = 0x200`)。
const IMG_HOVER_CAVE_SIZE: usize = 0x200;
/// img_hover cave 各 exit 都 tail-call 到這(原 draw_fn)。
const ORIGINAL_DRAW_FN: u32 = 0x00759BF0;

/// LIST_CAVE 大小:容納 ~85 個 DrawCmd 已綽綽有餘
/// (10 toast × 2 + 數十個 float)。
pub const LIST_CAVE_SIZE: usize = 0x400;
/// LIST_CAVE 內 [count:u32][DrawCmd × N] — 計算上限
pub const MAX_DRAW_CMDS: usize = (LIST_CAVE_SIZE - 4) / 12;

// NOTIF_DRAW_CAVE 內 diag counters 區(放在 shellcode 之後)
pub const OFF_DIAG_INVOCATIONS: u32 = 0x80;
pub const OFF_DIAG_LAST_COUNT: u32 = 0x84;
pub const OFF_DIAG_GSR_NULL: u32 = 0x88;
pub const OFF_DIAG_BLITS: u32 = 0x8C;

pub struct RendererHandle {
    pub list_cave: u32,
    pub notif_draw_cave: u32,
    pub wrapper_cave: u32,
    pub img_hover_cave: u32,
    /// 被改過 disp32 的 exit 位置(uninstall 用,目前只 log)。
    pub patched_exits: Vec<u32>,
}

pub fn install(h: HANDLE, pid: u32) -> Result<RendererHandle> {
    // 1) 讀 img_hover 的 call → 取得 cave addr
    let hook_bytes = memory::read_bytes(h, IMG_HOVER_HOOK_SITE, 5)
        .context("讀 img_hover hook site 失敗")?;
    if hook_bytes[0] != 0xE8 {
        bail!(
            "img_hover hook 未安裝(0x{IMG_HOVER_HOOK_SITE:08X} 不是 E8 call,first byte={:02X})",
            hook_bytes[0]
        );
    }
    let disp = i32::from_le_bytes([
        hook_bytes[1],
        hook_bytes[2],
        hook_bytes[3],
        hook_bytes[4],
    ]);
    let img_hover_cave = ((IMG_HOVER_HOOK_SITE + 5) as i64 + disp as i64) as u32;

    // 2) 掃 cave 內的 `E9 disp32` exit,挑 target = ORIGINAL_DRAW_FN 的
    let cave_bytes = memory::read_bytes(h, img_hover_cave, IMG_HOVER_CAVE_SIZE)
        .context("讀 img_hover cave 失敗")?;
    let mut exit_offsets = Vec::new();
    let mut i = 0;
    while i + 5 <= cave_bytes.len() {
        if cave_bytes[i] == 0xE9 {
            let d = i32::from_le_bytes([
                cave_bytes[i + 1],
                cave_bytes[i + 2],
                cave_bytes[i + 3],
                cave_bytes[i + 4],
            ]);
            let next_ip = img_hover_cave + (i as u32) + 5;
            let target = ((next_ip as i64) + d as i64) as u32;
            if target == ORIGINAL_DRAW_FN {
                exit_offsets.push(i);
                i += 5;
                continue;
            }
        }
        i += 1;
    }
    if exit_offsets.is_empty() {
        bail!(
            "img_hover cave @ 0x{img_hover_cave:08X} 找不到 jmp 0x{ORIGINAL_DRAW_FN:08X} \
             — img_hover 可能版本不符,放棄裝 renderer"
        );
    }

    // 3) 配三個 cave
    let list_cave = memory::alloc_exec(h, LIST_CAVE_SIZE)
        .context("alloc LIST_CAVE 失敗")?;
    memory::write_code(h, list_cave, &vec![0u8; LIST_CAVE_SIZE])
        .context("zero LIST_CAVE 失敗")?;

    let notif_draw_cave = memory::alloc_exec(h, 256)
        .context("alloc NOTIF_DRAW_CAVE 失敗")?;
    // 把整個 cave 先歸零(counter 區從 0 開始)
    memory::write_code(h, notif_draw_cave, &vec![0u8; 256])
        .context("zero NOTIF_DRAW_CAVE 失敗")?;
    let notif_draw_sc = build_draw_loop_with_diag(list_cave, notif_draw_cave);
    if notif_draw_sc.len() > OFF_DIAG_INVOCATIONS as usize {
        bail!(
            "notif_draw shellcode {} bytes 超過 0x{:X} counter 區",
            notif_draw_sc.len(),
            OFF_DIAG_INVOCATIONS
        );
    }
    memory::write_code(h, notif_draw_cave, &notif_draw_sc)
        .context("寫 notif_draw shellcode 失敗")?;

    let wrapper_cave = memory::alloc_exec(h, 16)
        .context("alloc WRAPPER_CAVE 失敗")?;
    let wrapper_bytes = build_wrapper(wrapper_cave, notif_draw_cave);
    memory::write_code(h, wrapper_cave, &wrapper_bytes)
        .context("寫 WRAPPER_CAVE 失敗")?;

    // 4) Patch img_hover exits — suspend threads 以免 race
    let threads = process::suspend_threads(pid)?;
    let mut patched = Vec::new();
    let mut patch_err: Option<anyhow::Error> = None;
    for &off in &exit_offsets {
        let exit_addr = img_hover_cave + off as u32;
        let new_disp = (wrapper_cave as i64) - ((exit_addr + 5) as i64);
        let new_disp = new_disp as i32;
        match memory::write_code(h, exit_addr + 1, &new_disp.to_le_bytes()) {
            Ok(()) => patched.push(exit_addr),
            Err(e) => {
                patch_err = Some(anyhow::anyhow!(e));
                break;
            }
        }
    }
    process::resume_threads(threads);
    if let Some(e) = patch_err {
        bail!("patch img_hover exit 失敗: {e:#}");
    }

    log_line!(
        "[OK] notification renderer @ list=0x{list_cave:08X} draw=0x{notif_draw_cave:08X} \
         wrapper=0x{wrapper_cave:08X} ({} exits patched in img_hover_cave 0x{img_hover_cave:08X})",
        patched.len()
    );

    Ok(RendererHandle {
        list_cave,
        notif_draw_cave,
        wrapper_cave,
        img_hover_cave,
        patched_exits: patched,
    })
}

/// 在 renderer::build_draw_loop_shellcode 前面塞 diag 區塊。
///
/// Diag 寫入 `cave_addr + OFF_DIAG_INVOCATIONS` 等 counter,純記憶體 op
/// 不動 register,可以在 pushad/pushfd 前安全執行。
fn build_draw_loop_with_diag(list_cave: u32, cave_addr: u32) -> Vec<u8> {
    let mut sc = Vec::with_capacity(160);

    // lock inc dword [invocations]  (7 bytes)
    sc.extend_from_slice(&[0xF0, 0xFF, 0x05]);
    sc.extend_from_slice(&(cave_addr + OFF_DIAG_INVOCATIONS).to_le_bytes());

    // 把 [list_cave]  (= count) 拷到 [last_count] —
    // 用 push/pop eax 避免 trashing register
    // push eax  (1)
    sc.push(0x50);
    // mov eax, [list_cave]  (A1 + addr, 5)
    sc.push(0xA1);
    sc.extend_from_slice(&list_cave.to_le_bytes());
    // mov [last_count], eax  (A3 + addr, 5)
    sc.push(0xA3);
    sc.extend_from_slice(&(cave_addr + OFF_DIAG_LAST_COUNT).to_le_bytes());
    // pop eax  (1)
    sc.push(0x58);

    // 接著原 draw loop(整段 pushad/pushfd... popfd/popad/ret)
    sc.extend_from_slice(&renderer::build_draw_loop_shellcode(list_cave));
    sc
}

/// 讀 NOTIF_DRAW_CAVE 的 4 個 diag counter。
pub fn read_diag(h: HANDLE) -> Option<(u32, u32, u32, u32)> {
    let cave = crate::aux::notification::renderer_cave_addr()?;
    let invocations = read_u32(h, cave + OFF_DIAG_INVOCATIONS)?;
    let last_count = read_u32(h, cave + OFF_DIAG_LAST_COUNT)?;
    let gsr_null = read_u32(h, cave + OFF_DIAG_GSR_NULL)?;
    let blits = read_u32(h, cave + OFF_DIAG_BLITS)?;
    Some((invocations, last_count, gsr_null, blits))
}

fn read_u32(h: HANDLE, addr: u32) -> Option<u32> {
    let b = memory::read_bytes(h, addr, 4).ok()?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// `call NOTIF_DRAW_CAVE; jmp ORIGINAL_DRAW_FN` — 10 bytes,position-aware。
fn build_wrapper(wrapper_addr: u32, notif_draw_cave: u32) -> Vec<u8> {
    let mut sc = Vec::with_capacity(10);
    // call notif_draw_cave  (E8 + disp32, 5 bytes)
    sc.push(0xE8);
    let call_disp = (notif_draw_cave as i64) - ((wrapper_addr + 5) as i64);
    sc.extend_from_slice(&(call_disp as i32).to_le_bytes());
    // jmp ORIGINAL_DRAW_FN  (E9 + disp32, 5 bytes)
    sc.push(0xE9);
    let jmp_disp = (ORIGINAL_DRAW_FN as i64) - ((wrapper_addr + 10) as i64);
    sc.extend_from_slice(&(jmp_disp as i32).to_le_bytes());
    sc
}

/// 把序列化好的 render list 寫到 LIST_CAVE。
/// 先把 count 寫 0(原子,避免 game 讀到舊 count 配新 entries),
/// 再寫 entries,最後 atomically 更新 count。
pub fn update_list(h: HANDLE, list_cave: u32, serialized: &[u8]) -> Result<()> {
    if serialized.len() < 4 {
        return Ok(());
    }
    if serialized.len() > LIST_CAVE_SIZE {
        log_line!(
            "[notification] render list {} bytes > LIST_CAVE {} — 截斷",
            serialized.len(),
            LIST_CAVE_SIZE
        );
    }
    let usable_len = serialized.len().min(LIST_CAVE_SIZE);

    // 1) 暫時把 count 寫 0
    memory::write_code(h, list_cave, &0u32.to_le_bytes())
        .context("zero LIST_CAVE count 失敗")?;
    // 2) 寫 entries(從 +4 開始)
    if usable_len > 4 {
        memory::write_code(h, list_cave + 4, &serialized[4..usable_len])
            .context("寫 LIST_CAVE entries 失敗")?;
    }
    // 3) 最後寫 final count
    memory::write_code(h, list_cave, &serialized[0..4])
        .context("寫 LIST_CAVE count 失敗")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_bytes_call_then_jmp() {
        // wrapper at 0x10000000, notif at 0x20000000, target at 0x759BF0
        let w = build_wrapper(0x10000000, 0x20000000);
        assert_eq!(w.len(), 10);
        assert_eq!(w[0], 0xE8); // call
        let call_disp = i32::from_le_bytes([w[1], w[2], w[3], w[4]]);
        assert_eq!(call_disp, 0x20000000_i32 - 0x10000005);
        assert_eq!(w[5], 0xE9); // jmp
        let jmp_disp = i32::from_le_bytes([w[6], w[7], w[8], w[9]]);
        assert_eq!(jmp_disp, ORIGINAL_DRAW_FN as i32 - 0x1000000A);
    }

    #[test]
    fn wrapper_call_lands_on_notif_draw() {
        let w = build_wrapper(0x10000000, 0x20000000);
        let call_disp = i32::from_le_bytes([w[1], w[2], w[3], w[4]]);
        let target = 0x10000005_i64 + call_disp as i64;
        assert_eq!(target as u32, 0x20000000);
    }

    #[test]
    fn wrapper_jmp_lands_on_original_draw_fn() {
        let w = build_wrapper(0x10000000, 0x20000000);
        let jmp_disp = i32::from_le_bytes([w[6], w[7], w[8], w[9]]);
        let target = 0x1000000A_i64 + jmp_disp as i64;
        assert_eq!(target as u32, ORIGINAL_DRAW_FN);
    }

    #[test]
    fn max_draw_cmds_fits_list_cave() {
        // count(4) + MAX_DRAW_CMDS * 12 ≤ LIST_CAVE_SIZE
        assert!(4 + MAX_DRAW_CMDS * 12 <= LIST_CAVE_SIZE);
    }
}
