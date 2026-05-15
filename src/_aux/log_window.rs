//! LinHelperZ log 視窗 — 跟 LHX 主視窗一起顯示/隱藏
//!
//! 設計:
//! - NWG Window + 單一多行唯讀 TextBox(填滿視窗)
//! - 100ms timer drain `logger::subscribe()` 的 mpsc::Receiver,append 到 textbox
//! - visible flag 跟 LHX 共用同一個 [`AtomicU8`],HOME 鍵會同時切換兩者
//! - buffer 上限 100KB,超過從頭裁掉,避免長時間執行 OOM
//! - owner window 跟 LHX 一樣設為遊戲主視窗,LHX 隱藏時 log 視窗也隱藏

extern crate native_windows_derive as nwd;
extern crate native_windows_gui as nwg;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use nwd::NwgUi;
use nwg::NativeUi;

use crate::aux::lhx_window::{load_app_icon, VISIBLE_CLOSE, VISIBLE_HIDDEN, VISIBLE_SHOWN};
use crate::log_line;

/// log buffer 上限(超過從頭裁掉)
const LOG_BUF_LIMIT: usize = 100 * 1024;

#[derive(Default, NwgUi)]
pub struct LogWindow {
    #[nwg_control(
        size: (700, 400),
        position: (830, 200),
        title: "LinHelperZ Log",
        flags: "WINDOW|VISIBLE|MINIMIZE_BOX|RESIZABLE"
    )]
    #[nwg_events(OnWindowClose: [LogWindow::on_close])]
    window: nwg::Window,

    #[nwg_control(
        parent: window,
        position: (0, 0),
        size: (700, 400),
        flags: "VISIBLE|VSCROLL|AUTOVSCROLL",
        readonly: true
    )]
    log_view: nwg::TextBox,

    #[nwg_control(
        parent: window,
        interval: std::time::Duration::from_millis(100),
        active: false
    )]
    #[nwg_events(OnTimerTick: [LogWindow::on_drain_tick])]
    drain_timer: nwg::AnimationTimer,

    /// 共享 visible flag(跟 LHX 共用同一個)
    visible: Arc<AtomicU8>,

    /// log mpsc receiver,從 [`crate::logger::subscribe`] 取得
    log_rx: Mutex<Option<mpsc::Receiver<String>>>,

    /// 內部 buffer(append-only,超過 [`LOG_BUF_LIMIT`] 從頭裁掉)
    buffer: Mutex<String>,
}

impl LogWindow {
    fn on_close(&self) {
        // 等同隱藏:不 destroy,下次 HOME 重顯
        self.visible.store(VISIBLE_HIDDEN, Ordering::Relaxed);
        self.window.set_visible(false);
    }

    /// 100ms timer:drain mpsc + 同步 visible flag
    fn on_drain_tick(&self) {
        // 1. 同步 visible
        let v = self.visible.load(Ordering::Relaxed);
        let cur = self.window.visible();
        match v {
            VISIBLE_HIDDEN if cur => self.window.set_visible(false),
            VISIBLE_SHOWN if !cur => self.window.set_visible(true),
            VISIBLE_CLOSE => {
                self.drain_timer.stop();
                nwg::stop_thread_dispatch();
                return;
            }
            _ => {}
        }

        // 2. drain log channel(non-blocking)
        let mut new_lines = String::new();
        if let Ok(rx_guard) = self.log_rx.lock() {
            if let Some(rx) = rx_guard.as_ref() {
                while let Ok(msg) = rx.try_recv() {
                    new_lines.push_str(&msg);
                    new_lines.push_str("\r\n");
                }
            }
        }

        if new_lines.is_empty() {
            return;
        }

        // 3. append + 裁切 + 寫回 textbox
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_str(&new_lines);
            if buf.len() > LOG_BUF_LIMIT {
                let trim_to = buf.len() - LOG_BUF_LIMIT;
                // 裁到 char boundary(避免切到 multi-byte UTF-8 中間)
                let trim_at = buf
                    .char_indices()
                    .find(|(i, _)| *i >= trim_to)
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                buf.replace_range(..trim_at, "");
            }
            self.log_view.set_text(&buf);
        }

        // 4. 捲到最底 — set_selection 只挪 caret 不 scroll view,要送 EM_SCROLLCARET
        let len = self.log_view.text().len() as u32;
        self.log_view.set_selection(len..len);
        unsafe {
            use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
            use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
            const EM_SCROLLCARET: u32 = 0xB7;
            if let Some(raw_hwnd) = self.log_view.handle.hwnd() {
                let hwnd = HWND(raw_hwnd as *mut _);
                SendMessageW(hwnd, EM_SCROLLCARET, Some(WPARAM(0)), Some(LPARAM(0)));
            }
        }
    }
}

/// 啟動 log 視窗 thread。
///
/// `visible` 跟 LHX 共用同一個 `AtomicU8`,HOME 鍵切換時兩個視窗同步顯示/隱藏。
pub fn spawn_log_window_thread(visible: Arc<AtomicU8>) -> JoinHandle<()> {
    let visible_clone = visible.clone();

    std::thread::spawn(move || {
        if let Err(e) = nwg::init() {
            log_line!("[log-win] nwg init 失敗: {e:?}");
            return;
        }

        let mut font = nwg::Font::default();
        let _ = nwg::Font::builder()
            .family("Consolas")
            .size(14)
            .build(&mut font);
        nwg::Font::set_global_default(Some(font));

        let initial = LogWindow {
            visible: visible_clone,
            log_rx: Mutex::new(Some(crate::logger::subscribe())),
            ..Default::default()
        };
        let app = match LogWindow::build_ui(initial) {
            Ok(a) => a,
            Err(e) => {
                log_line!("[log-win] build_ui 失敗: {e:?}");
                return;
            }
        };
        if let Some(icon) = load_app_icon() {
            let icon = Box::leak(Box::new(icon));
            app.window.set_icon(Some(icon));
        }
        app.drain_timer.start();

        // 設遊戲為 owner,跟 LHX 行為一致
        unsafe {
            use windows::core::PCWSTR;
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{
                FindWindowW, SetWindowLongW, GWLP_HWNDPARENT,
            };
            let title: Vec<u16> = "Lineage Windows Client (13081901)\0"
                .encode_utf16()
                .collect();
            if let Ok(game_hwnd) = FindWindowW(PCWSTR::null(), PCWSTR(title.as_ptr())) {
                if !game_hwnd.is_invalid() {
                    if let Some(nwg_hwnd) = app.window.handle.hwnd() {
                        let log_hwnd = HWND(nwg_hwnd as *mut _);
                        SetWindowLongW(log_hwnd, GWLP_HWNDPARENT, game_hwnd.0 as i32);
                    }
                }
            }
        }

        nwg::dispatch_thread_events();
        log_line!("[log-win] thread 結束");
    })
}
