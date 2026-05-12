//! 自繪候選字 overlay window(Win11 風格,UpdateLayeredWindow 路徑)
//!
//! ## 為什麼用 UpdateLayeredWindow
//!
//! 遊戲是 legacy DirectDraw windowed mode,DDraw 把 primary surface
//! Blt 整塊蓋掉,**不理會** WS_EX_TOPMOST 的 clip region。所以普通
//! WS_POPUP top-most 視窗的內容會被遊戲 surface 覆蓋,看起來像透明
//! 又一直閃。
//!
//! 唯一打贏 DDraw 的方法是讓 DWM 在最後一刻合成我們的 bitmap 在所有
//! surface 之上 — 那就是 `UpdateLayeredWindow` + `WS_EX_LAYERED`。
//! 這條路 OBS / aim assist tool / Discord overlay 全部都用,即使遊戲
//! 是 fullscreen DX 也壓得住(windowed DDraw 更不在話下)。
//!
//! 流程跟一般 GDI 不一樣:
//!   - **不**用 InvalidateRect / BeginPaint / EndPaint
//!   - 自己建 32bpp ARGB DIB section
//!   - 用 GDI 把內容畫進去(跟原本完全一樣的 layout 程式碼)
//!   - 最後一次 alpha-postprocess 把每個 pixel 的 A 設成 0xFF(GDI
//!     不會自動填 alpha,DrawText 出來的 byte 是 RGB0)
//!   - 呼叫 `UpdateLayeredWindow` 把 bitmap commit 給 DWM
//!
//! WM_PAINT 還是可能來(系統某些事件會觸發),但我們 ValidateRect
//! 即可,不在那邊畫 — 所有繪製都從 show_for / update 的 path 主動觸發。
//!
//! ## 視窗特性
//!   - WS_POPUP(無邊框)
//!   - WS_EX_LAYERED(必要 — UpdateLayeredWindow 前提)
//!   - WS_EX_TOPMOST(永遠最上層)
//!   - WS_EX_NOACTIVATE(顯示時不搶焦點)
//!   - WS_EX_TOOLWINDOW(不出現在工作列 / Alt-Tab)
//!   - DWMWA_WINDOW_CORNER_PREFERENCE = ROUND(Win11 圓角)

use std::ptr::null_mut;
use std::sync::Mutex;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreatePen, CreateSolidBrush, DeleteDC,
    DeleteObject, DrawTextW, FillRect, GetDC, GetStockObject, ReleaseDC, Rectangle, SelectObject,
    SetBkMode, SetTextColor, ValidateRect, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS,
    DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FW_NORMAL, NULL_BRUSH, OUT_DEFAULT_PRECIS,
    PROOF_QUALITY, PS_SOLID, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, FindWindowW, GetSystemMetrics, GetWindowRect, LoadCursorW,
    RegisterClassExW, SetWindowPos, ShowWindow, UpdateLayeredWindow, CS_HREDRAW, CS_VREDRAW,
    HWND_TOPMOST, IDC_ARROW, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WM_PAINT, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::candidates::ImeState;
use crate::dbg_log;

const CLASS_NAME: PCWSTR = w!("LineageImeOverlay");
const GAME_CLASS: PCWSTR = w!("Lineage");
const GAME_TITLE: PCWSTR = w!("Lineage Windows Client (13081901)");

// ===== 尺寸常數(垂直布局)=====
const PADDING_X: i32 = 8;
const PADDING_Y: i32 = 6;
const COMP_H: i32 = 22; // 組字字串列高
const ITEM_H: i32 = 26; // 每個候選字列高
const NUM_W: i32 = 22; // 數字 prefix 寬度
const ACCENT_W: i32 = 3; // 左邊 accent 條寬度
const WIN_W: i32 = 220; // 整個視窗寬度
const FONT_SIZE: i32 = 16;
const COMP_FONT_SIZE: i32 = 14;
const MAX_PAGE_SIZE: usize = 9;

// ===== 配色(BGR 格式 — COLORREF 是 0x00BBGGRR)=====
const BG_COLOR: u32 = 0x00202020; // 深灰背景
const BORDER_COLOR: u32 = 0x003C3C3C; // 邊框
const TEXT_COLOR: u32 = 0x00FFFFFF; // 白文字
const NUM_COLOR: u32 = 0x00B4B4B4; // 數字淺灰
const COMP_COLOR: u32 = 0x00AAAAAA; // 組字中灰
const SEL_BG: u32 = 0x003C3C3C; // 選中背景
const ACCENT_COLOR: u32 = 0x00D77800; // Win11 accent 藍(BGR: 00 78 D7)

