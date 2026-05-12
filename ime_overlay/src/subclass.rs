//! LUnicodeEdit 視窗 subclass
//!
//! 攔三類訊息:
//!   - WM_IME_NOTIFY:
//!       wp=IMN_OPENCANDIDATE   → 拉候選 list,顯示 overlay
//!       wp=IMN_CHANGECANDIDATE → 拉候選 list,觸發 overlay 重繪
//!       wp=IMN_CLOSECANDIDATE  → 隱藏 overlay
//!   - WM_IME_COMPOSITION:
//!       lp & GCS_COMPSTR → 組字字串變了,如果 overlay 已開就重繪
//!   - WM_KILLFOCUS / WM_DESTROY:
//!       自動隱藏 overlay,避免殘影
//!
//! 不能完全吃掉 IME 訊息 — 遊戲自己有 polling 邏輯讀組字字串(會用 ImmGetCompositionStringA)
//! 跟最終結果(GCS_RESULTSTR),所以原 wndproc 還是要 call 一遍,讓遊戲拿到該拿的。

use std::collections::HashMap;
use std::sync::Mutex;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush,
    DeleteDC, DeleteObject, FillRect, GetDC, GetSysColorBrush, InvalidateRect, ReleaseDC,
    ScreenToClient, SelectObject, SetBkColor, SetTextColor, BeginPaint, EndPaint, COLOR_WINDOW,
    HBRUSH, HDC, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::Input::Ime::{
    ImmGetContext, ImmGetDefaultIMEWnd, ImmReleaseContext, ImmSetCandidateWindow,
    ImmSetCompositionWindow, CANDIDATEFORM, CFS_CANDIDATEPOS, CFS_POINT, COMPOSITIONFORM,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, EnumChildWindows, EnumThreadWindows, EnumWindows,
    GetAncestor, GetClassNameW, GetClientRect, GetParent, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, IsWindowVisible, KillTimer, SendMessageW, SetTimer,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, EVENT_OBJECT_CREATE, EVENT_OBJECT_FOCUS,
    GA_ROOT, GWL_EXSTYLE, GWL_STYLE, GWLP_WNDPROC, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNOACTIVATE, WINEVENT_OUTOFCONTEXT, WM_CHAR, WM_CTLCOLOREDIT,
    WM_CTLCOLORSTATIC, WM_DESTROY, WM_ERASEBKGND, WM_IME_CHAR, WM_IME_COMPOSITION,
    WM_IME_ENDCOMPOSITION, WM_IME_NOTIFY, WM_IME_SETCONTEXT, WM_IME_STARTCOMPOSITION, WM_KEYUP,
    WM_KILLFOCUS, WM_NCPAINT, WM_PAINT, WM_PRINTCLIENT, WM_SETFOCUS, WM_SETTEXT, WM_TIMER,
};

const PRF_CLIENT: usize = 0x0000_0004;
const PRF_NONCLIENT: usize = 0x0000_0002;
#[cfg(test)]
const PRF_ERASEBKGND: usize = 0x0000_0008;
const EDIT_PRINT_FLAGS: usize = PRF_CLIENT | PRF_NONCLIENT;

use crate::dbg_log;

use crate::candidates::fetch_ime_state;
use crate::overlay;

const TARGET_CLASS: &str = "LUnicodeEdit";

const IMN_CHANGECANDIDATE: usize = 0x03;
const IMN_CLOSECANDIDATE: usize = 0x04;
const IMN_OPENCANDIDATE: usize = 0x05;

const GCS_COMPSTR: u32 = 0x0008;

/// Phase 4 repaint timer — 每 33ms 從 DDraw capture 灌一次背景到 LUnicodeEdit
/// 的 redirection bitmap。30Hz 對打字顯示足夠平滑,且開銷遠低於 60Hz。
const REPAINT_TIMER_ID: usize = 0xAB_7C_5E_01;
const REPAINT_TIMER_MS: u32 = 33;
const USE_DDRAW_REPAINT_TIMER: bool = false;
const USE_DDRAW_PAINT_OVERRIDE: bool = true;
const USE_DDRAW_ERASE_BACKGROUND: bool = true;
const USE_DDRAW_EVENT_REPAINT: bool = true;
const WS_EX_COMPOSITED_STYLE: u32 = 0x0200_0000;

fn needs_post_create_composited(ex_style: u32) -> bool {
    let _ = ex_style;
    false
}

/// 已 subclass 的視窗:hwnd → 原 wndproc(32-bit:i32)
///
/// 32-bit Windows 上 SetWindowLongW/GetWindowLongW 用 i32(LONG),不是 LONG_PTR。
static SUBCLASSED: Mutex<Option<HashMap<isize, i32>>> = Mutex::new(None);
static GAME_WNDPROC: Mutex<i32> = Mutex::new(0);
static DIAG_SEEN: Mutex<Option<HashMap<isize, u64>>> = Mutex::new(None);
static EDIT_BG_BRUSH: Mutex<isize> = Mutex::new(0);
/// Phase 4:已套 repaint timer 的 hwnd 集合,值是 SetTimer 回傳 id(0 = 失敗)
static TIMER_ARMED: Mutex<Option<HashMap<isize, u32>>> = Mutex::new(None);
/// Phase 4:WM_TIMER 第一次/前幾次 fire 計數(per hwnd) — 用來確認 timer 真有跑
static TIMER_FIRE_COUNT: Mutex<Option<HashMap<isize, u32>>> = Mutex::new(None);

const EDIT_BG_COLOR: u32 = 0x00FFFFFF;
const EDIT_TEXT_COLOR: u32 = 0x00000000;

fn with_map<R>(f: impl FnOnce(&mut HashMap<isize, i32>) -> R) -> R {
    let mut g = SUBCLASSED.lock().unwrap();
    if g.is_none() {
        *g = Some(HashMap::new());
    }
    f(g.as_mut().unwrap())
}

