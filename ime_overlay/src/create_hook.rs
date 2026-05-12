//! Inline hook `user32!CreateWindowExA` / `CreateWindowExW`,在 LUnicodeEdit
//! 建立**當下**就把 `WS_EX_COMPOSITED` 塞進 `dwExStyle`。
//!
//! 為什麼要這樣做(而非建好之後 SetWindowLong):
//!   `WS_EX_COMPOSITED` 必須在 CreateWindow 階段就讓 EDIT 知道,EDIT 內部
//!   wndproc 才會走「DWM-aware」路徑(IME 訊息 forward 給 DefWindowProc、
//!   雙緩衝合成)。後天用 SetWindowLong + SWP_FRAMECHANGED 補只會改 ex-style
//!   外觀位元,EDIT 已經初始化過、行為不會變。
//!
//! 這個做法是某 Lineage 民間 launcher 已驗證的解法 — 同時解打字閃爍 +
//! Win11 IMM32 候選字視窗(我們可以丟掉自繪 overlay)。
//!
//! ## 安裝細節
//!
//! Win10/11 32-bit 的 `user32!CreateWindowExA` / `W` 通常以「hot-patch
//! prologue」開頭(`8B FF 55 8B EC` = mov edi,edi; push ebp; mov ebp,esp,
//! 共 5 bytes),剛好給我們塞 5-byte JMP rel32。
//!
//! 我們做:
//!   1. 配置 32 bytes RWX 給 trampoline(原 5 bytes + JMP 回 target+5)
//!   2. 寫 5-byte JMP rel32 從 target → 我們的 handler
//!
//! 開頭如果不是預期的 5 bytes(被別人 hook 或 prologue 形式改了)就 bail。

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use windows::core::PCSTR;
use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND};
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleA, GetModuleHandleExW, GetProcAddress, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::Memory::{
    VirtualAlloc, VirtualProtect, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE,
    PAGE_PROTECTION_FLAGS,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::Ime::ImmDisableTextFrameService;
use windows::Win32::UI::WindowsAndMessaging::{HMENU, WNDCLASSEXA, WNDCLASSEXW, WNDCLASS_STYLES};

use crate::dbg_log;

/// 是否已嘗試 disable TSF — 只試一次,在第一個 hooked CreateWindowEx 觸發時。
/// 為什麼放在 hook 裡:`ImmDisableTextFrameService` 是 thread-local,**必須**在
/// 遊戲 UI thread 上呼叫才有意義。我們的 worker thread 跟遊戲 UI thread 不同,
/// 直接在 worker 呼叫只關掉 worker thread 的 TSF(沒意義)。
/// CreateWindowEx hook 是被遊戲呼叫,所以一定在遊戲 UI thread。
static TSF_DISABLE_TRIED: AtomicBool = AtomicBool::new(false);

/// 在遊戲 UI thread 第一次呼叫到我們 hook 時,試著把 TSF 關掉,強迫 IMM32 fallback。
///
/// **caveat**:`ImmDisableTextFrameService` 文件規定**必須**在 thread 還沒 init
/// TSF 之前呼叫,init 後呼叫無效。我們 inject 時間點通常在 init 之後,所以**可能無效**。
/// 但呼叫便宜,先試一次,看 log 結果再決定下一步。
unsafe fn try_disable_tsf_once() {
    if TSF_DISABLE_TRIED.swap(true, Ordering::SeqCst) {
        return;
    }
    let tid = GetCurrentThreadId();
    let r1 = ImmDisableTextFrameService(tid);
    // -1 = process-wide(影響所有 thread)
    let r2 = ImmDisableTextFrameService(0xFFFF_FFFF);
    dbg_log!(
        "[ime] ImmDisableTextFrameService(tid={tid}) → {} | (-1 process-wide) → {}",
        r1.as_bool(),
        r2.as_bool()
    );
}

const CS_OWNDC: u32 = 0x0020;
const CS_CLASSDC: u32 = 0x0040;
const TARGET_CLASS: &str = "LUnicodeEdit";

fn patched_lunicodeedit_ex_style(dw_ex_style: u32) -> u32 {
    dw_ex_style
}

fn patched_lunicodeedit_class_style(style: u32) -> u32 {
    style
}

/// 預期的 hot-patch prologue:`mov edi, edi; push ebp; mov ebp, esp`
const EXPECTED_PROLOGUE: [u8; 5] = [0x8B, 0xFF, 0x55, 0x8B, 0xEC];

type CreateWindowExAFn = unsafe extern "system" fn(
    dw_ex_style: u32,
    lp_class_name: *const u8,
    lp_window_name: *const u8,
    dw_style: u32,
    x: i32,
    y: i32,
    n_width: i32,
    n_height: i32,
    h_wnd_parent: HWND,
    h_menu: HMENU,
    h_instance: HINSTANCE,
    lp_param: *const c_void,
) -> HWND;

type CreateWindowExWFn = unsafe extern "system" fn(
    dw_ex_style: u32,
    lp_class_name: *const u16,
    lp_window_name: *const u16,
    dw_style: u32,
    x: i32,
    y: i32,
    n_width: i32,
    n_height: i32,
    h_wnd_parent: HWND,
    h_menu: HMENU,
    h_instance: HINSTANCE,
    lp_param: *const c_void,
) -> HWND;

type RegisterClassExAFn = unsafe extern "system" fn(*const WNDCLASSEXA) -> u16;
type RegisterClassExWFn = unsafe extern "system" fn(*const WNDCLASSEXW) -> u16;
type GetClassNameAFn = unsafe extern "system" fn(HWND, *mut u8, i32) -> i32;
type GetClassNameWFn = unsafe extern "system" fn(HWND, *mut u16, i32) -> i32;

static TRAMPOLINE_GCN_A: AtomicUsize = AtomicUsize::new(0);
static TRAMPOLINE_GCN_W: AtomicUsize = AtomicUsize::new(0);
static GCN_LIE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// 已經查過 caller 模組的 cache:return-addr 對齊到頁(0x1000)→ 是否該說謊
/// 同 page 多次呼叫直接命中 cache,省掉 GetModuleHandleEx + 字串比對
static CALLER_LIE_CACHE: Mutex<Option<std::collections::HashMap<usize, bool>>> = Mutex::new(None);

/// 判斷 return address 是否屬於要對它說謊的 IME 模組(msctf / imm32 / ctfime / *.ime)
unsafe fn caller_should_be_lied_to(ret_addr: usize) -> bool {
    let page = ret_addr & !0xFFF;
    {
        let mut g = CALLER_LIE_CACHE.lock().unwrap();
        if g.is_none() {
            *g = Some(std::collections::HashMap::new());
        }
        if let Some(cached) = g.as_ref().unwrap().get(&page) {
            return *cached;
        }
    }

    // 拉這個位址所屬的 module name(取小寫 base name 比對)
    let mut hmod = HMODULE::default();
    let ok = GetModuleHandleExW(
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        windows::core::PCWSTR(ret_addr as *const u16),
        &mut hmod,
    )
    .is_ok();
    let mut should_lie = false;
    if ok {
        let mut buf = [0u16; 260];
        let len = windows::Win32::System::LibraryLoader::GetModuleFileNameW(Some(hmod), &mut buf);
        if len > 0 {
            let path = String::from_utf16_lossy(&buf[..len as usize]).to_ascii_lowercase();
            // 只挑 file name(取最後一個 \ 之後)
            let name = path.rsplit('\\').next().unwrap_or(&path);
            should_lie = name.contains("msctf")
                || name.contains("imm32")
                || name.contains("ctfime")
                || name.ends_with(".ime")
                || name.contains("textinput")
                || name.contains("input.dll");
            // 只 log 第一次「值得說謊」的呼叫者,避免 log 爆量
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if should_lie && !LOGGED.swap(true, Ordering::Relaxed) {
                dbg_log!("[ime] GetClassName lie target module: '{}'", name);
            }
        }
    }
    {
        let mut g = CALLER_LIE_CACHE.lock().unwrap();
        g.as_mut().unwrap().insert(page, should_lie);
    }
    should_lie
}

/// 把 "Edit" 寫到 wide buffer。回傳實際寫入字元數(不含 NUL)
unsafe fn write_edit_lie_w(buf: *mut u16, max: i32) -> i32 {
    if buf.is_null() || max <= 0 {
        return 0;
    }
    let lie: [u16; 5] = [b'E' as u16, b'd' as u16, b'i' as u16, b't' as u16, 0];
    let cap = max as usize;
    let n = cap.min(lie.len());
    ptr::copy_nonoverlapping(lie.as_ptr(), buf, n);
    if n > 0 {
        *buf.add(n - 1) = 0;
    }
    (n.saturating_sub(1)) as i32
}

unsafe fn write_edit_lie_a(buf: *mut u8, max: i32) -> i32 {
    if buf.is_null() || max <= 0 {
        return 0;
    }
    let lie = b"Edit\0";
    let cap = max as usize;
    let n = cap.min(lie.len());
    ptr::copy_nonoverlapping(lie.as_ptr(), buf, n);
    if n > 0 {
        *buf.add(n - 1) = 0;
    }
    (n.saturating_sub(1)) as i32
}

unsafe extern "system" fn hooked_gcn_w(hwnd: HWND, buf: *mut u16, max: i32) -> i32 {
    let ret_addr: usize;
    core::arch::asm!("mov {x}, [ebp + 4]", x = out(reg) ret_addr, options(nostack));
    if caller_should_be_lied_to(ret_addr)
        && crate::subclass::is_subclassed_lunicode_edit(hwnd)
    {
        let n = GCN_LIE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 {
            dbg_log!(
                "[ime] GetClassNameW LIE #{n} hwnd=0x{:X} ret=0x{:X} → 'Edit'",
                hwnd.0 as usize,
                ret_addr
            );
        }
        return write_edit_lie_w(buf, max);
    }
    let tramp = TRAMPOLINE_GCN_W.load(Ordering::Acquire);
    let original: GetClassNameWFn = std::mem::transmute(tramp);
    original(hwnd, buf, max)
}

unsafe extern "system" fn hooked_gcn_a(hwnd: HWND, buf: *mut u8, max: i32) -> i32 {
    let ret_addr: usize;
    core::arch::asm!("mov {x}, [ebp + 4]", x = out(reg) ret_addr, options(nostack));
    if caller_should_be_lied_to(ret_addr)
        && crate::subclass::is_subclassed_lunicode_edit(hwnd)
    {
        let n = GCN_LIE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 {
            dbg_log!(
                "[ime] GetClassNameA LIE #{n} hwnd=0x{:X} ret=0x{:X} → 'Edit'",
                hwnd.0 as usize,
                ret_addr
            );
        }
        return write_edit_lie_a(buf, max);
    }
    let tramp = TRAMPOLINE_GCN_A.load(Ordering::Acquire);
    let original: GetClassNameAFn = std::mem::transmute(tramp);
    original(hwnd, buf, max)
}

static TRAMPOLINE_A: AtomicUsize = AtomicUsize::new(0);
static TRAMPOLINE_W: AtomicUsize = AtomicUsize::new(0);
static TRAMPOLINE_REG_A: AtomicUsize = AtomicUsize::new(0);
static TRAMPOLINE_REG_W: AtomicUsize = AtomicUsize::new(0);
static HIT_COUNT: AtomicUsize = AtomicUsize::new(0);
static REG_HIT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "system" fn hooked_a(
    dw_ex_style: u32,
    lp_class_name: *const u8,
    lp_window_name: *const u8,
    dw_style: u32,
    x: i32,
    y: i32,
    n_width: i32,
    n_height: i32,
    h_wnd_parent: HWND,
    h_menu: HMENU,
    h_instance: HINSTANCE,
    lp_param: *const c_void,
) -> HWND {
    try_disable_tsf_once();
    let mut final_ex_style = dw_ex_style;
    if class_matches_a(lp_class_name) {
        final_ex_style = patched_lunicodeedit_ex_style(dw_ex_style);
        let n = HIT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 {
            dbg_log!(
                "[ime] CreateWindowExA LUnicodeEdit #{n}: ex 0x{:08X} -> 0x{:08X}",
                dw_ex_style,
                final_ex_style
            );
        }
    }
    let tramp = TRAMPOLINE_A.load(Ordering::Acquire);
    let original: CreateWindowExAFn = std::mem::transmute(tramp);
    original(
        final_ex_style,
        lp_class_name,
        lp_window_name,
        dw_style,
        x,
        y,
        n_width,
        n_height,
        h_wnd_parent,
        h_menu,
        h_instance,
        lp_param,
    )
}

unsafe extern "system" fn hooked_w(
    dw_ex_style: u32,
    lp_class_name: *const u16,
    lp_window_name: *const u16,
    dw_style: u32,
    x: i32,
    y: i32,
    n_width: i32,
    n_height: i32,
    h_wnd_parent: HWND,
    h_menu: HMENU,
    h_instance: HINSTANCE,
    lp_param: *const c_void,
) -> HWND {
    try_disable_tsf_once();
    let mut final_ex_style = dw_ex_style;
    if class_matches_w(lp_class_name) {
        final_ex_style = patched_lunicodeedit_ex_style(dw_ex_style);
        let n = HIT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 {
            dbg_log!(
                "[ime] CreateWindowExW LUnicodeEdit #{n}: ex 0x{:08X} -> 0x{:08X}",
                dw_ex_style,
                final_ex_style
            );
        }
    }
    let tramp = TRAMPOLINE_W.load(Ordering::Acquire);
    let original: CreateWindowExWFn = std::mem::transmute(tramp);
    original(
        final_ex_style,
        lp_class_name,
        lp_window_name,
        dw_style,
        x,
        y,
        n_width,
        n_height,
        h_wnd_parent,
        h_menu,
        h_instance,
        lp_param,
    )
}

unsafe extern "system" fn hooked_register_class_a(lpwcx: *const WNDCLASSEXA) -> u16 {
    let mut patched_class;
    let mut final_lpwcx = lpwcx;
    if !lpwcx.is_null() {
        let pcstr = (*lpwcx).lpszClassName;
        if class_matches_a(pcstr.0) {
            patched_class = *lpwcx;
            let old_style = patched_class.style.0;
            let new_style = patched_lunicodeedit_class_style(old_style);
            patched_class.style = WNDCLASS_STYLES(new_style);
            final_lpwcx = &patched_class;

            let n = REG_HIT.fetch_add(1, Ordering::Relaxed) + 1;
            dbg_log!(
                "[ime] RegisterClassExA LUnicodeEdit #{n}: style 0x{:08X} -> 0x{:08X}",
                old_style,
                new_style
            );
        }
    }
    let tramp = TRAMPOLINE_REG_A.load(Ordering::Acquire);
    let original: RegisterClassExAFn = std::mem::transmute(tramp);
    original(final_lpwcx)
}

unsafe extern "system" fn hooked_register_class_w(lpwcx: *const WNDCLASSEXW) -> u16 {
    let mut patched_class;
    let mut final_lpwcx = lpwcx;
    if !lpwcx.is_null() {
        let pcwstr = (*lpwcx).lpszClassName;
        if class_matches_w(pcwstr.0) {
            patched_class = *lpwcx;
            let old_style = patched_class.style.0;
            let new_style = patched_lunicodeedit_class_style(old_style);
            patched_class.style = WNDCLASS_STYLES(new_style);
            final_lpwcx = &patched_class;

            let n = REG_HIT.fetch_add(1, Ordering::Relaxed) + 1;
            dbg_log!(
                "[ime] RegisterClassExW LUnicodeEdit #{n}: style 0x{:08X} -> 0x{:08X}",
                old_style,
                new_style
            );
        }
    }
    let tramp = TRAMPOLINE_REG_W.load(Ordering::Acquire);
    let original: RegisterClassExWFn = std::mem::transmute(tramp);
    original(final_lpwcx)
}

/// 比對 ANSI class name。如果 `p` 是 ATOM(< 0x10000)就跳過。
unsafe fn class_matches_a(p: *const u8) -> bool {
    if (p as usize) < 0x10000 || p.is_null() {
        return false;
    }
    let target = TARGET_CLASS.as_bytes();
    for i in 0..target.len() {
        let c = *p.add(i);
        if c.eq_ignore_ascii_case(&target[i]) {
            continue;
        }
        return false;
    }
    *p.add(target.len()) == 0
}

/// 比對 wide class name。
unsafe fn class_matches_w(p: *const u16) -> bool {
    if (p as usize) < 0x10000 || p.is_null() {
        return false;
    }
    let target = TARGET_CLASS.as_bytes();
    for i in 0..target.len() {
        let c = *p.add(i);
        if c > 0x7F {
            return false;
        }
        if (c as u8).eq_ignore_ascii_case(&target[i]) {
            continue;
        }
        return false;
    }
    *p.add(target.len()) == 0
}

pub unsafe fn install() -> Result<(), String> {
    let user32 = GetModuleHandleA(PCSTR(b"user32.dll\0".as_ptr()))
        .map_err(|e| format!("GetModuleHandleA(user32.dll): {e}"))?;

    // RegisterClassEx 必須**比** CreateWindowEx 早裝 — class 註冊發生在第一個
    // window 建立之前。我們現在在 ResumeThread 之前的 worker thread 起頭就裝,
    // 一定早於遊戲 init。
    let reg_a = resolve(user32, b"RegisterClassExA\0")?;
    let reg_w = resolve(user32, b"RegisterClassExW\0")?;
    install_one(
        reg_a,
        hooked_register_class_a as *mut u8,
        &TRAMPOLINE_REG_A,
        "RegisterClassExA",
    )?;
    install_one(
        reg_w,
        hooked_register_class_w as *mut u8,
        &TRAMPOLINE_REG_W,
        "RegisterClassExW",
    )?;

    let target_a = resolve(user32, b"CreateWindowExA\0")?;
    let target_w = resolve(user32, b"CreateWindowExW\0")?;
    install_one(target_a, hooked_a as *mut u8, &TRAMPOLINE_A, "CreateWindowExA")?;
    install_one(target_w, hooked_w as *mut u8, &TRAMPOLINE_W, "CreateWindowExW")?;

    // GetClassName lie 路線已退回 — 之前嘗試裝 GetClassNameA/W 騙 IME UI
    // 把 LUnicodeEdit 講成 Edit,結果遊戲閃退。改走自繪候選(overlay.rs)。
    Ok(())
}

unsafe fn resolve(module: HMODULE, name: &[u8]) -> Result<*mut u8, String> {
    let p = GetProcAddress(module, PCSTR(name.as_ptr()))
        .ok_or_else(|| format!("GetProcAddress({:?}) returned NULL", std::str::from_utf8(name)))?;
    Ok(p as *mut u8)
}

unsafe fn install_one(
    target: *mut u8,
    detour: *mut u8,
    trampoline_slot: &AtomicUsize,
    tag: &str,
) -> Result<(), String> {
    // 確認 prologue 是預期的 5-byte hot-patch 形式
    let mut prologue = [0u8; 5];
    ptr::copy_nonoverlapping(target, prologue.as_mut_ptr(), 5);
    if prologue != EXPECTED_PROLOGUE {
        return Err(format!(
            "{tag} prologue not hot-patchable: {:02X?} (expected {:02X?})",
            prologue, EXPECTED_PROLOGUE
        ));
    }

    // 配置 trampoline:[原 5 bytes][JMP rel32 回 target+5]
    let trampoline = VirtualAlloc(
        None,
        16,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_EXECUTE_READWRITE,
    ) as *mut u8;
    if trampoline.is_null() {
        return Err(format!("{tag}: VirtualAlloc trampoline failed"));
    }
    ptr::copy_nonoverlapping(target, trampoline, 5);
    *trampoline.add(5) = 0xE9; // JMP rel32
    let rel = (target as i32)
        .wrapping_add(5)
        .wrapping_sub((trampoline as i32).wrapping_add(10));
    ptr::write_unaligned(trampoline.add(6) as *mut i32, rel);
    trampoline_slot.store(trampoline as usize, Ordering::Release);

    // 改 target 前 5 bytes 為 JMP rel32 → detour
    let mut old = PAGE_PROTECTION_FLAGS(0);
    VirtualProtect(target as *const _, 5, PAGE_EXECUTE_READWRITE, &mut old)
        .map_err(|e| format!("{tag}: VirtualProtect RWX failed: {e}"))?;
    *target = 0xE9;
    let rel = (detour as i32).wrapping_sub((target as i32).wrapping_add(5));
    ptr::write_unaligned(target.add(1) as *mut i32, rel);
    let mut prev = PAGE_PROTECTION_FLAGS(0);
    let _ = VirtualProtect(target as *const _, 5, old, &mut prev);

    dbg_log!(
        "[ime] {tag} hooked: target=0x{:X} trampoline=0x{:X} detour=0x{:X}",
        target as usize,
        trampoline as usize,
        detour as usize
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddraw_mode_keeps_window_ex_style_unchanged() {
        let original = 0x0000_0100;

        let patched = patched_lunicodeedit_ex_style(original);

        assert_eq!(patched, original);
    }

    #[test]
    fn ddraw_mode_keeps_class_dc_styles_unchanged() {
        let original = CS_OWNDC | CS_CLASSDC | 0x0008;

        let patched = patched_lunicodeedit_class_style(original);

        assert_eq!(patched, original);
    }
}
