//! 左下道具通知 + 畫面 EXP/金幣 飄字
//!
//! Spec: docs/superpowers/specs/2026-05-11-pickup-notification-design.md

pub mod dispatcher;
pub mod image_draw_hook;
pub mod layout;
pub mod overlay;
pub mod packet_hook;
pub mod queue;
pub mod renderer;
pub mod renderer_install;
pub mod sprite_pak;
pub mod tbt;
pub mod types;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use once_cell::sync::Lazy;
use windows::Win32::Foundation::HANDLE;

/// 計 polling tick 數,在 probe log 印出 — 跟 invocations 比,可判 wrapper 是否真的每 frame 跑。
static POLLING_TICKS: AtomicU32 = AtomicU32::new(0);
/// 計 update_list 失敗次數 — handle 死掉時會 spam,聚合成單 log。
static UPDATE_LIST_FAILS: AtomicU32 = AtomicU32::new(0);

/// 左下道具 toast 開關。launcher 啟動時依 AuxConfig.pickup_toast_enabled 設定。
/// 為什麼用 atomic 而非 lock:on_packet_recv 是高頻路徑(每個 packet 進來都查),
/// AtomicBool::load(Relaxed) 比 Mutex lock 快兩個數量級,而且這只是 hint flag,
/// 不需要強 ordering。
static TOAST_ENABLED: AtomicBool = AtomicBool::new(true);
/// 金幣 / 經驗值 飄字開關(EXP + Gold 共用一個 flag)。同上理由。
static FLOAT_ENABLED: AtomicBool = AtomicBool::new(true);

/// 由 launcher 啟動鏈呼叫,把編碼器 AuxConfig 的兩個 flag 設進來。
/// 任一 toggle 關掉時 packet_hook / overlay 仍 install,只是 on_packet_recv 把
/// 對應種類的 Notification 在進 queue 前就 drop — 反正 server 還是會送 packet。
pub fn set_enabled(pickup_toast: bool, exp_drift: bool) {
    TOAST_ENABLED.store(pickup_toast, Ordering::Relaxed);
    FLOAT_ENABLED.store(exp_drift, Ordering::Relaxed);
    log_line!(
        "[notification] toggle pickup_toast={} exp_drift={}",
        pickup_toast,
        exp_drift
    );
}

use crate::logger::log_line;
use queue::NotificationQueue;
use renderer::DrawCmd;

static QUEUE: Lazy<Mutex<NotificationQueue>> =
    Lazy::new(|| Mutex::new(NotificationQueue::new()));

/// 診斷:每 5s log 一次 hook hit counters(total + packetbox)。
/// `total_hits` 任何 opcode > 183 都 +1(判 hook 通);
/// `packetbox_hits` 只在 opcode == 250 +1(判 server 送 PACKETBOX)。
static LAST_PROBE: Lazy<Mutex<Option<(Instant, u32, u32)>>> = Lazy::new(|| Mutex::new(None));
const PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// renderer 安裝後的 cave addr — 若 None 表 install 失敗(packet path 還是會跑,只是看不到)。
static RENDERER: Lazy<Mutex<Option<renderer_install::RendererHandle>>> =
    Lazy::new(|| Mutex::new(None));

/// 新架構:image_draw_hook (0x42F450 hook,用 game 自己的 ImageElement_draw 畫 PNG)。
static IMAGE_DRAW: Lazy<Mutex<Option<image_draw_hook::ImageDrawHandle>>> =
    Lazy::new(|| Mutex::new(None));

/// 給 renderer_install diag 用(舊架構,已禁用)。
pub fn renderer_cave_addr() -> Option<u32> {
    RENDERER.lock().ok().and_then(|s| s.as_ref().map(|h| h.notif_draw_cave))
}

/// 主啟動鏈呼叫 — 裝 packet hook + renderer。
/// `game_dir` 用來掃 `Sprite*.idx` 建 PNG 索引(撿物 toast / EXP / Gold icon)。
/// renderer 安裝失敗不算致命(packet path 還能跑,只是看不到 toast/float)。
pub fn install(h: HANDLE, pid: u32, game_dir: &std::path::Path) -> Result<()> {
    packet_hook::install(h, pid)?;
    // Sprite.pak PNG 索引 — 失敗只 warning,overlay 退回色塊 placeholder。
    if let Err(e) = sprite_pak::init(game_dir) {
        log_line!("[notification] sprite_pak init 失敗(改用色塊 placeholder): {e:#}");
    }
    // TBT decoder LUT dump — 失敗只 warning,toast icon 退回空 1888 框。
    if let Err(e) = tbt::init(h) {
        log_line!("[notification] tbt init 失敗(toast 無 item icon): {e:#}");
    }
    // image_draw_hook (hook 0x42F450) 經 live test 確認不可行:
    //   invocations 在 ~10s 後完全凍住 (896),代表這個 vtable method 不是 in-world per-frame,
    //   只在登入/角選/特定 UI panel 才被叫。 進入世界後就沒了。
    // 改用 overlay window(launcher process 內透明置頂視窗):完全不碰遊戲記憶體,
    // 真 per-frame(我們自己 30ms tick),NPC dialog 不會閃退。
    if let Err(e) = overlay::ensure_running() {
        log_line!("[notification] overlay thread 啟動失敗(packet path 仍運作): {e:#}");
    }
    log_line!("[notification] install ok");
    let _ = pid;
    Ok(())
}