fn with_timer_map<R>(f: impl FnOnce(&mut HashMap<isize, u32>) -> R) -> R {
    let mut g = TIMER_ARMED.lock().unwrap();
    if g.is_none() {
        *g = Some(HashMap::new());
    }
    f(g.as_mut().unwrap())
}

fn with_fire_map<R>(f: impl FnOnce(&mut HashMap<isize, u32>) -> R) -> R {
    let mut g = TIMER_FIRE_COUNT.lock().unwrap();
    if g.is_none() {
        *g = Some(HashMap::new());
    }
    f(g.as_mut().unwrap())
}

/// 給 create_hook 的 GetClassNameW lie 用 — 確認 hwnd 是被 subclass 過的
/// LUnicodeEdit(只有 IME 路由要為它說謊;其他視窗照實回)
pub fn is_subclassed_lunicode_edit(hwnd: HWND) -> bool {
    let key = hwnd.0 as isize;
    with_map(|m| m.contains_key(&key))
}

/// 對遊戲視窗的所有 LUnicodeEdit 子視窗 subclass — 回 subclass 數量
fn with_diag_map<R>(f: impl FnOnce(&mut HashMap<isize, u64>) -> R) -> R {
    let mut g = DIAG_SEEN.lock().unwrap();
    if g.is_none() {
        *g = Some(HashMap::new());
    }
    f(g.as_mut().unwrap())
}

fn diag_msg_bit(msg: u32) -> Option<(u64, &'static str)> {
    match msg {
        WM_ERASEBKGND => Some((1 << 0, "WM_ERASEBKGND")),
        WM_PAINT => Some((1 << 1, "WM_PAINT")),
        WM_NCPAINT => Some((1 << 2, "WM_NCPAINT")),
        WM_CHAR => Some((1 << 3, "WM_CHAR")),
        WM_KEYUP => Some((1 << 4, "WM_KEYUP")),
        WM_SETTEXT => Some((1 << 5, "WM_SETTEXT")),
        WM_SETFOCUS => Some((1 << 6, "WM_SETFOCUS")),
        WM_KILLFOCUS => Some((1 << 7, "WM_KILLFOCUS")),
        WM_IME_COMPOSITION => Some((1 << 8, "WM_IME_COMPOSITION")),
        WM_IME_NOTIFY => Some((1 << 9, "WM_IME_NOTIFY")),
        WM_DESTROY => Some((1 << 10, "WM_DESTROY")),
        _ => None,
    }
}

unsafe fn diag_log_edit_msg(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, note: &str) {
    let Some((bit, name)) = diag_msg_bit(msg) else {
        return;
    };
    let key = hwnd.0 as isize;
    let first_seen = with_diag_map(|m| {
        let seen = m.entry(key).or_insert(0);
        if *seen & bit != 0 {
            false
        } else {
            *seen |= bit;
            true
        }
    });
    if !first_seen {
        return;
    }

    dbg_log!(
        "[ime-diag] edit msg {name} hwnd=0x{:X} wp=0x{:X} lp=0x{:X} {note} {}",
        key,
        wparam.0,
        lparam.0,
        describe_window(hwnd)
    );
}

unsafe fn diag_log_ctlcolor(parent: HWND, child: HWND, msg: u32, wparam: WPARAM) {
    let name = if msg == WM_CTLCOLOREDIT {
        "WM_CTLCOLOREDIT"
    } else {
        "WM_CTLCOLORSTATIC"
    };
    let key = child.0 as isize;
    let bit = if msg == WM_CTLCOLOREDIT {
        1 << 20
    } else {
        1 << 21
    };
    let first_seen = with_diag_map(|m| {
        let seen = m.entry(key).or_insert(0);
        if *seen & bit != 0 {
            false
        } else {
            *seen |= bit;
            true
        }
    });
    if first_seen {
        dbg_log!(
            "[ime-diag] parent {name} parent=0x{:X} child=0x{:X} hdc=0x{:X} child_info={}",
            parent.0 as usize,
            child.0 as usize,
            wparam.0,
            describe_window(child)
        );
    }
}

unsafe fn describe_window(hwnd: HWND) -> String {
    let parent = GetParent(hwnd).unwrap_or(HWND(std::ptr::null_mut()));
    let visible = IsWindowVisible(hwnd).as_bool();
    let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
    let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;

    let mut win = RECT::default();
    let win_text = if GetWindowRect(hwnd, &mut win).is_ok() {
        format!(
            "win=({},{} {}x{})",
            win.left,
            win.top,
            win.right.saturating_sub(win.left),
            win.bottom.saturating_sub(win.top)
        )
    } else {
        "win=<err>".to_string()
    };

    let mut client = RECT::default();
    let client_text = if GetClientRect(hwnd, &mut client).is_ok() {
        format!(
            "client={}x{}",
            client.right.saturating_sub(client.left),
            client.bottom.saturating_sub(client.top)
        )
    } else {
        "client=<err>".to_string()
    };

    format!(
        "parent=0x{:X} visible={} style=0x{:08X} ex=0x{:08X} {win_text} {client_text}",
        parent.0 as usize, visible, style, ex_style
    )
}

pub unsafe fn subclass_all_existing(game_hwnd: HWND) -> usize {
    subclass_game_window(game_hwnd);
    let _ = EnumChildWindows(Some(game_hwnd), Some(enum_child_proc), LPARAM(0));
    with_map(|m| m.len())
}

