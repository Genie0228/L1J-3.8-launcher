//! DDraw 表面操作 hook + 螢幕 capture
//!
//! Phase 3 階段:hook 在 game 呼叫 primary surface 的 Blt 時,**呼叫原 Blt 之後**
//! 用 GetDC + BitBlt 把當前 surface 內容捕獲到我們的 memory DIB。
//! 配合節流(15 fps capture),避免每幀 60 fps GetDC 太重。
//!
//! 後續 Phase 4 LUnicodeEdit subclass 會從這個 DIB 拿背景像素疊文字,
//! 取代 WS_EX_COMPOSITED 解打字閃爍 / 黑框。
//!
//! ## ABI 重要點(Phase 2 驗證)
//!
//! apphelp DXCOMHooks shim **用 stdcall(this 在 stack)不是 thiscall**。
//! 所有 thunk 因此只需 forward stack args,不處理 ECX。Hook 函式自己 call original。
//!
//! ## 已驗證的 vtable index
//!
//! - [5]  Blt        — windowed mode 的 present 主路徑(`DDBLT_WAIT` 為 game 標準 flag)
//! - [7]  BltFast    — 備用(目前 game 似乎不走)
//! - [11] Flip       — 不會 fire(windowed mode 不用 Flip)
//! - [17] GetDC      — 拿 surface GDI HDC(capture 用)
//! - [26] ReleaseDC  — 釋放 surface HDC

use core::arch::naked_asm;
use core::mem;
use core::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    SRCCOPY,
};
use windows::Win32::System::Memory::{VirtualProtect, PAGE_PROTECTION_FLAGS, PAGE_READWRITE};

use crate::dbg_log;

// =================================================================
// 靜態位址(Phase 1 RE)+ 動態狀態
// =================================================================

const PRIMARY_SURFACE_GLOBAL: usize = 0x009A_84E8;

const BLT_VTABLE_INDEX: usize = 5;
const BLTFAST_VTABLE_INDEX: usize = 7;
const FLIP_VTABLE_INDEX: usize = 11;
const GETDC_VTABLE_INDEX: usize = 17;
const RELEASEDC_VTABLE_INDEX: usize = 26;

/// 原 Blt / BltFast / Flip 函式位址(thunk 透過 hook 間接 call)
static ORIGINAL_BLT: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_BLTFAST: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_FLIP: AtomicUsize = AtomicUsize::new(0);

/// 原 GetDC / ReleaseDC 函式位址(我們直接呼叫做 capture)
static GETDC_ADDR: AtomicUsize = AtomicUsize::new(0);
static RELEASEDC_ADDR: AtomicUsize = AtomicUsize::new(0);

static INSTALLED: AtomicBool = AtomicBool::new(false);
static PRIMARY_SURFACE: AtomicUsize = AtomicUsize::new(0);

static BLT_COUNT: AtomicU64 = AtomicU64::new(0);
static BLTFAST_COUNT: AtomicU64 = AtomicU64::new(0);
static FLIP_COUNT: AtomicU64 = AtomicU64::new(0);

static CAPTURE_OK_COUNT: AtomicU64 = AtomicU64::new(0);
static CAPTURE_FAIL_COUNT: AtomicU64 = AtomicU64::new(0);

/// 每 N 幀 capture 一次(60fps / N = capture rate)
/// 4 = 15fps,對 LUnicodeEdit 顯示 IME 文字夠用了
const CAPTURE_EVERY_N: u64 = 4;

// =================================================================
// Capture buffer(memory DC + DIB section)
// =================================================================

struct CaptureBuffer {
    mem_dc: HDC,
    bitmap: HBITMAP,
    old_obj: HGDIOBJ,
    width: i32,
    height: i32,
}

// SAFETY: HDC / HGDIOBJ 是 process-global GDI handle,跨 thread 用沒問題(只要呼叫
// GDI API 時不要有兩個 thread 同時 BitBlt 同一個 HDC,我們用 Mutex 保護存取)
unsafe impl Send for CaptureBuffer {}

static CAPTURE_BUFFER: Mutex<Option<CaptureBuffer>> = Mutex::new(None);

unsafe fn create_capture_buffer(width: i32, height: i32) -> Option<CaptureBuffer> {
    let mem_dc = unsafe { CreateCompatibleDC(None) };
    if mem_dc.is_invalid() {
        return None;
    }

    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = width;
    bmi.bmiHeader.biHeight = -height; // 負值 = top-down,跟 surface 方向一致
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB.0;

    let mut pixels: *mut core::ffi::c_void = ptr::null_mut();
    let bitmap = match unsafe {
        CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut pixels, None, 0)
    } {
        Ok(b) => b,
        Err(_) => {
            let _ = unsafe { DeleteDC(mem_dc) };
            return None;
        }
    };

    let old_obj = unsafe { SelectObject(mem_dc, bitmap.into()) };

    Some(CaptureBuffer {
        mem_dc,
        bitmap,
        old_obj,
        width,
        height,
    })
}

