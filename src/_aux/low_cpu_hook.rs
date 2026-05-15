//! 降低 CPU — 直接 inline hook user32!PeekMessageA,失焦時注 Sleep(50)
//!
//! 取代失敗的 IAT redirect 策略。3.8 packer 把 game 的 IAT 加密成
//! daisy-chain JMP thunks,raw user32 位址不會出現在 game module 內,
//! 找不到 IAT 槽。改成直接 patch user32 內 PeekMessageA 開頭 5 bytes —
//! WriteProcessMemory 會觸發 COW,user32 那頁變成 game 私有副本,
//! launcher 自己的 user32 不受影響。
//!
//! Sleep 條件:
//!   - PeekMessageA retval == 0(無訊息 = 真 idle,訊息可用時不延遲)
//!   - GetForegroundWindow != game_hwnd(失焦 — 擺攤掛機)
//!   - 兩者皆成立才 sleep,避免每 PeekMessage 都 sleep 50ms 訊息積壓
//!
//! Codecave (78 bytes):
//!   detour:
//!     00: FF 74 24 14 ×5     ; 重 push 5 個 args (PeekMessageA __stdcall)
//!     14: E8 [trampoline]    ; call trampoline (= 真實 PeekMessageA)
//!     19: 85 C0              ; test eax, eax
//!     1B: 75 18              ; jnz .ret (有訊息 → 直接 ret 14h)
//!     1D: FF 15 [&fg]        ; call GetForegroundWindow
//!     23: 3B 05 [&hwnd]      ; cmp eax, [game_hwnd]
//!     29: 74 08              ; je .skip_sleep (有焦 → 不 sleep)
//!     2B: 6A 32              ; push 50
//!     2D: FF 15 [&sleep]     ; call Sleep
//!     33: 33 C0              ; .skip_sleep: xor eax, eax (確保 ret = 0)
//!     35: C2 14 00           ; .ret: ret 14h
//!   trampoline:
//!     38: <stolen 5 bytes>   ; PeekMessageA 原 prologue
//!     3D: E9 [peek+5]        ; jmp PeekMessageA+5
//!   data:
//!     42: dd GetForegroundWindow
//!     46: dd Sleep
//!     4A: dd game_hwnd

use crate::logger::log_line;
use crate::memory;
use anyhow::{anyhow, bail, Context, Result};
use std::sync::Mutex;
use windows::Win32::Foundation::HANDLE;

const SLEEP_MS_WHEN_UNFOCUSED: u8 = 50;
const CODECAVE_SIZE: u32 = 0x7A;
const TRAMPOLINE_OFFSET: u32 = 0x60;
const DATA_GET_FG: u32 = 0x6A;
const DATA_SLEEP: u32 = 0x6E;
const DATA_GAME_HWND: u32 = 0x72;
const DATA_GET_ASYNC_KEY_STATE: u32 = 0x76;
const PEEK_PROLOGUE_LEN: usize = 5;

struct HookState {
    codecave_addr: u32,
    peek_addr: u32,
    original_prologue: [u8; PEEK_PROLOGUE_LEN],
}

static STATE: Mutex<Option<HookState>> = Mutex::new(None);

pub fn is_installed() -> bool {
    STATE.lock().expect("low_cpu STATE poisoned").is_some()
}