unsafe fn subclass_game_window(hwnd: HWND) {
    let mut orig_slot = GAME_WNDPROC.lock().unwrap();
    if *orig_slot != 0 {
        return;
    }
    let orig = GetWindowLongPtrW(hwnd, GWLP_WNDPROC);
    if orig == 0 {
        dbg_log!(
            "[ime] subclass_game_window FAIL hwnd=0x{:X} (GetWindowLongPtrW=0)",
            hwnd.0 as usize
        );
        return;
    }
    let new_proc = game_wndproc as *const () as usize as i32;
    SetWindowLongPtrW(hwnd, GWLP_WNDPROC, new_proc);
    *orig_slot = orig;
    dbg_log!(
        "[ime] subclassed game hwnd=0x{:X} orig_wndproc=0x{:X} {}",
        hwnd.0 as usize,
        orig as u32,
        describe_window(hwnd)
    );
}

extern "system" fn enum_child_proc(hwnd: HWND, _lp: LPARAM) -> windows::Win32::Foundation::BOOL {
    unsafe {
        if class_is_target(hwnd) {
            subclass_window(hwnd);
        }
    }
    windows::Win32::Foundation::BOOL(1)
}

unsafe fn class_is_target(hwnd: HWND) -> bool {
    let mut buf = [0u16; 64];
    let n = GetClassNameW(hwnd, &mut buf);
    if n <= 0 {
        return false;
    }
    let name = String::from_utf16_lossy(&buf[..n as usize]);
    name == TARGET_CLASS
}

unsafe fn subclass_window(hwnd: HWND) {
    let key = hwnd.0 as isize;
    let already = with_map(|m| m.contains_key(&key));
    if already {
        return;
    }

    let orig = GetWindowLongPtrW(hwnd, GWLP_WNDPROC);
    if orig == 0 {
        dbg_log!(
            "[ime] subclass_window FAIL hwnd=0x{:X} (GetWindowLongPtrW=0)",
            key
        );
        return;
    }

    // 32-bit:wndproc 是 i32 範圍。subclass_wndproc 在 i686 也是 32-bit pointer。
    let new_proc = subclass_wndproc as *const () as usize as i32;
    SetWindowLongPtrW(hwnd, GWLP_WNDPROC, new_proc);

    with_map(|m| {
        m.insert(key, orig);
    });
    // Phase 4:不再套 WS_EX_COMPOSITED。改用 SetTimer 30Hz 從 DDraw capture 灌背景。
    // SetTimer 不在這裡叫 — 此處是 worker thread 的 EVENT_OBJECT_FOCUS callback,
    // hwnd 屬於 UI thread。改在 subclass_wndproc 第一次 fire 時 lazy-arm,
    // 那一定是 hwnd 自己的 owning thread。
    // apply_double_buffer_post(hwnd);
    let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
    if needs_post_create_composited(ex_style) {
        apply_double_buffer_post(hwnd);
    }
    dbg_log!(
        "[ime] subclassed LUnicodeEdit hwnd=0x{:X} orig_wndproc=0x{:X} {}",
        key,
        orig as u32,
        describe_window(hwnd)
    );
}

/// 確保此 hwnd 已套 repaint timer。**必須在 hwnd 的 owning thread 呼叫**
/// (subclass_wndproc 一定符合)。
unsafe fn ensure_repaint_timer(hwnd: HWND) {
    let key = hwnd.0 as isize;
    let need = with_timer_map(|m| !m.contains_key(&key));
    if !need {
        return;
    }
    let id = SetTimer(Some(hwnd), REPAINT_TIMER_ID, REPAINT_TIMER_MS, None);
    with_timer_map(|m| {
        m.insert(key, id as u32);
    });
    dbg_log!(
        "[ime] repaint timer armed hwnd=0x{:X} id={} (lazy on first msg)",
        key,
        id as u32
    );
}

/// Phase 4:timer callback — GetDC(LUnicodeEdit) + paint_with_ddraw_capture + ReleaseDC。
///
/// LUnicodeEdit 是 DDraw 自繪 control,不會主動發 WM_PAINT / WM_ERASEBKGND,DWM
/// 又因為 redirection 把 child 區看成「空」→ 不套 WS_EX_COMPOSITED 就黑框。我們
/// 用 timer 主動把 DDraw capture 對應區塊推進 child 的 redirection bitmap,等效
/// 於 COMPOSITED 但不需要進 DWM bottom-up 合成 path → 跨第二螢幕不凍。
///
/// 開銷:33ms 一次 433x18 BitBlt = 約 31KB/cycle = 完全可忽略
unsafe fn blit_capture_to_window(hwnd: HWND) {
    let hdc = GetDC(Some(hwnd));
    if hdc.0.is_null() {
        return;
    }
    let _ = paint_edit_over_capture(hwnd, hdc);
    ReleaseDC(Some(hwnd), hdc);
}

/// 跟 `blit_capture_to_window` 一樣,但回傳是否真的 paint 成功(供 log 用)
unsafe fn blit_capture_to_window_logged(hwnd: HWND) -> bool {
    let hdc = GetDC(Some(hwnd));
    if hdc.0.is_null() {
        return false;
    }
    let ok = paint_edit_over_capture(hwnd, hdc);
    ReleaseDC(Some(hwnd), hdc);
    ok
}

unsafe fn paint_edit_over_capture(hwnd: HWND, target_hdc: HDC) -> bool {
    let mut rc = RECT::default();
    if GetClientRect(hwnd, &mut rc).is_err() {
        return false;
    }
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    if w <= 0 || h <= 0 {
        return false;
    }

    let mem_dc = CreateCompatibleDC(Some(target_hdc));
    if mem_dc.0.is_null() {
        return false;
    }
    let mem_bmp = CreateCompatibleBitmap(target_hdc, w, h);
    if mem_bmp.0.is_null() {
        let _ = DeleteDC(mem_dc);
        return false;
    }
    let old_bmp = SelectObject(mem_dc, mem_bmp.into());

    let painted_bg = paint_with_ddraw_capture(hwnd, mem_dc);
    if painted_bg {
        print_edit_client(hwnd, mem_dc);
        let _ = BitBlt(target_hdc, 0, 0, w, h, Some(mem_dc), 0, 0, SRCCOPY);
    }

    SelectObject(mem_dc, old_bmp);
    let _ = DeleteObject(HGDIOBJ(mem_bmp.0));
    let _ = DeleteDC(mem_dc);
    painted_bg
}

