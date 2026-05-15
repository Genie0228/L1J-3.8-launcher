//! NotificationQueue 與生命週期管理。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::types::{FloatKind, LiveFloat, LiveToast, Notification};

pub const TOAST_TTL: Duration = Duration::from_millis(5_000);
pub const FLOAT_TTL: Duration = Duration::from_millis(1_500);
pub const TOAST_MAX: usize = 10;

#[derive(Debug, Default)]
pub struct NotificationQueue {
    pub(super) toasts: VecDeque<LiveToast>,
    pub(super) floats: VecDeque<LiveFloat>,
}

impl NotificationQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, n: Notification, now: Instant) {
        match n {
            Notification::ToastBottomLeft { gfxid, text } => {
                if self.toasts.len() >= TOAST_MAX {
                    self.toasts.pop_front();
                }
                self.toasts.push_back(LiveToast {
                    spawned_at: now,
                    gfxid,
                    text,
                });
            }
            Notification::FloatingScreen { kind, amount } => {
                // 累計:同 kind 已 active → merge amount + reset spawned_at,讓 float 保持新鮮。
                // 永遠最多 1 個 EXP + 1 個 Gold,避免快速連殺時疊一柱數字。
                if let Some(existing) = self.floats.iter_mut().find(|f| f.kind == kind) {
                    existing.amount = existing.amount.saturating_add(amount);
                    existing.spawned_at = now;
                } else {
                    self.floats.push_back(LiveFloat {
                        spawned_at: now,
                        kind,
                        amount,
                        cascade_offset: 0,
                    });
                }
            }
        }
    }

    pub fn tick(&mut self, now: Instant) {
        self.toasts
            .retain(|t| now.duration_since(t.spawned_at) < TOAST_TTL);
        self.floats
            .retain(|f| now.duration_since(f.spawned_at) < FLOAT_TTL);
    }

    pub fn toast_count(&self) -> usize { self.toasts.len() }
    pub fn float_count(&self) -> usize { self.floats.len() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn push_toast_appears() {
        let mut q = NotificationQueue::new();
        let now = t0();
        q.push(
            Notification::ToastBottomLeft {
                gfxid: 1,
                text: vec![b'X'],
            },
            now,
        );
        assert_eq!(q.toast_count(), 1);
    }

    #[test]
    fn toast_max_evicts_oldest() {
        let mut q = NotificationQueue::new();
        let now = t0();
        for i in 0..(TOAST_MAX + 5) {
            q.push(
                Notification::ToastBottomLeft {
                    gfxid: i as u16,
                    text: vec![],
                },
                now,
            );
        }
        assert_eq!(q.toast_count(), TOAST_MAX);
        // 最舊 5 個應已驅逐 → 剩 gfxid 5..15
        let first = q.toasts.front().unwrap();
        assert_eq!(first.gfxid, 5);
    }

    #[test]
    fn push_float_appears() {
        let mut q = NotificationQueue::new();
        q.push(
            Notification::FloatingScreen {
                kind: FloatKind::Exp,
                amount: 100,
            },
            t0(),
        );
        assert_eq!(q.float_count(), 1);
    }

    #[test]
    fn same_kind_floats_coalesce_into_one() {
        // 多次 push 同 kind → 永遠最多 1 個 float,amount 累加
        let mut q = NotificationQueue::new();
        let now = t0();
        for _ in 0..200 {
            q.push(
                Notification::FloatingScreen {
                    kind: FloatKind::Exp,
                    amount: 5,
                },
                now,
            );
        }
        assert_eq!(q.float_count(), 1);
        assert_eq!(q.floats[0].amount, 5 * 200);
    }

    #[test]
    fn exp_and_gold_are_separate_floats() {
        let mut q = NotificationQueue::new();
        let now = t0();
        q.push(
            Notification::FloatingScreen { kind: FloatKind::Exp, amount: 100 },
            now,
        );
        q.push(
            Notification::FloatingScreen { kind: FloatKind::Gold, amount: 50 },
            now,
        );
        assert_eq!(q.float_count(), 2);
    }

    #[test]
    fn coalesce_resets_spawned_at_to_keep_float_fresh() {
        // 快速連續 push → spawned_at 一直被刷新,避免動畫淡出
        let mut q = NotificationQueue::new();
        let t_a = t0();
        q.push(
            Notification::FloatingScreen { kind: FloatKind::Exp, amount: 10 },
            t_a,
        );
        let t_b = t_a + Duration::from_millis(800);
        q.push(
            Notification::FloatingScreen { kind: FloatKind::Exp, amount: 20 },
            t_b,
        );
        assert_eq!(q.floats[0].amount, 30);
        assert_eq!(q.floats[0].spawned_at, t_b);
    }

    #[test]
    fn tick_removes_expired_toasts() {
        let mut q = NotificationQueue::new();
        let now = t0();
        q.push(
            Notification::ToastBottomLeft {
                gfxid: 1,
                text: vec![],
            },
            now,
        );
        // 超過 TOAST_TTL 後應移除
        q.tick(now + TOAST_TTL + Duration::from_millis(1));
        assert_eq!(q.toast_count(), 0);
    }

    #[test]
    fn tick_removes_expired_floats() {
        let mut q = NotificationQueue::new();
        let now = t0();
        q.push(
            Notification::FloatingScreen {
                kind: FloatKind::Gold,
                amount: 1,
            },
            now,
        );
        q.tick(now + FLOAT_TTL + Duration::from_millis(1));
        assert_eq!(q.float_count(), 0);
    }
}