/// 安裝 hook(idempotent)
pub fn install(h: HANDLE, pid: u32) -> Result<()> {
    let mut guard = STATE.lock().expect("low_cpu STATE poisoned");
    if guard.is_some() {
        return Ok(());
    }

    let game_hwnd = crate::img_hover::find_hwnd_by_pid(pid)
        .context("找不到遊戲視窗 HWND,無法安裝 low_cpu hook")? as u32;
    log_line!("[low_cpu] game_hwnd = 0x{:X}", game_hwnd);

    let (peek_addr, get_fg_addr, sleep_addr, get_async_key_state_addr) = get_apis()?;
    log_line!(
        "[low_cpu] PeekMessageA={:#X} GetForegroundWindow={:#X} Sleep={:#X} GetAsyncKeyState={:#X}",
        peek_addr,
        get_fg_addr,
        sleep_addr,
        get_async_key_state_addr
    );

    // 讀 PeekMessageA 開頭 5 bytes — 同時當 trampoline 用 + 卸載備份
    let prologue_vec = memory::read_bytes(h, peek_addr, PEEK_PROLOGUE_LEN)
        .context("讀 PeekMessageA prologue 失敗")?;
    if prologue_vec.len() != PEEK_PROLOGUE_LEN {
        bail!("讀 PeekMessageA prologue 長度錯誤");
    }
    let mut original_prologue = [0u8; PEEK_PROLOGUE_LEN];
    original_prologue.copy_from_slice(&prologue_vec);
    log_line!(
        "[low_cpu] PeekMessageA prologue = {:02X?}",
        original_prologue
    );

    // 防呆:已被 hook 過(E9/EB)的話再 patch 會破壞別人的 trampoline
    if matches!(original_prologue[0], 0xE9 | 0xEB) {
        bail!(
            "PeekMessageA prologue 開頭為 {:#X},疑似已被其他 hook patched,放棄安裝",
            original_prologue[0]
        );
    }

    // 配置 codecave
    let cave = memory::alloc_exec(h, CODECAVE_SIZE as usize)?;
    log_line!("[low_cpu] codecave @ 0x{:X}", cave);

    // 寫 codecave (detour + trampoline + data)
    let shellcode = build_shellcode(
        cave,
        peek_addr,
        get_fg_addr,
        sleep_addr,
        get_async_key_state_addr,
        game_hwnd,
        &original_prologue,
    );
    memory::write_code(h, cave, &shellcode)?;

    // patch PeekMessageA 開頭 5 bytes 為 JMP rel32 → cave (觸發 COW)
    let jmp_rel = (cave as i64 - (peek_addr as i64 + 5)) as i32;
    let mut jmp_bytes = [0u8; PEEK_PROLOGUE_LEN];
    jmp_bytes[0] = 0xE9;
    jmp_bytes[1..5].copy_from_slice(&jmp_rel.to_le_bytes());
    memory::write_code(h, peek_addr, &jmp_bytes).context("patch PeekMessageA prologue 失敗")?;

    *guard = Some(HookState {
        codecave_addr: cave,
        peek_addr,
        original_prologue,
    });

    log_line!("[low_cpu] 安裝完成");
    Ok(())
}

/// 卸載 hook(idempotent)
///
/// 還原 PeekMessageA prologue 後,codecave 不釋放 — 仍在 trampoline 內的
/// thread 還沒 ret 出來,釋放會 crash。重灌時直接 alloc 新塊。
pub fn uninstall(h: HANDLE) -> Result<()> {
    let mut guard = STATE.lock().expect("low_cpu STATE poisoned");
    let Some(state) = guard.take() else {
        return Ok(());
    };

    memory::write_code(h, state.peek_addr, &state.original_prologue)
        .context("還原 PeekMessageA prologue 失敗")?;

    log_line!(
        "[low_cpu] 卸載完成 (codecave 0x{:X} 保留)",
        state.codecave_addr
    );
    Ok(())
}

fn get_apis() -> Result<(u32, u32, u32, u32)> {
    use windows::core::s;
    use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

    unsafe {
        let user32 = GetModuleHandleA(s!("user32.dll"))?;
        let kernel32 = GetModuleHandleA(s!("kernel32.dll"))?;

        let peek = GetProcAddress(user32, s!("PeekMessageA"))
            .ok_or_else(|| anyhow!("GetProcAddress(PeekMessageA) 失敗"))?;
        let fg = GetProcAddress(user32, s!("GetForegroundWindow"))
            .ok_or_else(|| anyhow!("GetProcAddress(GetForegroundWindow) 失敗"))?;
        let sleep = GetProcAddress(kernel32, s!("Sleep"))
            .ok_or_else(|| anyhow!("GetProcAddress(Sleep) 失敗"))?;

        let get_async_key_state = GetProcAddress(user32, s!("GetAsyncKeyState"))
            .ok_or_else(|| anyhow!("GetProcAddress(GetAsyncKeyState) failed"))?;

        Ok((
            peek as u32,
            fg as u32,
            sleep as u32,
            get_async_key_state as u32,
        ))
    }
}