unsafe fn print_edit_client(hwnd: HWND, hdc: HDC) {
    let key = hwnd.0 as isize;
    let orig: i32 = with_map(|m| m.get(&key).copied()).unwrap_or(0);
    if orig == 0 {
        return;
    }
    let proc_ptr: extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
        std::mem::transmute(orig as usize);
    CallWindowProcW(
        Some(proc_ptr),
        hwnd,
        WM_PRINTCLIENT,
        WPARAM(hdc.0 as usize),
        LPARAM(EDIT_PRINT_FLAGS as isize),
    );
}

/// Phase 4:WM_ERASEBKGND 從 DDraw capture 取 LUnicodeEdit 對應區塊的遊戲畫面像素
/// BitBlt 進來,做為 EDIT 控件的背景。
///
/// 為什麼這比 WS_EX_COMPOSITED 好:
///   - 不進 DWM bottom-up 合成 path → 跨第二螢幕不會卡住整個遊戲畫面
///   - LUnicodeEdit 區域顯示「真正當下的遊戲背景」(透過 DDraw primary surface capture)
///     而不是 DWM 認為的「空」(黑)或單色 brush
///
/// 流程:
///   1. GetAncestor(GA_ROOT) 拿遊戲主視窗 hwnd
///   2. GetWindowRect 拿 LUnicodeEdit 螢幕座標 → ScreenToClient 轉成遊戲 client 座標
///   3. with_capture_dc 拿 DDraw capture 的 memory DC(內含當前遊戲 primary surface 像素)
///   4. BitBlt 對應區塊到 EDIT 的 hdc
///
/// 回傳 true 表示已成功填背景(caller return LRESULT(1));false 表示 capture 未就緒
/// 或邊界檢查 fail,讓原 wndproc 跑預設處理。
unsafe fn paint_with_ddraw_capture(hwnd: HWND, hdc: HDC) -> bool {
    if hdc.0.is_null() {
        return false;
    }

    let game_root = GetAncestor(hwnd, GA_ROOT);
    if game_root.0.is_null() {
        return false;
    }

    let mut edit_screen = RECT::default();
    if GetWindowRect(hwnd, &mut edit_screen).is_err() {
        return false;
    }
    let edit_w = edit_screen.right - edit_screen.left;
    let edit_h = edit_screen.bottom - edit_screen.top;
    if edit_w <= 0 || edit_h <= 0 {
        return false;
    }

    let mut topleft = POINT {
        x: edit_screen.left,
        y: edit_screen.top,
    };
    let ok = ScreenToClient(game_root, &mut topleft);
    if !ok.as_bool() {
        return false;
    }

    crate::ddraw_hook::with_capture_dc(|cap_dc, cap_w, cap_h| {
        // 邊界檢查:edit 必須完全落在 capture 範圍內,否則放棄
        if topleft.x < 0
            || topleft.y < 0
            || topleft.x + edit_w > cap_w
            || topleft.y + edit_h > cap_h
        {
            return false;
        }
        BitBlt(
            hdc,
            0,
            0,
            edit_w,
            edit_h,
            Some(cap_dc),
            topleft.x,
            topleft.y,
            SRCCOPY,
        )
        .is_ok()
    })
    .unwrap_or(false)
}

/// post-creation 套 WS_EX_COMPOSITED 雙緩衝。
///
/// 已驗過走不通的替代方案(2026-05-12):
///   - WS_EX_LAYERED + LWA_COLORKEY:DWM 不會把 DDraw redirection bitmap 顯示
///     在 child window 區域下方,colorkey 透出的是空(=黑),三項皆失敗
///   - 拿掉所有 ex-style 修改:第二螢幕凍結照樣發生 → 凍結是 Win11 native
///     DDraw 跨 adapter 的固有問題,跟我們的 patch 無關
///
/// 結論:WS_EX_COMPOSITED 是目前唯一可行;第二螢幕凍結是 Win11 限制,接受。
unsafe fn apply_double_buffer_post(hwnd: HWND) {
    let composited = WS_EX_COMPOSITED_STYLE as i32;
    let cur = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    if cur & composited != 0 {
        return;
    }
    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, cur | composited);
    let _ = SetWindowPos(
        hwnd,
        None,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
    );
    dbg_log!(
        "[ime] WS_EX_COMPOSITED applied (post-create) hwnd=0x{:X} ex 0x{:08X} → 0x{:08X}",
        hwnd.0 as usize,
        cur as u32,
        (cur | composited) as u32
    );
}

