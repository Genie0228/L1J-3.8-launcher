//! 純函式 layout 計算(toast / float 螢幕座標 + alpha)。

use std::time::{Duration, Instant};

use super::queue::{FLOAT_TTL, TOAST_TTL};
use super::types::{FloatKind, LiveFloat, LiveToast};

/// 單筆 layout 結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub x: i32,
    pub y: i32,
    pub alpha: u8, // 0~255
}

/// Toast fade-out 開始時間(過期前 0.5s 起漸隱)。
const TOAST_FADE_START: Duration = Duration::from_millis(4_500);
/// Float fade-out 開始時間(過期前 0.3s 起漸隱)。
const FLOAT_FADE_START: Duration = Duration::from_millis(1_200);
/// Float 上飄總像素。
const FLOAT_DRIFT_PX: i32 = 80;

pub fn toast_layout(
    t: &LiveToast,
    slot: usize,
    _screen_w: i32,
    screen_h: i32,
    now: Instant,
) -> Layout {
    let elapsed = now.duration_since(t.spawned_at);
    Layout {
        x: 10,
        y: screen_h - 50 - (slot as i32) * 36,
        alpha: fade_alpha(elapsed, TOAST_FADE_START, TOAST_TTL),
    }
}

pub fn float_layout(
    f: &LiveFloat,
    screen_w: i32,
    screen_h: i32,
    now: Instant,
) -> Layout {
    let elapsed = now.duration_since(f.spawned_at);
    let drift = ((elapsed.as_micros() * FLOAT_DRIFT_PX as u128) / FLOAT_TTL.as_micros()) as i32;
    let kind_y_offset = match f.kind {
        FloatKind::Exp => 30,
        FloatKind::Gold => 60,
    };
    // cascade_offset 把較新的項往下排(避免疊在仍在飄的舊項上)
    Layout {
        x: screen_w / 2 + 160,
        y: screen_h / 2 + kind_y_offset - drift + (f.cascade_offset as i32) * 25,
        alpha: fade_alpha(elapsed, FLOAT_FADE_START, FLOAT_TTL),
    }
}

fn fade_alpha(elapsed: Duration, fade_start: Duration, ttl: Duration) -> u8 {
    if elapsed >= ttl {
        return 0;
    }
    if elapsed < fade_start {
        return 255;
    }
    let fade_window = ttl - fade_start;
    let into_fade = elapsed - fade_start;
    let progress_q15 = (into_fade.as_micros() * 255) / fade_window.as_micros();
    255u32.saturating_sub(progress_q15 as u32) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_toast(spawned_at: Instant) -> LiveToast {
        LiveToast {
            spawned_at,
            gfxid: 1,
            text: vec![],
        }
    }

    fn dummy_float(spawned_at: Instant, kind: FloatKind, cascade: u8) -> LiveFloat {
        LiveFloat {
            spawned_at,
            kind,
            amount: 100,
            cascade_offset: cascade,
        }
    }

    #[test]
    fn toast_slot_0_at_bottom() {
        let now = Instant::now();
        let l = toast_layout(&dummy_toast(now), 0, 800, 600, now);
        assert_eq!(l.x, 10);
        assert_eq!(l.y, 600 - 50 - 0);
        assert_eq!(l.alpha, 255);
    }

    #[test]
    fn toast_slot_3_y_above() {
        let now = Instant::now();
        let l = toast_layout(&dummy_toast(now), 3, 800, 600, now);
        assert_eq!(l.y, 600 - 50 - 3 * 36);
    }

    #[test]
    fn toast_alpha_full_before_fade() {
        let now = Instant::now();
        let later = now + Duration::from_millis(4_499);
        let l = toast_layout(&dummy_toast(now), 0, 800, 600, later);
        assert_eq!(l.alpha, 255);
    }

    #[test]
    fn toast_alpha_zero_at_ttl() {
        let now = Instant::now();
        let later = now + TOAST_TTL;
        let l = toast_layout(&dummy_toast(now), 0, 800, 600, later);
        assert_eq!(l.alpha, 0);
    }

    #[test]
    fn toast_alpha_half_mid_fade() {
        let now = Instant::now();
        // fade window 4500ms~5000ms,中點 4750ms 應是 ~0.5 alpha
        let later = now + Duration::from_millis(4_750);
        let l = toast_layout(&dummy_toast(now), 0, 800, 600, later);
        // 允許 ±2 誤差(rounding)
        assert!((l.alpha as i32 - 127).abs() <= 2, "alpha={}", l.alpha);
    }

    #[test]
    fn float_exp_at_screen_right_of_center_above() {
        let now = Instant::now();
        let f = dummy_float(now, FloatKind::Exp, 0);
        let l = float_layout(&f, 800, 600, now);
        assert_eq!(l.x, 800 / 2 + 160);
        // EXP 在中央偏下(+30 offset),drift=0 since elapsed=0
        assert_eq!(l.y, 600 / 2 + 30);
        assert_eq!(l.alpha, 255);
    }

    #[test]
    fn float_gold_at_screen_below_exp() {
        let now = Instant::now();
        let f = dummy_float(now, FloatKind::Gold, 0);
        let l = float_layout(&f, 800, 600, now);
        // Gold 在 EXP 下方(+60 offset)
        assert_eq!(l.y, 600 / 2 + 60);
    }

    #[test]
    fn float_drifts_up_over_time() {
        let now = Instant::now();
        let f = dummy_float(now, FloatKind::Exp, 0);
        let l_start = float_layout(&f, 800, 600, now);
        let l_mid = float_layout(&f, 800, 600, now + Duration::from_millis(750));
        let l_end = float_layout(&f, 800, 600, now + FLOAT_TTL);
        // 起點 vs 中點:y 減少約 40px(drift 一半)
        assert_eq!(l_start.y - l_mid.y, 40);
        // 起點 vs 終點:y 減少 80px(drift 全部)
        assert_eq!(l_start.y - l_end.y, FLOAT_DRIFT_PX);
    }

    #[test]
    fn float_cascade_offset_lowers_y() {
        let now = Instant::now();
        let f0 = dummy_float(now, FloatKind::Exp, 0);
        let f3 = dummy_float(now, FloatKind::Exp, 3);
        let l0 = float_layout(&f0, 800, 600, now);
        let l3 = float_layout(&f3, 800, 600, now);
        // cascade=3 應比 cascade=0 多向下 75px
        assert_eq!(l0.y - l3.y, -3 * 25);
    }
}
