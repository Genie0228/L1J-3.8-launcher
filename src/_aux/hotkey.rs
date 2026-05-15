//! 全域 F1-F4 hotkey — `SetWindowsHookEx(WH_KEYBOARD_LL)` 攔截鍵盤
//!
//! 邏輯:
//! 1. LL hook callback 攔到 F1~F4 keydown,如果遊戲視窗在 foreground 就吃掉並
//!    把 idx (0..3) 推進 channel
//! 2. Worker thread 從 channel 拿到 idx,讀 `fkey_macros[idx].command`
//!    (字串格式 `name_id_suffix`,例 `行走加速_44_ME`),用 [`parse_buff_item`] 解析後 dispatch
//! 3. 物品(I)→ 找背包用 USE_ITEM;技能(S/Self_)→ spell_book lookup + cast_skill
//!
//! ## 為什麼要 worker thread
//! LL hook callback 在 OS input thread 跑,不能阻塞、不能 ReadProcessMemory(會 timeout
//! 直接被卸 hook)。callback 唯一動作是 push channel,實際 cast 在獨立 thread 跑。
//!
//! ## Focus 檢查
//! 只有遊戲視窗在 foreground 才觸發 — 否則 user 在 launcher GUI / Notepad / etc 按 F1
//! 也會中招。對比 GetForegroundWindow 的 PID 跟 game pid。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use windows::Win32::Foundation::{HANDLE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{VIRTUAL_KEY, VK_F1, VK_F2, VK_F3, VK_F4};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId,
    PeekMessageW, PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
    KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN,
};

use crate::aux::buff_dispatch::{execute_buff_item, DispatchCtx};
use crate::aux::drink_hook::DrinkHandle;
use crate::aux::lhx_window::parse_buff_item;
use crate::aux::runtime::AuxSettings;
use crate::aux::spell_book::SpellBook;
use crate::aux::spell_db::SpellDb;
use crate::log_line;

const HOTKEY_SHUTDOWN_POLL_MS: u64 = 50;

fn hotkey_shutdown_poll_timeout() -> Duration {
    Duration::from_millis(HOTKEY_SHUTDOWN_POLL_MS)
}

#[cfg(test)]
fn hook_message_pump_sleep_timeout() -> Duration {
    Duration::ZERO
}

/// channel sender — LL hook callback push F-key idx 進來
///
/// 為什麼用 `Mutex<Option<...>>` 不用 `OnceLock<...>`:LhxActiveSession 在換角時會
/// shutdown + 重啟,新 session 需要重新 install hotkey channel。OnceLock 一旦 set
/// 就再也不能 set,新 session 拿到 `set().is_err()` 會直接 bail,worker thread 跟
/// LL hook 全沒裝起來 → F1-F4 失靈。改用 Mutex 讓 install 能直接覆蓋舊 sender。
static HOTKEY_TX: Mutex<Option<Sender<usize>>> = Mutex::new(None);
/// 遊戲 process PID — focus 檢查用。同 process 重連 PID 不變,保留 OnceLock。
static GAME_PID: OnceLock<u32> = OnceLock::new();

/// 安裝 hotkey 系統 — 起 worker thread + LL hook thread。
pub fn install(
    h_process: HANDLE,
    target_pid: u32,
    settings: Arc<RwLock<AuxSettings>>,
    drink: Arc<RwLock<Option<Arc<DrinkHandle>>>>,
    spell_book: Arc<RwLock<Option<SpellBook>>>,
    spell_db: Arc<RwLock<Option<SpellDb>>>,
    cancel: Arc<AtomicBool>,
) -> Vec<JoinHandle<()>> {
    let (tx, rx) = channel::<usize>();
    // 覆蓋舊 sender — 上一個 session shutdown 後 worker thread 已退出,舊 sender
    // 已是孤兒。直接覆寫讓新 session 的 LL hook callback 能 send 到新 channel。
    *HOTKEY_TX.lock() = Some(tx);
    let _ = GAME_PID.set(target_pid);

    let mut handles = Vec::new();
    let h_raw = h_process.0 as usize;

    // Worker thread:處理 hotkey events
    let cancel_w = cancel.clone();
    handles.push(thread::spawn(move || {
        let h = HANDLE(h_raw as *mut _);
        // per-key cooldown 500ms — 防 key-repeat 連發
        let mut last_fire: [Option<Instant>; 4] = [None; 4];
        const COOLDOWN: Duration = Duration::from_millis(500);

        loop {
            if cancel_w.load(Ordering::Relaxed) {
                break;
            }
            let idx = match rx.recv_timeout(hotkey_shutdown_poll_timeout()) {
                Ok(idx) => idx,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            if idx >= 4 {
                continue;
            }
            if let Some(t) = last_fire[idx] {
                if t.elapsed() < COOLDOWN {
                    continue;
                }
            }

            let (enabled, cmd) = {
                let s = settings.read();
                (
                    s.fkey_macros[idx].enabled,
                    s.fkey_macros[idx].command.clone(),
                )
            };
            if !enabled || cmd.trim().is_empty() {
                continue;
            }
            last_fire[idx] = Some(Instant::now());
            fire_macro(h, &cmd, &drink, &spell_book, &spell_db, idx);
        }
        log_line!("[hotkey] worker thread 退出");
    }));

    // Hook thread:LL keyboard hook 必須有 message loop 否則 callback 不被叫
    let cancel_h = cancel.clone();
    let hook_thread_id = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let hook_thread_id_for_thread = hook_thread_id.clone();
    handles.push(thread::spawn(move || unsafe {
        hook_thread_id_for_thread.store(GetCurrentThreadId(), Ordering::Release);
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);

        let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_kb_proc), None, 0) {
            Ok(h) => h,
            Err(e) => {
                log_line!("[hotkey] SetWindowsHookExW 失敗: {e}");
                return;
            }
        };
        log_line!("[hotkey] WH_KEYBOARD_LL 安裝完成 — F1~F4 啟用");

        // Low-level keyboard hooks must be pumped immediately. A polling sleep here
        // delays every held key event, so shutdown is handled by posting WM_QUIT.
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 || cancel_h.load(Ordering::Relaxed) {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
        log_line!("[hotkey] WH_KEYBOARD_LL 卸載");
    }));

    let cancel_post = cancel.clone();
    handles.push(thread::spawn(move || {
        while !cancel_post.load(Ordering::Relaxed) {
            thread::sleep(hotkey_shutdown_poll_timeout());
        }

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let tid = hook_thread_id.load(Ordering::Acquire);
            if tid != 0 {
                unsafe {
                    let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
                }
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }));

    handles
}