extern "system" fn subclass_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        // Phase 4:Lazy-arm repaint timer — 一定在 hwnd 自己的 owning thread context
        // (cross-thread SetTimer 行為不可靠)。第一次有任何訊息進來就 arm。
        if USE_DDRAW_REPAINT_TIMER {
            ensure_repaint_timer(hwnd);
        }

        // 記住最後拿到 focus 的 LUnicodeEdit — TSF callback 用來定位 overlay
        if msg == WM_SETFOCUS {
            crate::tsf_sink::set_last_focus_edit(hwnd);
        }

        if USE_DDRAW_ERASE_BACKGROUND && msg == WM_ERASEBKGND {
            let hdc = HDC(wparam.0 as *mut _);
            if paint_with_ddraw_capture(hwnd, hdc) {
                return LRESULT(1);
            }
        }

        // Phase 4:WM_TIMER 驅動的「灌底圖」— LUnicodeEdit 是 DDraw 自繪 control,
        // 從不發 WM_ERASEBKGND/WM_PAINT。改用 SetTimer 每 33ms 主動 GetDC + BitBlt
        // capture → LUnicodeEdit hdc,把當前遊戲 primary surface 的對應區塊推進
        // DWM child redirection bitmap → 等效於 WS_EX_COMPOSITED 但不進 DWM bottom-up
        // 合成 path → 跨螢幕不凍。
        if USE_DDRAW_REPAINT_TIMER && msg == WM_TIMER && wparam.0 == REPAINT_TIMER_ID {
            // 前 3 次 fire 紀錄 + 之後每 300 次 (~10s) 紀錄一次,確認 timer 還活著
            let key = hwnd.0 as isize;
            let n = with_fire_map(|m| {
                let c = m.entry(key).or_insert(0);
                *c = c.saturating_add(1);
                *c
            });
            if n <= 3 || n % 300 == 0 {
                let painted = blit_capture_to_window_logged(hwnd);
                dbg_log!(
                    "[ime] WM_TIMER fire #{n} hwnd=0x{:X} painted={}",
                    key,
                    painted
                );
            } else {
                blit_capture_to_window(hwnd);
            }
            return LRESULT(0);
        }

        // 視窗 destroy 時關 timer(避免 timer fire 到已釋放的 hwnd)
        if USE_DDRAW_PAINT_OVERRIDE && msg == WM_PAINT {
            return paint_double_buffered_over_capture(hwnd);
        }

        if msg == WM_DESTROY {
            let _ = KillTimer(Some(hwnd), REPAINT_TIMER_ID);
        }

        diag_log_edit_msg(hwnd, msg, wparam, lparam, "before_orig");

        // 額外:IME 系列訊息**每筆都記**(不走 dedup),用來確認 IMM32 有沒有
        // 真的送 IMN_OPENCANDIDATE(wp=5)/CHANGECANDIDATE(wp=3)/CLOSECANDIDATE(wp=4)。
        // diag_log_edit_msg 每個 msg 只記一次,看不到完整 IME 訊息流。
        if matches!(
            msg,
            WM_IME_NOTIFY
                | WM_IME_COMPOSITION
                | WM_IME_STARTCOMPOSITION
                | WM_IME_ENDCOMPOSITION
                | WM_IME_CHAR
                | WM_IME_SETCONTEXT
        ) {
            let name = match msg {
                WM_IME_NOTIFY => "WM_IME_NOTIFY",
                WM_IME_COMPOSITION => "WM_IME_COMPOSITION",
                WM_IME_STARTCOMPOSITION => "WM_IME_STARTCOMPOSITION",
                WM_IME_ENDCOMPOSITION => "WM_IME_ENDCOMPOSITION",
                WM_IME_CHAR => "WM_IME_CHAR",
                WM_IME_SETCONTEXT => "WM_IME_SETCONTEXT",
                _ => "?",
            };
            dbg_log!(
                "[ime-trace] {name} hwnd=0x{:X} wp=0x{:X} lp=0x{:X}",
                hwnd.0 as usize,
                wparam.0,
                lparam.0
            );
        }

        // === IMM32 default IME UI 路由補洞 ===
        //
        // LUnicodeEdit 自己吃掉 IME 訊息不呼叫 DefWindowProc,IMM32 default
        // IME window(class "IME")永遠收不到 IMN_OPENCANDIDATE → 候選視窗
        // 不開。我們在原 wndproc 之**前**先手動 DefWindowProcW 一次補通知。
        //
        // **不能轉**的是 `WM_IME_COMPOSITION(GCS_RESULTSTR)` 跟 `WM_IME_CHAR`
        // — DefWindowProc 對這兩個會自動轉成 WM_CHAR 送一份,加上原 wndproc
        // 自己會吃結果字插入,結果同一個字被插 2~3 次。
        //
        // 只轉純通知類:NOTIFY / STARTCOMPOSITION / ENDCOMPOSITION / SETCONTEXT。
        if is_ime_notify_message(msg) {
            DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        // === 攔截策略(實驗 A 擴大版)===
        //
        // 攔住**所有** WM_IME_NOTIFY 不傳給原 wndproc。Lineage 對 IME 私有
        // 通知會強制 commit;對標準 IMN_OPEN/CHANGE/CLOSECANDIDATE 也可能
        // 偷偷把 IMM32 default IME window 隱藏。一律不轉,讓 IMM32 自己管。
        // 我們已經提前 DefWindowProcW 通知了,IMM32 state 會正確。
        let block_original_for_ime_notify = msg == WM_IME_NOTIFY;

        // === 自繪 overlay 驅動 ===
        // OPENCANDIDATE   → 拉狀態 + show_for
        // CHANGECANDIDATE → 更新狀態 + InvalidateRect
        // CLOSECANDIDATE  → hide
        if msg == WM_IME_NOTIFY {
            match wparam.0 {
                IMN_OPENCANDIDATE => {
                    if let Some(state) = fetch_ime_state(hwnd) {
                        overlay::show_for(hwnd, state);
                    }
                }
                IMN_CHANGECANDIDATE => {
                    if let Some(state) = fetch_ime_state(hwnd) {
                        overlay::update(state);
                    }
                }
                IMN_CLOSECANDIDATE => {
                    overlay::hide();
                }
                _ => {}
            }
        }
        // WM_IME_COMPOSITION 組字字串變更 — 候選 overlay 已開的話也更新
        if msg == WM_IME_COMPOSITION
            && (lparam.0 as u32) & GCS_COMPSTR != 0
            && overlay::is_visible()
        {
            if let Some(state) = fetch_ime_state(hwnd) {
                overlay::update(state);
            }
        }
        // 失焦 / destroy 時隱藏 overlay,避免殘影
        if msg == WM_KILLFOCUS || msg == WM_DESTROY {
            overlay::hide();
        }

        if msg == WM_IME_NOTIFY && wparam.0 == IMN_OPENCANDIDATE {
            rescue_imm32_candidate_window(hwnd);
        }

        // 呼叫原 LUnicodeEdit wndproc(讓遊戲拿到組字 / 結果字串)— 但
        // 所有 IME notify 都不傳(實驗 A 擴大)
        let key = hwnd.0 as isize;
        let orig: i32 = with_map(|m| m.get(&key).copied()).unwrap_or(0);
        let result = if block_original_for_ime_notify {
            LRESULT(0)
        } else if orig != 0 {
            let proc_ptr: extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
                std::mem::transmute(orig as usize);
            CallWindowProcW(Some(proc_ptr), hwnd, msg, wparam, lparam)
        } else {
            LRESULT(0)
        };

        if should_refresh_edit(msg) {
            if USE_DDRAW_EVENT_REPAINT {
                let painted = blit_capture_to_window_logged(hwnd);
                if !painted {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            } else {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }

        if false && should_refresh_edit(msg) {
            // 第三參數 false:只 invalidate 不 erase,避免觸發 WM_ERASEBKGND
            // 造成打字時整片白閃。EDIT 內部 WM_PAINT 會自己畫文字 + 透過
            // WM_CTLCOLOREDIT 拿到的 brush 畫背景,不會殘影。
            let _ = InvalidateRect(Some(hwnd), None, false);
        }

        result
    }
}

/// 純通知類 IME 訊息 — 轉給 DefWindowProcW 不會生額外 WM_CHAR。
fn is_ime_notify_message(msg: u32) -> bool {
    matches!(
        msg,
        WM_IME_STARTCOMPOSITION | WM_IME_ENDCOMPOSITION | WM_IME_NOTIFY | WM_IME_SETCONTEXT
    )
}

/// LUnicodeEdit 內部雙緩衝 paint — DDraw 在背景每 frame 重畫會把 EDIT 的位元抹掉
/// 變成黑框。我們:
///   1. BeginPaint 拿到目標 hdc
///   2. CreateCompatibleDC + Bitmap 蓋住整個 client 區
///   3. 先用 WM_CTLCOLOREDIT 拿 parent 給的 brush 填背景(沒拿到就用 COLOR_WINDOW)
///   4. SendMessageW WM_PRINTCLIENT 給原 EDIT wndproc → 它把文字畫到 mem DC
///   5. BitBlt 一次到 screen DC(原子操作,沒中間白/黑閃)
///   6. EndPaint
unsafe fn paint_double_buffered_over_capture(hwnd: HWND) -> LRESULT {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    if !hdc.0.is_null() {
        let _ = paint_edit_over_capture(hwnd, hdc);
    }
    let _ = EndPaint(hwnd, &ps);
    LRESULT(0)
}

unsafe fn paint_double_buffered(hwnd: HWND) -> LRESULT {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    if hdc.0.is_null() {
        return LRESULT(0);
    }

    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    if w <= 0 || h <= 0 {
        let _ = EndPaint(hwnd, &ps);
        return LRESULT(0);
    }

    let mem_dc = CreateCompatibleDC(Some(hdc));
    if mem_dc.0.is_null() {
        let _ = EndPaint(hwnd, &ps);
        return LRESULT(0);
    }
    let mem_bmp = CreateCompatibleBitmap(hdc, w, h);
    if mem_bmp.0.is_null() {
        let _ = DeleteDC(mem_dc);
        let _ = EndPaint(hwnd, &ps);
        return LRESULT(0);
    }
    let old_bmp = SelectObject(mem_dc, mem_bmp.into());

    // 填背景 — 先嘗試問 parent 要 WM_CTLCOLOREDIT brush
    let parent = GetParent(hwnd).unwrap_or(HWND(std::ptr::null_mut()));
    let mut filled = false;
    if !parent.0.is_null() {
        let brush_lr = SendMessageW(
            parent,
            WM_CTLCOLOREDIT,
            Some(WPARAM(mem_dc.0 as usize)),
            Some(LPARAM(hwnd.0 as isize)),
        );
        if brush_lr.0 != 0 {
            let brush = HBRUSH(brush_lr.0 as *mut _);
            let _ = FillRect(mem_dc, &rc, brush);
            filled = true;
        }
    }
    if !filled {
        let sys = GetSysColorBrush(COLOR_WINDOW);
        if !sys.0.is_null() {
            let _ = FillRect(mem_dc, &rc, sys);
        }
    }

    // 讓原 EDIT wndproc 畫到 mem DC — WM_PRINTCLIENT 是「請你印一份到我給的 DC」
    let key = hwnd.0 as isize;
    let orig: i32 = with_map(|m| m.get(&key).copied()).unwrap_or(0);
    if orig != 0 {
        let proc_ptr: extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
            std::mem::transmute(orig as usize);
        let flags = EDIT_PRINT_FLAGS as isize;
        CallWindowProcW(
            Some(proc_ptr),
            hwnd,
            WM_PRINTCLIENT,
            WPARAM(mem_dc.0 as usize),
            LPARAM(flags),
        );
    }

    // 一次性 BitBlt 到 screen DC
    let _ = BitBlt(hdc, 0, 0, w, h, Some(mem_dc), 0, 0, SRCCOPY);

    // cleanup
    SelectObject(mem_dc, old_bmp);
    let _ = DeleteObject(HGDIOBJ(mem_bmp.0));
    let _ = DeleteDC(mem_dc);
    let _ = EndPaint(hwnd, &ps);
    LRESULT(0)
}

/// IMN_OPENCANDIDATE 觸發次數計數 — 只在前 3 次做完整 enumeration,避免 log 爆掉
static OPEN_CANDIDATE_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// IMN_OPENCANDIDATE 時:列出 thread 裡所有 top-level 視窗 + process 內所有
/// IME-like 視窗,找出真正的候選 UI window(class 通常是 MSCTFIME UI /
/// IMECandidateXxx / Microsoft IME 之類)。**不再** 對 ImmGetDefaultIMEWnd
/// 結果做 ShowWindow — 那個是 thread-level message proxy(class "IME"),
/// 本來就 invisible 0x0,不是真正的候選 UI。
unsafe fn rescue_imm32_candidate_window(edit_hwnd: HWND) {
    let count = OPEN_CANDIDATE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if count > 3 {
        // 只前 3 次做 full enumeration
        return;
    }

    // 1. 設候選位置(讓 IME UI 知道往哪畫,有些 IME 沒收到這個會躲在 0,0)
    let himc = ImmGetContext(edit_hwnd);
    if !himc.is_invalid() {
        let mut rc = RECT::default();
        let _ = GetClientRect(edit_hwnd, &mut rc);
        let mut origin = POINT { x: 0, y: rc.bottom };
        let _ = windows::Win32::Graphics::Gdi::ClientToScreen(edit_hwnd, &mut origin);

        let comp = COMPOSITIONFORM {
            dwStyle: CFS_POINT,
            ptCurrentPos: POINT { x: 0, y: 0 },
            rcArea: RECT::default(),
        };
        let _ = ImmSetCompositionWindow(himc, &comp);
        let cand = CANDIDATEFORM {
            dwIndex: 0,
            dwStyle: CFS_CANDIDATEPOS,
            ptCurrentPos: POINT { x: 0, y: rc.bottom },
            rcArea: RECT::default(),
        };
        let _ = ImmSetCandidateWindow(himc, &cand);
        let _ = ImmReleaseContext(edit_hwnd, himc);
        dbg_log!(
            "[ime-enum] #{} OPENCANDIDATE edit=0x{:X} screen_origin=({},{}) ime_proxy=0x{:X}",
            count,
            edit_hwnd.0 as usize,
            origin.x,
            origin.y,
            ImmGetDefaultIMEWnd(edit_hwnd).0 as usize
        );
    }

    // 2. EnumThreadWindows — thread 裡所有 top-level windows
    let mut tid_pid: u32 = 0;
    let tid = GetWindowThreadProcessId(edit_hwnd, Some(&mut tid_pid));
    dbg_log!("[ime-enum] #{} thread tid={} pid={}", count, tid, tid_pid);
    let _ = EnumThreadWindows(tid, Some(enum_thread_log_proc), LPARAM(count as isize));

    // 3. EnumWindows — 全 system top-level windows,過濾同 process,找 IME UI
    let _ = EnumWindows(Some(enum_global_log_proc), LPARAM(tid_pid as isize));
}

extern "system" fn enum_thread_log_proc(
    hwnd: HWND,
    lp: LPARAM,
) -> windows::Win32::Foundation::BOOL {
    unsafe {
        let count = lp.0 as usize;
        let mut buf = [0u16; 128];
        let n = GetClassNameW(hwnd, &mut buf);
        let class = if n > 0 {
            String::from_utf16_lossy(&buf[..n as usize])
        } else {
            "<?>".to_string()
        };
        let visible = IsWindowVisible(hwnd).as_bool();
        let mut rect = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rect);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let parent = GetParent(hwnd).unwrap_or(HWND(std::ptr::null_mut()));
        dbg_log!(
            "[ime-enum] #{} thread-wnd hwnd=0x{:X} class='{}' visible={} ex=0x{:08X} parent=0x{:X} rect=({},{} {}x{})",
            count,
            hwnd.0 as usize,
            class,
            visible,
            ex,
            parent.0 as usize,
            rect.left,
            rect.top,
            rect.right.saturating_sub(rect.left),
            rect.bottom.saturating_sub(rect.top)
        );
    }
    windows::Win32::Foundation::BOOL(1)
}