fn build_shellcode(
    cave: u32,
    peek: u32,
    get_fg: u32,
    sleep: u32,
    get_async_key_state: u32,
    game_hwnd: u32,
    prologue: &[u8; PEEK_PROLOGUE_LEN],
) -> Vec<u8> {
    let mut sc = Vec::<u8>::with_capacity(CODECAVE_SIZE as usize);

    let data_fg = cave + DATA_GET_FG;
    let data_sleep = cave + DATA_SLEEP;
    let data_hwnd = cave + DATA_GAME_HWND;
    let data_get_async_key_state = cave + DATA_GET_ASYNC_KEY_STATE;
    let trampoline = cave + TRAMPOLINE_OFFSET;

    // detour: 5 × push [esp+0x14] — 把 caller 的 5 個 args 重 push 上去
    // 注意:每次 push 後 esp -= 4,但 [esp+0x14] 的位移剛好補回原 args 的位置
    //   進來時 [esp+04..18] = args
    //   push1 後 [esp+18..2C]=args,但 [esp+0x14] 取的是 arg5(原 [esp+18]) ✓
    //   push2 後 [esp+0x14] 取的是 arg4(原 [esp+14]) ✓ ...以此類推
    for _ in 0..5 {
        sc.extend_from_slice(&[0xFF, 0x74, 0x24, 0x14]);
    }
    debug_assert_eq!(sc.len(), 0x14);

    // call trampoline (rel32)
    let call_rel = (trampoline as i64 - (cave as i64 + sc.len() as i64 + 5)) as i32;
    sc.push(0xE8);
    sc.extend_from_slice(&call_rel.to_le_bytes());
    debug_assert_eq!(sc.len(), 0x19);

    // test eax, eax
    sc.extend_from_slice(&[0x85, 0xC0]);
    // jnz +0x18 → 跳到 ret 14h(0x35)
    let jnz_message_available = push_rel8_jump(&mut sc, 0x75);
    debug_assert_eq!(sc.len(), 0x1D);

    // call dword ptr [&get_fg]
    sc.extend_from_slice(&[0xFF, 0x15]);
    sc.extend_from_slice(&data_fg.to_le_bytes());
    debug_assert_eq!(sc.len(), 0x23);

    // cmp eax, dword ptr [&game_hwnd]
    sc.extend_from_slice(&[0x3B, 0x05]);
    sc.extend_from_slice(&data_hwnd.to_le_bytes());
    debug_assert_eq!(sc.len(), 0x29);

    // je +8 → 跳到 .skip_sleep(0x33)
    let je_game_foreground = push_rel8_jump(&mut sc, 0x74);
    debug_assert_eq!(sc.len(), 0x2B);

    // Any held key can keep the client in an active input loop. Scan virtual
    // keys before throttling so Ctrl/Shift/F-keys and other holds do not freeze.
    sc.push(0x53); // push ebx
    sc.extend_from_slice(&[0x31, 0xDB]); // xor ebx, ebx
    sc.push(0x43); // inc ebx => vkey = 1
    let scan_loop = sc.len();
    sc.push(0x53); // push ebx
    sc.extend_from_slice(&[0xFF, 0x15]);
    sc.extend_from_slice(&data_get_async_key_state.to_le_bytes());
    sc.extend_from_slice(&[0x66, 0xA9, 0x00, 0x80]); // test ax, 0x8000
    let jnz_any_key_down = push_rel8_jump(&mut sc, 0x75);
    sc.push(0x43); // inc ebx
    sc.extend_from_slice(&[0x81, 0xFB, 0xFF, 0x00, 0x00, 0x00]); // cmp ebx, 0xFF
    let jl_scan_loop = push_rel8_jump(&mut sc, 0x7C);

    sc.push(0x5B); // pop ebx
                   // push 50
    sc.extend_from_slice(&[0x6A, SLEEP_MS_WHEN_UNFOCUSED]);
    // call dword ptr [&sleep]
    sc.extend_from_slice(&[0xFF, 0x15]);
    sc.extend_from_slice(&data_sleep.to_le_bytes());
    let jmp_after_sleep = push_rel8_jump(&mut sc, 0xEB);

    let any_key_down = sc.len();
    sc.push(0x5B); // pop ebx

    let skip_sleep = sc.len();
    sc.extend_from_slice(&[0x33, 0xC0]);
    let ret = sc.len();
    sc.extend_from_slice(&[0xC2, 0x14, 0x00]);

    patch_rel8(&mut sc, jnz_message_available, ret);
    patch_rel8(&mut sc, je_game_foreground, skip_sleep);
    patch_rel8(&mut sc, jnz_any_key_down, any_key_down);
    patch_rel8(&mut sc, jl_scan_loop, scan_loop);
    patch_rel8(&mut sc, jmp_after_sleep, skip_sleep);

    while sc.len() < TRAMPOLINE_OFFSET as usize {
        sc.push(0x90);
    }
    debug_assert_eq!(sc.len() as u32, TRAMPOLINE_OFFSET);

    // trampoline: stolen 5 bytes + jmp peek+5
    sc.extend_from_slice(prologue);
    let jmp_rel = (peek as i64 + 5 - (cave as i64 + sc.len() as i64 + 5)) as i32;
    sc.push(0xE9);
    sc.extend_from_slice(&jmp_rel.to_le_bytes());
    debug_assert_eq!(sc.len() as u32, DATA_GET_FG);

    // data dwords
    sc.extend_from_slice(&get_fg.to_le_bytes());
    sc.extend_from_slice(&sleep.to_le_bytes());
    sc.extend_from_slice(&game_hwnd.to_le_bytes());
    sc.extend_from_slice(&get_async_key_state.to_le_bytes());
    debug_assert_eq!(sc.len() as u32, CODECAVE_SIZE);

    sc
}