/// 公開 API:讓 subclass 在 WM_PAINT 用 BitBlt 取出 capture 內容
///
/// 用 closure 是為了把 mutex lock 範圍限制在 closure 內,避免 caller 拿 raw HDC
/// 出去後 mutex 已被釋放又有別人改 capture 的競態。
pub fn with_capture_dc<R>(f: impl FnOnce(HDC, i32, i32) -> R) -> Option<R> {
    let buffer = CAPTURE_BUFFER.lock().ok()?;
    let b = buffer.as_ref()?;
    Some(f(b.mem_dc, b.width, b.height))
}

// =================================================================
// Capture 本體 — 在 Blt 成功之後對 primary surface 做 GetDC + BitBlt
// =================================================================

type GetDcFn = unsafe extern "stdcall" fn(this: usize, phdc: *mut HDC) -> i32;
type ReleaseDcFn = unsafe extern "stdcall" fn(this: usize, hdc: HDC) -> i32;

unsafe fn capture_primary_surface(this: usize) {
    let getdc_raw = GETDC_ADDR.load(Ordering::Relaxed);
    let releasedc_raw = RELEASEDC_ADDR.load(Ordering::Relaxed);
    if getdc_raw == 0 || releasedc_raw == 0 {
        return;
    }
    let get_dc: GetDcFn = unsafe { mem::transmute(getdc_raw) };
    let release_dc: ReleaseDcFn = unsafe { mem::transmute(releasedc_raw) };

    let buffer_guard = match CAPTURE_BUFFER.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let buffer = match buffer_guard.as_ref() {
        Some(b) => b,
        None => return,
    };

    let mut surface_hdc = HDC::default();
    let hr = unsafe { get_dc(this, &mut surface_hdc as *mut _) };
    if hr < 0 || surface_hdc.is_invalid() {
        let fails = CAPTURE_FAIL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if fails <= 3 || fails % 60 == 0 {
            dbg_log!(
                "[ddraw] capture GetDC FAIL #{} hr=0x{:08X} this=0x{:X}",
                fails,
                hr as u32,
                this
            );
        }
        return;
    }

    let blt_ok = unsafe {
        BitBlt(
            buffer.mem_dc,
            0,
            0,
            buffer.width,
            buffer.height,
            Some(surface_hdc),
            0,
            0,
            SRCCOPY,
        )
    };

    let _ = unsafe { release_dc(this, surface_hdc) };

    if blt_ok.is_ok() {
        let oks = CAPTURE_OK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if oks <= 3 || oks % 60 == 0 {
            dbg_log!(
                "[ddraw] capture OK #{} this=0x{:X} {}x{}",
                oks,
                this,
                buffer.width,
                buffer.height
            );
        }
    } else {
        let fails = CAPTURE_FAIL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if fails <= 3 || fails % 60 == 0 {
            dbg_log!(
                "[ddraw] capture BitBlt FAIL #{} this=0x{:X}",
                fails,
                this
            );
        }
    }
}

// =================================================================
// Hook 函式(stdcall,call 原函式之後 capture)
// =================================================================

fn maybe_log_call(name: &str, count: u64, this: usize, extra: &str) {
    let primary = PRIMARY_SURFACE.load(Ordering::Relaxed);
    if count <= 3 || count % 600 == 0 {
        dbg_log!(
            "[ddraw] {} #{} this=0x{:X} is_primary={} {}",
            name,
            count,
            this,
            this == primary,
            extra
        );
    }
}

extern "stdcall" fn flip_hook(this: usize, target: usize, flags: u32) -> i32 {
    let count = FLIP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    maybe_log_call(
        "Flip",
        count,
        this,
        &format!("target=0x{:X} flags=0x{:X}", target, flags),
    );

    let orig = ORIGINAL_FLIP.load(Ordering::Relaxed);
    type FlipFn = unsafe extern "stdcall" fn(usize, usize, u32) -> i32;
    let f: FlipFn = unsafe { mem::transmute(orig) };
    unsafe { f(this, target, flags) }
}

