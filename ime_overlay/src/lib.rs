//! Lineage 3.8 自繪 IME 候選字視窗
//!
//! 為什麼要自繪:
//!   Win11 的微軟新注音(TSF/IMM32 compat)候選字視窗由 ctfmon.exe 渲染,需要遊戲 LUnicodeEdit
//!   在收到 WM_IME_NOTIFY(IMN_OPENCANDIDATE) 時呼叫 DefWindowProc。但 Lineage 3.8 的
//!   LUnicodeEdit 自己吃掉這個訊息不轉,所以候選字視窗永遠不開。第三方 IME(自然輸入法/新酷音)
//!   能 work 是因為它們自己畫候選視窗,不依賴遊戲配合。
//!
//! 我們的策略是抄第三方 IME 的做法:自己畫。
//!
//! 流程:
//!   1. launcher 注入此 DLL → DllMain spawn worker thread(避免 loader lock)
//!   2. worker thread:
//!      - 找遊戲主視窗(class "Lineage")
//!      - EnumChildWindows 找所有 LUnicodeEdit,subclass 它們的 wndproc
//!      - 也 inline hook user32!CreateWindowExW 攔未來新建的 LUnicodeEdit
//!      - 註冊我們的 overlay window class "LineageImeOverlay",建一個隱藏視窗
//!   3. LUnicodeEdit subclass 收到 WM_IME_NOTIFY:
//!      - wp=IMN_OPENCANDIDATE   → 讀候選 list, ShowWindow(overlay)
//!      - wp=IMN_CHANGECANDIDATE → 重讀候選 list, InvalidateRect 觸發重繪
//!      - wp=IMN_CLOSECANDIDATE  → ShowWindow(overlay, SW_HIDE)
//!   4. overlay WM_PAINT 用 GDI 畫:
//!      - 上方:組字字串(注音符號 ㄋㄧˇ ㄏㄠˇ)
//!      - 下方:候選字 9 個(對應數字鍵 1-9),選到的 highlight
//!
//! IME 內部選字邏輯(數字鍵/方向鍵/翻頁/space)由系統處理,我們只負責畫。

#![cfg(windows)]

mod candidates;
mod create_hook;
mod dbg;
mod ddraw_hook;
mod overlay;
mod subclass;
mod tsf_sink;

use std::sync::OnceLock;
use std::thread;

use windows::Win32::Foundation::{BOOL, HINSTANCE};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

static MODULE: OnceLock<usize> = OnceLock::new();

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn DllMain(hinst: HINSTANCE, reason: u32, _reserved: *mut u8) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        let _ = MODULE.set(hinst.0 as usize);
        // 避開 loader lock — 把所有 init 工作丟到 worker thread
        thread::spawn(|| {
            // 等遊戲主視窗建好(可能 launcher 注入時遊戲還沒 ResumeThread,主視窗未建)
            unsafe {
                worker();
            }
        });
    }
    BOOL(1)
}

unsafe fn worker() {
    dbg_log!("[ime] worker thread started, pid={}", std::process::id());

    // (1) 早期就裝 RegisterClassExA/W + CreateWindowExA/W hook — 確保所有
    //     LUnicodeEdit 註冊時剝 CS_OWNDC/CS_CLASSDC、建立時加 WS_EX_COMPOSITED。
    if let Err(e) = create_hook::install() {
        dbg_log!("[ime] FAIL: hook install: {e}");
        return;
    }
    dbg_log!("[ime] hooks installed (RegisterClassEx + CreateWindowEx)");

    // (2) 等遊戲主視窗出現
    let hwnd = match overlay::wait_for_game_window(180_000) {
        Some(h) => h,
        None => {
            dbg_log!("[ime] FAIL: game window not found in 180s");
            return;
        }
    };
    dbg_log!("[ime] game hwnd = {:?}", hwnd.0);

    // (3) **延遲 5 秒**讓遊戲 init 走完 — Deploy 4 在 init 期間立刻 subclass +
    //     SetWinEventHook 會破壞視窗化 init,推測是 race。
    //     **不**做 subclass_all_existing — 既有 LUnicodeEdit(登入帳密那種)
    //     保持原樣,避免動到 init 期間建的視窗 wndproc。
    std::thread::sleep(std::time::Duration::from_secs(5));
    dbg_log!("[ime] post-init delay done — installing OBJECT_CREATE watcher");

    // (4) SetWinEventHook 抓未來新建的 LUnicodeEdit。它的 callback 在這個
    //     thread 跑(WINEVENT_OUTOFCONTEXT),會 subclass + 轉發 IME 通知。
    subclass::install_create_watcher();
    dbg_log!("[ime] WinEventHook installed");

    // (4.5) 建自繪 overlay window — 註冊 class + CreateWindow 隱藏狀態。
    //       這個 thread 跑 GetMessage loop 順便 dispatch overlay 的 WM_PAINT。
    match overlay::create_overlay(hwnd) {
        Ok(h) => {
            overlay::set_overlay_hwnd(h);
            dbg_log!("[ime] overlay window ready");
        }
        Err(e) => {
            dbg_log!("[ime] FAIL overlay create: {e}");
        }
    }

    // (4.6) subclass 遊戲主視窗 + 觸發 TSF init **在遊戲 UI thread 上**。
    //
    // TSF 是 per-thread:sink 必須註冊在實際發生打字 / 候選事件的那條 thread,
    // 我們 worker 註冊只會收到 worker thread 的事件(我們不打字所以永遠 0 次)。
    // 解法:subclass 遊戲主視窗加 WM_APP+1 handler,從 worker `PostMessage`
    // 觸發,handler 在遊戲 UI thread 跑 → 那條 thread 的 ITfThreadMgr 才會
    // advise 我們的 sink。
    subclass::subclass_game_window_pub(hwnd);
    use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
    let _ = PostMessageW(
        Some(hwnd),
        subclass::WM_TSF_INIT_ON_UI_THREAD,
        windows::Win32::Foundation::WPARAM(0),
        windows::Win32::Foundation::LPARAM(0),
    );
    dbg_log!("[ime] posted WM_TSF_INIT to game UI thread");

    // (4.7) 安裝 DDraw Flip hook(Phase 2 診斷階段:純 log,不改畫面)
    match ddraw_hook::install() {
        Ok(()) => dbg_log!("[ddraw] install OK"),
        Err(e) => dbg_log!("[ddraw] install FAIL: {e}"),
    }

    // (5) thread message loop — SetWinEventHook OUTOFCONTEXT callback 派發。
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage, MSG,
    };
    let mut msg = MSG::default();
    loop {
        let r = GetMessageW(&mut msg, None, 0, 0);
        if r.0 <= 0 {
            break;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}