// ===== 全域狀態 =====
static OVERLAY_HWND: Mutex<isize> = Mutex::new(0);
static CURRENT_STATE: Mutex<Option<ImeState>> = Mutex::new(None);
static ATTACHED_TO: Mutex<isize> = Mutex::new(0);
static VISIBLE: Mutex<bool> = Mutex::new(false);
// 目前視窗位置(screen),paint_layered 要傳給 UpdateLayeredWindow
static POS: Mutex<(i32, i32, i32, i32)> = Mutex::new((0, 0, WIN_W, ITEM_H * 9 + PADDING_Y * 2));

pub fn set_overlay_hwnd(h: HWND) {
    *OVERLAY_HWND.lock().unwrap() = h.0 as isize;
}
fn overlay_hwnd() -> HWND {
    HWND(*OVERLAY_HWND.lock().unwrap() as *mut _)
}
pub fn is_visible() -> bool {
    *VISIBLE.lock().unwrap()
}

/// 計算 overlay 視窗高度 — 視組字 + 候選數而定
fn compute_height(state: &ImeState) -> i32 {
    let mut h = PADDING_Y * 2;
    if !state.composition.is_empty() {
        h += COMP_H + 2;
    }
    let n = state.page_items().len().min(MAX_PAGE_SIZE);
    h += (n as i32) * ITEM_H;
    h.max(PADDING_Y * 2 + ITEM_H)
}

/// LUnicodeEdit 收到 IMN_OPENCANDIDATE 時叫
pub unsafe fn show_for(input_hwnd: HWND, state: ImeState) {
    *ATTACHED_TO.lock().unwrap() = input_hwnd.0 as isize;
    let win_h = compute_height(&state);
    *CURRENT_STATE.lock().unwrap() = Some(state);
    *VISIBLE.lock().unwrap() = true;

    let hwnd = overlay_hwnd();
    if hwnd.0.is_null() {
        return;
    }
    position_near(input_hwnd, win_h);
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    paint_layered(hwnd);
}

/// IMN_CHANGECANDIDATE 或 WM_IME_COMPOSITION 觸發時更新狀態
pub unsafe fn update(state: ImeState) {
    let win_h = compute_height(&state);
    let attached = HWND(*ATTACHED_TO.lock().unwrap() as *mut _);
    *CURRENT_STATE.lock().unwrap() = Some(state);
    let hwnd = overlay_hwnd();
    if hwnd.0.is_null() {
        return;
    }
    if !attached.0.is_null() {
        position_near(attached, win_h);
    }
    paint_layered(hwnd);
}

/// IMN_CLOSECANDIDATE / WM_KILLFOCUS / WM_DESTROY 隱藏
pub unsafe fn hide() {
    *VISIBLE.lock().unwrap() = false;
    let hwnd = overlay_hwnd();
    if hwnd.0.is_null() {
        return;
    }
    let _ = ShowWindow(hwnd, SW_HIDE);
}

/// 把 overlay 擺在輸入框下方 4px,自動避開螢幕邊界。
///
/// **不**呼叫 SetWindowPos — UpdateLayeredWindow 用 pptDst + psize 已經
/// 原子改位置+大小,而 SetWindowPos 在 layered window 上會多走一次 NCCALC
/// + WM_PAINT path,造成閃爍。我們只更新 POS state,實際移動由下一次
/// paint_layered 透過 UpdateLayeredWindow 完成。
unsafe fn position_near(input_hwnd: HWND, win_h: i32) {
    let mut rect = RECT::default();
    if GetWindowRect(input_hwnd, &mut rect).is_err() {
        return;
    }
    let x = rect.left;
    let mut y = rect.bottom + 4;

    let screen_w = GetSystemMetrics(SM_CXSCREEN);
    let screen_h = GetSystemMetrics(SM_CYSCREEN);
    let final_x = x.min(screen_w - WIN_W).max(0);
    if y + win_h > screen_h {
        y = rect.top - win_h - 4;
    }
    let final_y = y.max(0);

    *POS.lock().unwrap() = (final_x, final_y, WIN_W, win_h);
}

// ===== 視窗註冊 =====