extern "stdcall" fn blt_hook(
    this: usize,
    dst_rect: usize,
    src_surf: usize,
    src_rect: usize,
    flags: u32,
    blt_fx: usize,
) -> i32 {
    let count = BLT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    maybe_log_call(
        "Blt",
        count,
        this,
        &format!(
            "dstR=0x{:X} srcS=0x{:X} srcR=0x{:X} flags=0x{:X} fx=0x{:X}",
            dst_rect, src_surf, src_rect, flags, blt_fx
        ),
    );

    // 先 call 原 Blt 拿結果
    let orig = ORIGINAL_BLT.load(Ordering::Relaxed);
    type BltFn = unsafe extern "stdcall" fn(usize, usize, usize, usize, u32, usize) -> i32;
    let f: BltFn = unsafe { mem::transmute(orig) };
    let result = unsafe { f(this, dst_rect, src_surf, src_rect, flags, blt_fx) };

    // primary surface 的 Blt 成功 → 節流後 capture
    if result >= 0 && this == PRIMARY_SURFACE.load(Ordering::Relaxed) {
        if count % CAPTURE_EVERY_N == 0 {
            unsafe { capture_primary_surface(this) };
        }
    }

    result
}

extern "stdcall" fn bltfast_hook(
    this: usize,
    dst_x: u32,
    dst_y: u32,
    src_surf: usize,
    src_rect: usize,
    flags: u32,
) -> i32 {
    let count = BLTFAST_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    maybe_log_call(
        "BltFast",
        count,
        this,
        &format!(
            "dst=({},{}) srcS=0x{:X} srcR=0x{:X} flags=0x{:X}",
            dst_x, dst_y, src_surf, src_rect, flags
        ),
    );

    let orig = ORIGINAL_BLTFAST.load(Ordering::Relaxed);
    type BltFastFn = unsafe extern "stdcall" fn(usize, u32, u32, usize, usize, u32) -> i32;
    let f: BltFastFn = unsafe { mem::transmute(orig) };
    let result = unsafe { f(this, dst_x, dst_y, src_surf, src_rect, flags) };

    if result >= 0 && this == PRIMARY_SURFACE.load(Ordering::Relaxed) {
        if count % CAPTURE_EVERY_N == 0 {
            unsafe { capture_primary_surface(this) };
        }
    }

    result
}

// =================================================================
// Thunk(stdcall:全部從 caller stack forward,call hook 後直接 ret)
// =================================================================

/// Flip thunk — stdcall(this, target, flags),3 args = 12 bytes stack
///
/// Stack at entry: [ret][this][target][flags]
/// 反向 push 給 hook(hook 自己 ret 12 清自己的 args),thunk ret 12 清 caller 的 args。
#[unsafe(naked)]
unsafe extern "C" fn flip_thunk() {
    naked_asm!(
        "push DWORD PTR [esp + 12]", // flags
        "push DWORD PTR [esp + 12]", // target
        "push DWORD PTR [esp + 12]", // this
        "call {hook}",
        "ret 12",
        hook = sym flip_hook,
    );
}

/// Blt thunk — stdcall(this, dstRect, srcSurf, srcRect, flags, bltFx),6 args = 24 bytes
#[unsafe(naked)]
unsafe extern "C" fn blt_thunk() {
    naked_asm!(
        "push DWORD PTR [esp + 24]", // bltFx
        "push DWORD PTR [esp + 24]", // flags
        "push DWORD PTR [esp + 24]", // srcRect
        "push DWORD PTR [esp + 24]", // srcSurf
        "push DWORD PTR [esp + 24]", // dstRect
        "push DWORD PTR [esp + 24]", // this
        "call {hook}",
        "ret 24",
        hook = sym blt_hook,
    );
}

/// BltFast thunk — stdcall(this, dstX, dstY, srcSurf, srcRect, flags),6 args = 24 bytes
#[unsafe(naked)]
unsafe extern "C" fn bltfast_thunk() {
    naked_asm!(
        "push DWORD PTR [esp + 24]", // flags
        "push DWORD PTR [esp + 24]", // srcRect
        "push DWORD PTR [esp + 24]", // srcSurf
        "push DWORD PTR [esp + 24]", // dstY
        "push DWORD PTR [esp + 24]", // dstX
        "push DWORD PTR [esp + 24]", // this
        "call {hook}",
        "ret 24",
        hook = sym bltfast_hook,
    );
}

// =================================================================
// Install
// =================================================================