/// uninstall 對應 install。
pub fn uninstall(h: HANDLE, pid: u32) -> Result<()> {
    // packet_hook 的 handle 存在 module 內 — uninstall 從那邊讀。
    // 目前 launcher 沒有 hot uninstall 流程,留 stub。
    let _ = (h, pid);
    Ok(())
}

/// 對外 API:單元測試用 — 直接把 payload(從 sub_id byte 起)丟進 queue。
///
/// production 路徑也走這裡(on_polling_tick 從 packet_hook::drain 拿 payload 後呼叫)。
/// 依 [`set_enabled`] 設定的 toggle 決定是否真的進 queue:關掉的種類直接 drop,
/// 不浪費 queue 容量也不佔 overlay 渲染 slot。
pub fn on_packet_recv(payload: &[u8]) {
    let Some(n) = dispatcher::parse_packet_box(payload) else {
        return;
    };
    match &n {
        types::Notification::ToastBottomLeft { .. } => {
            if !TOAST_ENABLED.load(Ordering::Relaxed) {
                return;
            }
        }
        types::Notification::FloatingScreen { .. } => {
            if !FLOAT_ENABLED.load(Ordering::Relaxed) {
                return;
            }
        }
    }
    let mut q = lock_queue();
    q.push(n, Instant::now());
}

/// Polling thread 每 30ms 呼叫一次。
/// 1) drain ring(把 game-side shellcode 推進去的 packet 拉出來 dispatch)
/// 2) tick queue 清過期
/// 3) expand 成 DrawCmd 給渲染(Task 3.3b 接 codecave 後才被遊戲讀)
///
/// 一律 panic-safe — `catch_unwind` 包 整段,確保 polling thread 不被打掛。
pub fn on_polling_tick(
    h: HANDLE,
    now: Instant,
    screen_w: i32,
    screen_h: i32,
) -> Vec<DrawCmd> {
    std::panic::catch_unwind(|| {
        POLLING_TICKS.fetch_add(1, Ordering::Relaxed);
        // 0) 診斷探針(每 5s)
        probe_total_hits(h, now);
        // 1) 從 cave ring drain 出新 packet
        let payloads = packet_hook::drain(h);
        for p in &payloads {
            log_packet_diagnostic(p);
            on_packet_recv(p);
        }
        // 2) tick queue
        let mut q = lock_queue();
        q.tick(now);
        // 3) expand
        let toasts: Vec<_> = q.toasts.iter().cloned().collect();
        let floats: Vec<_> = q.floats.iter().cloned().collect();
        drop(q);
        // 3.5) TBT icon prefetch — 對每個 live toast 嘗試載入 icon
        // (cache hit 直接 return,miss 才跨 process 讀 + render;NULL pointer 不 cache,下次再試)
        for t in &toasts {
            tbt::ensure_icon(h, t.gfxid);
        }
        let cmds = renderer::expand_snapshot(&toasts, &floats, screen_w, screen_h, now);

        // 4) 把 toast/float snapshot 灌進 overlay thread。 overlay 自己每 30ms 醒一次 render。
        let toast_views: Vec<overlay::ToastView> = toasts
            .iter()
            .map(|t| overlay::ToastView {
                gfxid: t.gfxid,
                text: decode_packet_text(&t.text),
                age_ms: now.saturating_duration_since(t.spawned_at).as_millis() as u32,
            })
            .collect();
        let float_views: Vec<overlay::FloatView> = floats
            .iter()
            .map(|f| overlay::FloatView {
                kind: f.kind,
                amount: f.amount,
                age_ms: now.saturating_duration_since(f.spawned_at).as_millis() as u32,
                cascade_offset: f.cascade_offset,
            })
            .collect();
        overlay::write_snapshot(overlay::Snapshot {
            toasts: toast_views,
            floats: float_views,
            captured_at: Some(now),
        });

        cmds
    })
    .unwrap_or_else(|_| {
        log_line!("[notification] polling tick panicked,跳過此 frame");
        Vec::new()
    })
}