pub unsafe fn wait_for_game_window(timeout_ms: u32) -> Option<HWND> {
    let start = std::time::Instant::now();
    loop {
        if let Ok(h) = FindWindowW(GAME_CLASS, None) {
            if !h.0.is_null() {
                return Some(h);
            }
        }
        if let Ok(h) = FindWindowW(None, GAME_TITLE) {
            if !h.0.is_null() {
                return Some(h);
            }
        }
        if start.elapsed().as_millis() as u32 >= timeout_ms {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

pub unsafe fn create_overlay(_parent: HWND) -> Result<HWND, &'static str> {
    let hinst = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
        .map_err(|_| "GetModuleHandle")?;

    let cursor = LoadCursorW(None, IDC_ARROW).map_err(|_| "LoadCursor")?;

    let cls = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(overlay_wndproc),
        hInstance: hinst.into(),
        hCursor: cursor,
        // hbrBackground 留 null — UpdateLayeredWindow 不靠 WM_ERASEBKGND
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(null_mut()),
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    let _ = RegisterClassExW(&cls);

    // WS_EX_LAYERED 必要 — UpdateLayeredWindow 前提
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        CLASS_NAME,
        w!("LineageIME"),
        WS_POPUP,
        0,
        0,
        WIN_W,
        ITEM_H * 9 + PADDING_Y * 2,
        None,
        None,
        Some(hinst.into()),
        None,
    )
    .map_err(|_| "CreateWindowEx")?;

    // **不**呼叫 SetLayeredWindowAttributes — 改用 UpdateLayeredWindow path,
    // 兩個 API 互斥。SetLayeredWindowAttributes 會把視窗標成 LWA mode,後續
    // UpdateLayeredWindow 就 fail 了。

    // Win11 圓角
    let corner: u32 = 2;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &corner as *const u32 as *const _,
        std::mem::size_of::<u32>() as u32,
    );

    dbg_log!("[ime-overlay] created hwnd=0x{:X}", hwnd.0 as usize);
    Ok(hwnd)
}

extern "system" fn overlay_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            // 我們不靠 WM_PAINT — 從 show_for / update 主動 paint_layered。
            // 收到 WM_PAINT 時 ValidateRect 把 dirty 區域消掉避免重複觸發。
            WM_PAINT => {
                let _ = ValidateRect(Some(hwnd), None);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// ===== UpdateLayeredWindow 渲染 =====

unsafe fn paint_layered(hwnd: HWND) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static PAINT_COUNT: AtomicUsize = AtomicUsize::new(0);
    let n = PAINT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 5 {
        dbg_log!("[ime-overlay] paint_layered #{n} hwnd=0x{:X}", hwnd.0 as usize);
    }

    let (pos_x, pos_y, win_w, win_h) = *POS.lock().unwrap();
    if win_w <= 0 || win_h <= 0 {
        return;
    }

    // 螢幕 DC — UpdateLayeredWindow hdcDst 用
    let screen_dc = GetDC(None);
    if screen_dc.0.is_null() {
        if n <= 5 {
            dbg_log!("[ime-overlay] GetDC NULL");
        }
        return;
    }

    // 32bpp top-down DIB section
    let mut bmi: BITMAPINFO = std::mem::zeroed();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = win_w;
    bmi.bmiHeader.biHeight = -win_h; // negative = top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB.0;

    let mut bits_ptr: *mut std::ffi::c_void = null_mut();
    let bmp = match CreateDIBSection(
        Some(screen_dc),
        &bmi,
        DIB_RGB_COLORS,
        &mut bits_ptr,
        None,
        0,
    ) {
        Ok(b) => b,
        Err(_) => {
            ReleaseDC(None, screen_dc);
            if n <= 5 {
                dbg_log!("[ime-overlay] CreateDIBSection FAIL");
            }
            return;
        }
    };

    let mem_dc = CreateCompatibleDC(Some(screen_dc));
    let old_bmp = SelectObject(mem_dc, bmp.into());

    // === 把所有候選 UI 畫進 mem_dc(沿用原本 layout 程式碼)===
    draw_into(mem_dc, win_w, win_h);

    // === 把每個 pixel 的 alpha 補成 0xFF ===
    // GDI FillRect / DrawTextW 不會碰 alpha byte,UpdateLayeredWindow + AC_SRC_ALPHA
    // 看到 alpha=0 = 完全透明,所以要手動 force 0xFF。
    let bits = bits_ptr as *mut u8;
    let total = (win_w * win_h * 4) as usize;
    let mut i = 3;
    while i < total {
        *bits.add(i) = 0xFF;
        i += 4;
    }

    // === UpdateLayeredWindow commit ===
    let pt_dst = POINT { x: pos_x, y: pos_y };
    let sz = SIZE { cx: win_w, cy: win_h };
    let pt_src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    let r = UpdateLayeredWindow(
        hwnd,
        Some(screen_dc),
        Some(&pt_dst),
        Some(&sz),
        Some(mem_dc),
        Some(&pt_src),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );
    if n <= 5 {
        dbg_log!(
            "[ime-overlay] UpdateLayeredWindow #{n} pos=({pos_x},{pos_y}) size={win_w}x{win_h} ok={}",
            r.is_ok()
        );
    }

    SelectObject(mem_dc, old_bmp);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(mem_dc);
    ReleaseDC(None, screen_dc);
    // 注:不在 paint 末尾 SetWindowPos(TOPMOST) — 那會 invalidate 額外觸發
    // 重畫,造成閃爍。WS_EX_TOPMOST 已經在 create 時加,DWM 不會擅自降層。
}