unsafe extern "system" fn low_level_kb_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let msg_id = wparam.0 as u32;
        if msg_id == WM_KEYDOWN || msg_id == WM_SYSKEYDOWN {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let vk = VIRTUAL_KEY(kb.vkCode as u16);
            let idx: Option<usize> = match vk {
                VK_F1 => Some(0),
                VK_F2 => Some(1),
                VK_F3 => Some(2),
                VK_F4 => Some(3),
                _ => None,
            };
            if let Some(idx) = idx {
                if game_focused() {
                    // clone Sender 後立刻 release lock — LL hook callback 必須極速
                    // (parking_lot::Mutex fast path 不走 syscall;Sender::clone 是 Arc clone)
                    let tx_opt = HOTKEY_TX.lock().as_ref().cloned();
                    if let Some(tx) = tx_opt {
                        let _ = tx.send(idx);
                    }
                    // 吃掉 F-key — 避免遊戲也觸發其原生 F-key 行為(若有)
                    return LRESULT(1);
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// 比對前景視窗 PID 跟遊戲 PID。
unsafe fn game_focused() -> bool {
    let target_pid = match GAME_PID.get() {
        Some(p) => *p,
        None => return false,
    };
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return false;
    }
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(fg, Some(&mut pid));
    pid == target_pid
}

fn fire_macro(
    h: HANDLE,
    cmd: &str,
    drink: &Arc<RwLock<Option<Arc<DrinkHandle>>>>,
    spell_book: &Arc<RwLock<Option<SpellBook>>>,
    spell_db: &Arc<RwLock<Option<SpellDb>>>,
    idx: usize,
) {
    let dh = match drink.read().as_ref() {
        Some(h) => h.clone(),
        None => {
            log_line!("[hotkey] F{} 觸發但 DrinkHandle 未 ready", idx + 1);
            return;
        }
    };

    let bi = parse_buff_item(cmd);
    log_line!(
        "[hotkey] F{} 觸發 → name={:?} type={} target={:?}",
        idx + 1,
        bi.name,
        bi.item_type,
        bi.cast_target
    );

    // 技能路徑要 spell_book ready 且 cache 對得上當前角色 — 換角後 SPELL_BOOK_PTR
    // 會變,ensure_fresh 會偵測 stale 並 rebuild,避免拿到上一隻角色的 packed_skill_id。
    if bi.item_type == 'S' && !crate::aux::spell_book::ensure_fresh(h, spell_book, "hotkey") {
        return;
    }

    // 統一走 buff_dispatch::execute_buff_item — 跟 buff_tick / timer_tick 同 dispatch 邏輯,
    // 自動處理 IA/IW/I=name/IT/MIA/MIW 等所有後綴
    let ctx = DispatchCtx {
        h,
        dh: &dh,
        spell_book,
        spell_db,
    };
    let _ = execute_buff_item(&ctx, &bi);
}

#[cfg(test)]
mod tests {
    #[test]
    fn hotkey_shutdown_poll_timeout_is_bounded() {
        let timeout = super::hotkey_shutdown_poll_timeout();
        assert!(timeout > std::time::Duration::ZERO);
        assert!(timeout <= std::time::Duration::from_millis(100));
    }

    #[test]
    fn low_level_hook_message_pump_does_not_poll_sleep() {
        assert_eq!(
            super::hook_message_pump_sleep_timeout(),
            std::time::Duration::ZERO
        );
    }
}
