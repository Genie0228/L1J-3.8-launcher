//! TSF `ITfUIElementSink` — 直接從 TSF 訂閱候選 UI 事件,跳過 IMM32
//!
//! 為什麼需要這個:
//!   Win11 的微軟新注音 / Chewing TSF 等 TSF text service,候選資料只跑 TSF
//!   callback 路徑,**不寫進 IMM32 候選 cache**(IMM32 compat 路徑只在 focus
//!   是 TSF-aware Edit 內建 MSCTFIME 橋接時才會幫忙倒資料 — LUnicodeEdit 自己刻
//!   的 EDIT 沒有橋,所以 `ImmGetCandidateListW` 永遠回 size=0)。
//!
//! 解法:在我們 process 的 TSF ThreadMgr 上 advise 一個 `ITfUIElementSink`,
//! TSF 會在 candidate UI 開/更新/關時 callback 我們,我們直接 QI 成
//! `ITfCandidateListUIElement` 讀字串,餵給 overlay 自繪。
//!
//! 這條路 **legacy IMM32 IME + TSF text service 兩邊都 work**(MS 內部 UI
//! 也是用同一條 sink 機制)。
//!
//! ## STA / 訊息迴圈
//!
//! TSF ThreadMgr 必須跑在 STA(`COINIT_APARTMENTTHREADED`)。我們的 worker
//! thread 已經有 `GetMessageW` 迴圈派發 `SetWinEventHook` 的 OUTOFCONTEXT
//! callback,順便就 pump COM 訊息 — 一條 thread 兩用。

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{implement, Interface, Result};
use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::TextServices::{
    ITfCandidateListUIElement, ITfSource, ITfThreadMgr, ITfUIElementMgr, ITfUIElementSink,
    ITfUIElementSink_Impl, CLSID_TF_ThreadMgr,
};

use crate::candidates::ImeState;
use crate::dbg_log;
use crate::overlay;

/// 最近一次 LUnicodeEdit 拿到 focus 的 hwnd — overlay 定位用
/// 用 atomic 因為這個會被別 thread (subclass thread = 遊戲 UI thread) set,
/// worker thread (TSF callback 跑這條) read。
static LAST_FOCUS_EDIT: AtomicIsize = AtomicIsize::new(0);

pub fn set_last_focus_edit(h: HWND) {
    LAST_FOCUS_EDIT.store(h.0 as isize, Ordering::Release);
}

fn last_focus_edit() -> HWND {
    HWND(LAST_FOCUS_EDIT.load(Ordering::Acquire) as *mut _)
}

// ===== Thread-local TSF 物件 =====
// COM interface 在 windows-rs 是 NonNull<c_void> wrapper,不是 Send/Sync。
// TSF ThreadMgr 又必須跑 STA(只能在 init 那條 thread access),所以用 thread_local
// 完美 — init 跟 sink callback 都在 worker thread,thread_local 能正常存取。
thread_local! {
    static UI_ELEMENT_MGR: RefCell<Option<ITfUIElementMgr>> = const { RefCell::new(None) };
    static THREAD_MGR: RefCell<Option<ITfThreadMgr>> = const { RefCell::new(None) };
}

#[implement(ITfUIElementSink)]
struct UIElementSink;

impl ITfUIElementSink_Impl for UIElementSink_Impl {
    fn BeginUIElement(&self, dw_ui_element_id: u32, pb_show: *mut BOOL) -> Result<()> {
        unsafe {
            // 我們自己畫 → 告訴 OS 不要顯示預設 candidate UI
            // (回 BOOL(0) = FALSE 表示「不要 show」,OS 會跳過內建渲染)
            if !pb_show.is_null() {
                *pb_show = BOOL(0);
            }
            handle_event(dw_ui_element_id, EventKind::Begin);
        }
        Ok(())
    }

    fn UpdateUIElement(&self, dw_ui_element_id: u32) -> Result<()> {
        unsafe {
            handle_event(dw_ui_element_id, EventKind::Update);
        }
        Ok(())
    }

    fn EndUIElement(&self, dw_ui_element_id: u32) -> Result<()> {
        unsafe {
            handle_event(dw_ui_element_id, EventKind::End);
        }
        Ok(())
    }
}