/// 每 5s 讀 cave counters,log 變化(判 hook 通 + server 送 PACKETBOX)。
fn probe_total_hits(h: HANDLE, now: Instant) {
    let Ok(mut state) = LAST_PROBE.lock() else { return };
    let total = packet_hook::read_total_hits(h);
    let pkt = packet_hook::read_packetbox_hits(h);
    match *state {
        None => {
            *state = Some((now, total, pkt));
            log_line!(
                "[notification] probe: total={total} packetbox={pkt} (baseline)"
            );
        }
        Some((last_at, last_total, last_pkt)) => {
            if now.duration_since(last_at) >= PROBE_INTERVAL {
                let d_total = total.wrapping_sub(last_total);
                let d_pkt = pkt.wrapping_sub(last_pkt);
                log_line!(
                    "[notification] probe: total={total} Δ={d_total} | packetbox={pkt} Δ={d_pkt} (5s)"
                );
                let ticks = POLLING_TICKS.load(Ordering::Relaxed);
                let fails = UPDATE_LIST_FAILS.load(Ordering::Relaxed);
                let cave = IMAGE_DRAW.lock().ok().and_then(|s| s.as_ref().map(|h| h.cave));
                if let Some(cave) = cave {
                    if let Some((inv, stolen, count)) = image_draw_hook::read_diag(h, cave) {
                        log_line!(
                            "[notification] render diag: invocations={inv} stolen={stolen} draw_count={count} polling_ticks={ticks} fails={fails}"
                        );
                    } else {
                        log_line!(
                            "[notification] render diag: read_diag 失敗 polling_ticks={ticks} fails={fails}"
                        );
                    }
                } else {
                    log_line!(
                        "[notification] render diag: image_draw_hook 未安裝 polling_ticks={ticks} fails={fails}"
                    );
                }
                // 有新增 hits 才印 opcode ring(避免一直 spam 同樣的 16 個值)
                if d_total > 0 {
                    let (ring, total_writes) = packet_hook::read_opcode_ring(h);
                    if !ring.is_empty() {
                        // 取最近 d_total(最多 16)個 — 從 (total_writes-d_total) mod 16 開始
                        let take = d_total.min(16);
                        let start = total_writes.wrapping_sub(d_total) as usize;
                        let recent: Vec<u8> = (0..take as usize)
                            .map(|i| ring[(start + i) & 0x0F])
                            .collect();
                        log_line!(
                            "[notification] recent opcodes (>183) seen: {:?}",
                            recent
                        );
                    }
                }
                *state = Some((now, total, pkt));
            }
        }
    }
}

/// Live verify 用的 diagnostic — 收到 packet 時印出來。
/// Task 3.4 完成後可降成 debug_log。
fn log_packet_diagnostic(payload: &[u8]) {
    if payload.is_empty() {
        return;
    }
    let sub_id = payload[0];
    match sub_id {
        packet_hook::SUB_ID_ITEMBOARD => {
            if payload.len() >= 3 {
                let gfxid = u16::from_le_bytes([payload[1], payload[2]]);
                let name_bytes = payload.len().saturating_sub(3 + 1);
                log_line!(
                    "[notification] S_ItemBoard recv: gfxid={gfxid} name_bytes={name_bytes}"
                );
            }
        }
        packet_hook::SUB_ID_SHOWDROP => {
            if payload.len() >= 6 {
                let kind = payload[1];
                let amount = u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]]);
                let kind_name = match kind {
                    0 => "EXP",
                    1 => "Gold",
                    _ => "UNKNOWN",
                };
                log_line!(
                    "[notification] S_ShowDrop recv: kind={kind_name} amount={amount}"
                );
            }
        }
        _ => {
            log_line!("[notification] 未知 sub_id {sub_id} payload {} bytes", payload.len());
        }
    }
}

/// 把 packet 內的物品名 bytes 轉成 UTF-8。
/// 依編碼器 GUI 設定的 `TextEncodingMode`(Big5/Gbk/Auto)走 `legacy_text` 的共用解碼路徑,
/// 簡中私服 → Gbk,台版 → Big5,Auto 由 heuristic 二選一。
fn decode_packet_text(bytes: &[u8]) -> String {
    crate::legacy_text::decode_text_with_mode(bytes, crate::legacy_text::text_encoding_mode()).0
}

fn lock_queue() -> std::sync::MutexGuard<'static, NotificationQueue> {
    QUEUE.lock().unwrap_or_else(|poisoned| {
        log_line!("[notification] queue mutex poisoned,重置");
        let mut g = poisoned.into_inner();
        *g = NotificationQueue::new();
        g
    })
}