fn push_rel8_jump(sc: &mut Vec<u8>, opcode: u8) -> usize {
    sc.push(opcode);
    let rel_pos = sc.len();
    sc.push(0);
    rel_pos
}

fn patch_rel8(sc: &mut [u8], rel_pos: usize, target: usize) {
    let next_ip = rel_pos + 1;
    let rel = target as isize - next_ip as isize;
    assert!(
        (-128..=127).contains(&rel),
        "rel8 jump target out of range: rel={rel}"
    );
    sc[rel_pos] = rel as i8 as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROLOGUE: [u8; 5] = [0x8B, 0xFF, 0x55, 0x8B, 0xEC];

    #[test]
    fn shellcode_size_correct() {
        let sc = build_shellcode(
            0x1000_0000,
            0x7510_0000,
            0x7520_0000,
            0x7530_0000,
            0x7540_0000,
            0xABCD_EF01,
            &PROLOGUE,
        );
        assert_eq!(sc.len() as u32, CODECAVE_SIZE);
    }

    #[test]
    fn five_pushes_at_start() {
        let sc = build_shellcode(
            0x1000_0000,
            0x7510_0000,
            0x7520_0000,
            0x7530_0000,
            0x7540_0000,
            0xABCD_EF01,
            &PROLOGUE,
        );
        for i in 0..5 {
            assert_eq!(&sc[i * 4..i * 4 + 4], &[0xFF, 0x74, 0x24, 0x14]);
        }
    }

    #[test]
    fn call_trampoline_rel32_correct() {
        let cave = 0x1000_0000u32;
        let sc = build_shellcode(
            cave,
            0x7510_0000,
            0x7520_0000,
            0x7530_0000,
            0x7540_0000,
            0xABCD_EF01,
            &PROLOGUE,
        );
        assert_eq!(sc[0x14], 0xE8);
        let rel = i32::from_le_bytes(sc[0x15..0x19].try_into().unwrap());
        let next_ip = cave + 0x19;
        let target = (next_ip as i64 + rel as i64) as u32;
        assert_eq!(target, cave + TRAMPOLINE_OFFSET);
    }

    #[test]
    fn jnz_skips_to_ret() {
        let sc = build_shellcode(
            0x1000_0000,
            0x7510_0000,
            0x7520_0000,
            0x7530_0000,
            0x7540_0000,
            0xABCD_EF01,
            &PROLOGUE,
        );
        assert_eq!(sc[0x1B], 0x75);
        let target = (0x1D_i16 + sc[0x1C] as i8 as i16) as usize;
        // 跳到 0x1D + 0x18 = 0x35 = ret 14h 起點
        assert_eq!(&sc[target..target + 3], &[0xC2, 0x14, 0x00]);
    }

    #[test]
    fn je_skips_to_xor_eax() {
        let sc = build_shellcode(
            0x1000_0000,
            0x7510_0000,
            0x7520_0000,
            0x7530_0000,
            0x7540_0000,
            0xABCD_EF01,
            &PROLOGUE,
        );
        assert_eq!(sc[0x29], 0x74);
        let target = (0x2B_i16 + sc[0x2A] as i8 as i16) as usize;
        // 跳到 0x2B + 0x08 = 0x33 = xor eax, eax
        assert_eq!(&sc[target..target + 2], &[0x33, 0xC0]);
    }

    #[test]
    fn trampoline_stolen_then_jmp() {
        let cave = 0x1000_0000u32;
        let peek = 0x7510_0000u32;
        let sc = build_shellcode(
            cave,
            peek,
            0x7520_0000,
            0x7530_0000,
            0x7540_0000,
            0xABCD_EF01,
            &PROLOGUE,
        );
        let off = TRAMPOLINE_OFFSET as usize;
        assert_eq!(&sc[off..off + 5], &PROLOGUE);
        assert_eq!(sc[off + 5], 0xE9);
        let rel = i32::from_le_bytes(sc[off + 6..off + 10].try_into().unwrap());
        let next_ip = cave + off as u32 + 10;
        let target = (next_ip as i64 + rel as i64) as u32;
        assert_eq!(target, peek + 5);
    }

    #[test]
    fn data_section_correct() {
        let sc = build_shellcode(
            0x1000_0000,
            0x7510_0000,
            0x75AB_CDEF,
            0x75DD_EEFF,
            0x75EE_FF00,
            0xABCD_EF01,
            &PROLOGUE,
        );
        assert_eq!(
            u32::from_le_bytes(
                sc[DATA_GET_FG as usize..DATA_GET_FG as usize + 4]
                    .try_into()
                    .unwrap()
            ),
            0x75AB_CDEF
        );
        assert_eq!(
            u32::from_le_bytes(
                sc[DATA_SLEEP as usize..DATA_SLEEP as usize + 4]
                    .try_into()
                    .unwrap()
            ),
            0x75DD_EEFF
        );
        assert_eq!(
            u32::from_le_bytes(
                sc[DATA_GAME_HWND as usize..DATA_GAME_HWND as usize + 4]
                    .try_into()
                    .unwrap()
            ),
            0xABCD_EF01
        );
        assert_eq!(
            u32::from_le_bytes(
                sc[DATA_GET_ASYNC_KEY_STATE as usize..DATA_GET_ASYNC_KEY_STATE as usize + 4]
                    .try_into()
                    .unwrap()
            ),
            0x75EE_FF00
        );
    }

    #[test]
    fn shellcode_scans_all_virtual_keys_before_sleeping() {
        let sc = build_shellcode(
            0x1000_0000,
            0x7510_0000,
            0x7520_0000,
            0x7530_0000,
            0x7540_0000,
            0xABCD_EF01,
            &PROLOGUE,
        );

        assert!(
            sc.windows(4).any(|w| w == [0x53, 0x31, 0xDB, 0x43]),
            "expected shellcode to preserve EBX and start a virtual-key scan loop"
        );
        assert!(
            sc.windows(6)
                .any(|w| w == [0x81, 0xFB, 0xFF, 0x00, 0x00, 0x00]),
            "expected shellcode to scan through virtual key 0xFE before sleeping"
        );
    }
}