/// 全系統 top-level 列舉 — 只記錄屬於遊戲 process 的視窗(過濾掉系統其他 app),
/// 並進一步只記錄 class name 看起來像 IME UI 的(避免 log 爆量)。
extern "system" fn enum_global_log_proc(
    hwnd: HWND,
    lp: LPARAM,
) -> windows::Win32::Foundation::BOOL {
    unsafe {
        let target_pid = lp.0 as u32;
        let mut pid: u32 = 0;
        let _tid = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != target_pid {
            return windows::Win32::Foundation::BOOL(1);
        }
        let mut buf = [0u16; 128];
        let n = GetClassNameW(hwnd, &mut buf);
        let class = if n > 0 {
            String::from_utf16_lossy(&buf[..n as usize])
        } else {
            return windows::Win32::Foundation::BOOL(1);
        };
        // 篩 IME 相關 class — Microsoft IME / TSF / Bopomofo / Pinyin / Candidate
        let cl = class.to_ascii_lowercase();
        let interesting = cl.contains("ime")
            || cl.contains("candidate")
            || cl.contains("ctf")
            || cl.contains("tsf")
            || cl.contains("bopomofo")
            || cl.contains("pinyin")
            || cl.contains("reading")
            || cl.contains("composition")
            || cl.contains("chewing")
            || cl.contains("editcomposition")
            || cl.contains("phonetic")
            || cl.contains("hanyin")
            || cl.contains("natural");
        if !interesting {
            return windows::Win32::Foundation::BOOL(1);
        }
        let visible = IsWindowVisible(hwnd).as_bool();
        let mut rect = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rect);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let parent = GetParent(hwnd).unwrap_or(HWND(std::ptr::null_mut()));
        dbg_log!(
            "[ime-enum] global IME-like hwnd=0x{:X} class='{}' visible={} ex=0x{:08X} parent=0x{:X} rect=({},{} {}x{})",
            hwnd.0 as usize,
            class,
            visible,
            ex,
            parent.0 as usize,
            rect.left,
            rect.top,
            rect.right.saturating_sub(rect.left),
            rect.bottom.saturating_sub(rect.top)
        );
    }
    windows::Win32::Foundation::BOOL(1)
}