#[derive(Copy, Clone, PartialEq)]
enum EventKind {
    Begin,
    Update,
    End,
}

unsafe fn handle_event(id: u32, kind: EventKind) {
    if kind == EventKind::End {
        dbg_log!("[tsf] END id={id}");
        overlay::hide();
        return;
    }

    let elem = UI_ELEMENT_MGR.with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|mgr| mgr.GetUIElement(id).ok())
    });
    let Some(elem) = elem else {
        dbg_log!("[tsf] GetUIElement({id}) failed");
        return;
    };

    // 只關心 candidate list element
    let Ok(cand) = elem.cast::<ITfCandidateListUIElement>() else {
        // 可能是 reading info element 或別的(忽略)
        return;
    };

    let count = cand.GetCount().unwrap_or(0);
    if count == 0 {
        return;
    }
    let selection = cand.GetSelection().unwrap_or(0);
    let current_page = cand.GetCurrentPage().unwrap_or(0);

    // GetPageIndex(slice, *mut u32) — windows-rs 0.59 包成 slice + out param。
    // 預分配 32 個 entry(夠覆蓋大多數情況)。
    let mut pages = vec![0u32; 32];
    let mut got: u32 = 0;
    let _ = cand.GetPageIndex(&mut pages, &mut got);
    pages.truncate(got as usize);
    let page_start = pages.get(current_page as usize).copied().unwrap_or(0);
    let next_page_start = pages
        .get((current_page as usize) + 1)
        .copied()
        .unwrap_or(count);
    let page_size = next_page_start.saturating_sub(page_start);

    // 讀目前頁的字串(最多 9 個)
    let visible = (page_size as usize).min(9);
    let mut items = Vec::with_capacity(visible);
    for i in 0..visible {
        let idx = page_start + i as u32;
        if let Ok(bstr) = cand.GetString(idx) {
            items.push(bstr.to_string());
        } else {
            items.push(String::new());
        }
    }

    let state = ImeState {
        composition: String::new(),
        total: count,
        selection,
        page_start,
        page_size: visible as u32,
        items,
    };

    let focus = last_focus_edit();
    let label = match kind {
        EventKind::Begin => "BEGIN",
        EventKind::Update => "UPDATE",
        EventKind::End => "END",
    };
    dbg_log!(
        "[tsf] {label} id={id} count={count} sel={selection} page=({page_start},+{visible}) focus=0x{:X}",
        focus.0 as usize
    );
    if focus.0.is_null() {
        return;
    }

    if kind == EventKind::Begin {
        overlay::show_for(focus, state);
    } else {
        overlay::update(state);
    }
}

/// 在 worker thread 呼叫一次。會做:
///   1. CoInitializeEx STA
///   2. CoCreateInstance ThreadMgr
///   3. ThreadMgr.Activate
///   4. QI 成 ITfSource + ITfUIElementMgr
///   5. AdviseSink(ITfUIElementSink)
pub unsafe fn init() -> windows::core::Result<()> {
    let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    // S_OK / S_FALSE / RPC_E_CHANGED_MODE 都當成 OK — STA 已 init 過也能繼續
    if hr.is_err() && hr.0 != 0x80010106u32 as i32 {
        dbg_log!("[tsf] CoInitializeEx hr=0x{:08X}", hr.0 as u32);
        return Err(hr.into());
    }

    let thread_mgr: ITfThreadMgr =
        CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
    let client_id = thread_mgr.Activate()?;
    dbg_log!("[tsf] ThreadMgr activated client_id={client_id}");

    let source: ITfSource = thread_mgr.cast()?;
    let ui_mgr: ITfUIElementMgr = thread_mgr.cast()?;

    let sink: ITfUIElementSink = UIElementSink.into();
    let cookie = source.AdviseSink(&ITfUIElementSink::IID, &sink)?;

    UI_ELEMENT_MGR.with(|cell| *cell.borrow_mut() = Some(ui_mgr));
    THREAD_MGR.with(|cell| *cell.borrow_mut() = Some(thread_mgr));

    dbg_log!("[tsf] UIElementSink advised, cookie=0x{:X}", cookie);
    Ok(())
}