// ===== 把候選 UI layout 畫進指定 mem_dc =====
unsafe fn draw_into(mem_dc: windows::Win32::Graphics::Gdi::HDC, win_w: i32, win_h: i32) {
    // 背景
    let bg_brush = CreateSolidBrush(COLORREF(BG_COLOR));
    let full = RECT {
        left: 0,
        top: 0,
        right: win_w,
        bottom: win_h,
    };
    let _ = FillRect(mem_dc, &full, bg_brush);
    let _ = DeleteObject(bg_brush.into());

    // 邊框 1px
    let pen = CreatePen(PS_SOLID, 1, COLORREF(BORDER_COLOR));
    let old_pen = SelectObject(mem_dc, pen.into());
    let null_brush_old = SelectObject(mem_dc, GetStockObject(NULL_BRUSH));
    let _ = Rectangle(mem_dc, 0, 0, win_w, win_h);
    SelectObject(mem_dc, null_brush_old);
    SelectObject(mem_dc, old_pen);
    let _ = DeleteObject(pen.into());

    // 字型
    let font = CreateFontW(
        FONT_SIZE,
        0,
        0,
        0,
        FW_NORMAL.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        PROOF_QUALITY,
        0x02,
        w!("Microsoft JhengHei"),
    );
    let comp_font = CreateFontW(
        COMP_FONT_SIZE,
        0,
        0,
        0,
        FW_NORMAL.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        PROOF_QUALITY,
        0x02,
        w!("Microsoft JhengHei"),
    );

    SetBkMode(mem_dc, TRANSPARENT);

    let mut y = PADDING_Y;
    let st = CURRENT_STATE.lock().unwrap();
    if let Some(state) = st.as_ref() {
        // === 組字字串(若有)===
        if !state.composition.is_empty() {
            let old_font = SelectObject(mem_dc, comp_font.into());
            SetTextColor(mem_dc, COLORREF(COMP_COLOR));
            let mut comp_w: Vec<u16> = state.composition.encode_utf16().collect();
            let mut rc = RECT {
                left: PADDING_X,
                top: y,
                right: win_w - PADDING_X,
                bottom: y + COMP_H,
            };
            let _ = DrawTextW(
                mem_dc,
                &mut comp_w,
                &mut rc,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(mem_dc, old_font);
            y += COMP_H + 2;
        }

        // === 候選字列(垂直)===
        let old_font = SelectObject(mem_dc, font.into());
        let page = state.page_items();
        let sel_in_page = state.page_selection();

        for (i, cand) in page.iter().enumerate().take(MAX_PAGE_SIZE) {
            let row_top = y + (i as i32) * ITEM_H;
            let row_bot = row_top + ITEM_H;
            let is_sel = Some(i) == sel_in_page;

            // 整列背景(選中時 highlight)+ 左邊 accent 條
            if is_sel {
                let sel_rc = RECT {
                    left: 1,
                    top: row_top,
                    right: win_w - 1,
                    bottom: row_bot,
                };
                let sel_brush = CreateSolidBrush(COLORREF(SEL_BG));
                let _ = FillRect(mem_dc, &sel_rc, sel_brush);
                let _ = DeleteObject(sel_brush.into());

                let accent_rc = RECT {
                    left: 1,
                    top: row_top + 4,
                    right: 1 + ACCENT_W,
                    bottom: row_bot - 4,
                };
                let accent_brush = CreateSolidBrush(COLORREF(ACCENT_COLOR));
                let _ = FillRect(mem_dc, &accent_rc, accent_brush);
                let _ = DeleteObject(accent_brush.into());
            }

            // 數字 prefix(1-9)
            SetTextColor(mem_dc, COLORREF(NUM_COLOR));
            let num_label = format!("{}", i + 1);
            let mut num_w: Vec<u16> = num_label.encode_utf16().collect();
            let mut num_rc = RECT {
                left: PADDING_X + ACCENT_W,
                top: row_top,
                right: PADDING_X + ACCENT_W + NUM_W,
                bottom: row_bot,
            };
            let _ = DrawTextW(
                mem_dc,
                &mut num_w,
                &mut num_rc,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );

            // 候選字
            SetTextColor(mem_dc, COLORREF(TEXT_COLOR));
            let mut cand_w: Vec<u16> = cand.encode_utf16().collect();
            let mut cand_rc = RECT {
                left: PADDING_X + ACCENT_W + NUM_W,
                top: row_top,
                right: win_w - PADDING_X,
                bottom: row_bot,
            };
            let _ = DrawTextW(
                mem_dc,
                &mut cand_w,
                &mut cand_rc,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        }
        SelectObject(mem_dc, old_font);
    }
    drop(st);

    let _ = DeleteObject(font.into());
    let _ = DeleteObject(comp_font.into());
}