/// 遊戲 UI thread 的 TSF init 觸發訊息(從 worker thread `PostMessage` 過來)
pub const WM_TSF_INIT_ON_UI_THREAD: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;

extern "system" fn game_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        // 在遊戲 UI thread 上做 TSF init — TSF 是 per-thread,sink 必須註冊在
        // 實際打字 / 候選事件發生的那條 thread 才收得到
        if msg == WM_TSF_INIT_ON_UI_THREAD {
            let tid = windows::Win32::System::Threading::GetCurrentThreadId();
            match crate::tsf_sink::init() {
                Ok(_) => dbg_log!("[tsf] init on UI thread OK (tid={tid})"),
                Err(e) => dbg_log!("[tsf] init on UI thread FAIL: {:?}", e),
            }
            return LRESULT(0);
        }

        if msg == WM_CTLCOLOREDIT || msg == WM_CTLCOLORSTATIC {
            let child = HWND(lparam.0 as *mut _);
            if !child.0.is_null() && class_is_target(child) {
                diag_log_ctlcolor(hwnd, child, msg, wparam);
                let hdc = HDC(wparam.0 as *mut _);
                SetBkColor(hdc, COLORREF(EDIT_BG_COLOR));
                SetTextColor(hdc, COLORREF(EDIT_TEXT_COLOR));
                return LRESULT(edit_bg_brush().0 as isize);
            }
        }

        let orig = *GAME_WNDPROC.lock().unwrap();
        if orig != 0 {
            let proc_ptr: extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
                std::mem::transmute(orig as usize);
            CallWindowProcW(Some(proc_ptr), hwnd, msg, wparam, lparam)
        } else {
            LRESULT(0)
        }
    }
}