unsafe fn patch_one_slot(
    vtable_ptr: usize,
    idx: usize,
    new_target: usize,
    save_to: &AtomicUsize,
) -> Result<(usize, usize), String> {
    let slot_addr = vtable_ptr + idx * 4;
    let current = unsafe { *(slot_addr as *const usize) };
    if current == 0 {
        return Err(format!("vtable[{}] slot @ 0x{:X} 是 null", idx, slot_addr));
    }

    let mut old_prot = PAGE_PROTECTION_FLAGS(0);
    let result =
        unsafe { VirtualProtect(slot_addr as *mut _, 4, PAGE_READWRITE, &mut old_prot) };
    if result.is_err() {
        return Err(format!("VirtualProtect failed at 0x{:X}", slot_addr));
    }

    save_to.store(current, Ordering::SeqCst);
    unsafe {
        *(slot_addr as *mut usize) = new_target;
    }

    let _ = unsafe { VirtualProtect(slot_addr as *mut _, 4, old_prot, &mut old_prot) };

    Ok((slot_addr, current))
}

pub unsafe fn install() -> Result<(), String> {
    if INSTALLED.load(Ordering::Relaxed) {
        return Ok(());
    }

    let surface_ptr = unsafe { *(PRIMARY_SURFACE_GLOBAL as *const usize) };
    if surface_ptr == 0 {
        return Err(format!(
            "primary surface global @ 0x{:X} 是 null — game 還沒 init?",
            PRIMARY_SURFACE_GLOBAL
        ));
    }

    let vtable_ptr = unsafe { *(surface_ptr as *const usize) };
    if vtable_ptr == 0 {
        return Err(format!("surface 0x{:X} 的 vtable 是 null", surface_ptr));
    }

    PRIMARY_SURFACE.store(surface_ptr, Ordering::SeqCst);

    // 抓 GetDC / ReleaseDC 位址(我們不 hook,只 call)
    let getdc_addr = unsafe { *((vtable_ptr + GETDC_VTABLE_INDEX * 4) as *const usize) };
    let releasedc_addr =
        unsafe { *((vtable_ptr + RELEASEDC_VTABLE_INDEX * 4) as *const usize) };
    if getdc_addr == 0 || releasedc_addr == 0 {
        return Err("GetDC / ReleaseDC vtable entry 是 null".to_string());
    }
    GETDC_ADDR.store(getdc_addr, Ordering::SeqCst);
    RELEASEDC_ADDR.store(releasedc_addr, Ordering::SeqCst);

    // 建 capture buffer(用 game window client 大小 1200x900)
    let buffer = match unsafe { create_capture_buffer(1200, 900) } {
        Some(b) => b,
        None => return Err("create capture buffer failed".to_string()),
    };
    let cap_dc = buffer.mem_dc;
    *CAPTURE_BUFFER.lock().map_err(|e| e.to_string())? = Some(buffer);

    // 安裝 hook(三個 slot)
    let blt_thunk_addr = blt_thunk as *const () as usize;
    let bltfast_thunk_addr = bltfast_thunk as *const () as usize;
    let flip_thunk_addr = flip_thunk as *const () as usize;

    let (blt_slot, blt_orig) = unsafe {
        patch_one_slot(vtable_ptr, BLT_VTABLE_INDEX, blt_thunk_addr, &ORIGINAL_BLT)
    }?;
    let (bltfast_slot, bltfast_orig) = unsafe {
        patch_one_slot(
            vtable_ptr,
            BLTFAST_VTABLE_INDEX,
            bltfast_thunk_addr,
            &ORIGINAL_BLTFAST,
        )
    }?;
    let (flip_slot, flip_orig) = unsafe {
        patch_one_slot(vtable_ptr, FLIP_VTABLE_INDEX, flip_thunk_addr, &ORIGINAL_FLIP)
    }?;

    INSTALLED.store(true, Ordering::SeqCst);

    dbg_log!(
        "[ddraw] install OK: surface=0x{:X} vtable=0x{:X} cap_dc=0x{:X}",
        surface_ptr,
        vtable_ptr,
        cap_dc.0 as usize
    );
    dbg_log!(
        "[ddraw]   Blt     slot=0x{:X} orig=0x{:X} thunk=0x{:X}",
        blt_slot,
        blt_orig,
        blt_thunk_addr
    );
    dbg_log!(
        "[ddraw]   BltFast slot=0x{:X} orig=0x{:X} thunk=0x{:X}",
        bltfast_slot,
        bltfast_orig,
        bltfast_thunk_addr
    );
    dbg_log!(
        "[ddraw]   Flip    slot=0x{:X} orig=0x{:X} thunk=0x{:X}",
        flip_slot,
        flip_orig,
        flip_thunk_addr
    );
    dbg_log!(
        "[ddraw]   GetDC=0x{:X} ReleaseDC=0x{:X} capture rate=1/{} frames",
        getdc_addr,
        releasedc_addr,
        CAPTURE_EVERY_N
    );

    Ok(())
}