/// public: 給 lib.rs worker 用,subclass 遊戲主視窗
pub unsafe fn subclass_game_window_pub(hwnd: HWND) {
    subclass_game_window(hwnd);
}

unsafe fn erase_edit_background(hwnd: HWND, hdc: HDC) {
    if hdc.0.is_null() {
        return;
    }
    let mut rect = RECT::default();
    if GetClientRect(hwnd, &mut rect).is_ok() {
        FillRect(hdc, &rect, edit_bg_brush());
    }
}

unsafe fn edit_bg_brush() -> HBRUSH {
    let mut slot = EDIT_BG_BRUSH.lock().unwrap();
    if *slot == 0 {
        *slot = CreateSolidBrush(COLORREF(EDIT_BG_COLOR)).0 as isize;
    }
    HBRUSH(*slot as *mut _)
}

fn should_refresh_edit(msg: u32) -> bool {
    matches!(
        msg,
        WM_CHAR | WM_KEYUP | WM_SETTEXT | WM_SETFOCUS | WM_IME_COMPOSITION
    )
}

// ===== CreateWindow watcher (SetWinEventHook 監聽 OBJECT_CREATE) =====
//
// 攔截未來新建的 LUnicodeEdit(玩家點不同對話框時遊戲會建新 instance)。
// 用 WinEvent OBJECT_CREATE 比 inline hook user32!CreateWindowExW 簡單,不會踩 race。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_print_flags_keep_ddraw_background() {
        assert_ne!(EDIT_PRINT_FLAGS & PRF_CLIENT, 0);
        assert_eq!(EDIT_PRINT_FLAGS & PRF_ERASEBKGND, 0);
    }

    #[test]
    fn ddraw_mode_uses_paint_and_erase_without_timer() {
        assert!(!USE_DDRAW_REPAINT_TIMER);
        assert!(USE_DDRAW_PAINT_OVERRIDE);
        assert!(USE_DDRAW_ERASE_BACKGROUND);
    }

    #[test]
    fn ddraw_mode_repaints_input_events_immediately() {
        assert!(USE_DDRAW_EVENT_REPAINT);
    }

    #[test]
    fn ddraw_mode_never_applies_post_create_composited() {
        assert!(!needs_post_create_composited(0));
        assert!(!needs_post_create_composited(WS_EX_COMPOSITED_STYLE));
    }
}

static mut HOOK: HWINEVENTHOOK = HWINEVENTHOOK(std::ptr::null_mut());

pub unsafe fn install_create_watcher() {
    // 一次掛 OBJECT_CREATE..OBJECT_FOCUS 的整個 range — 0x8000~0x8005,
    // 涵蓋 CREATE / DESTROY / SHOW / HIDE / REORDER / FOCUS 6 種事件。
    // 我們真正在意的是:
    //   - OBJECT_CREATE:玩家在遊戲過程中新建 LUnicodeEdit(罕見但可能)
    //   - OBJECT_FOCUS:玩家點聊天框 → 既有 LUnicodeEdit 拿到 focus
    //     ← 主要靠這個事件抓到 init 階段就建好的 LUnicodeEdit,而且時機
    //     一定是「玩家正要打字」,絕對不會在 init 階段觸發
    //
    // 不能用 WINEVENT_SKIPOWNPROCESS — 我們是注入到目標 process,
    // 「自己的 process」就是遊戲 process。skip own 等於 skip 全部。
    HOOK = SetWinEventHook(
        EVENT_OBJECT_CREATE,
        EVENT_OBJECT_FOCUS,
        None,
        Some(win_event_proc),
        std::process::id(),
        0,
        WINEVENT_OUTOFCONTEXT,
    );
}

extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    // 只處理 top-level/window 物件(OBJID_WINDOW = 0)
    // CREATE 跟 FOCUS 兩種事件我們都要;其他事件忽略。
    let interesting = event == EVENT_OBJECT_CREATE || event == EVENT_OBJECT_FOCUS;
    if !interesting || id_object != 0 || hwnd.0.is_null() {
        return;
    }
    unsafe {
        if class_is_target(hwnd) {
            subclass_window(hwnd);
        }
    }
}
