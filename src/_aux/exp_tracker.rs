//! LHX 視窗顯示經驗值追蹤
//!
//! 為什麼這麼做(2026-05-02 snapshot diff + 跨 session 重驗 + 升級實測):
//!   - TOTAL EXP @ `0x00C31EA4` (u32, cumulative from level 1) ★★★★★
//!   - 當前 LEVEL = 從 total_exp 反推(threshold 表二分),**不依賴記憶體 level byte**
//!   - threshold 表為各等級累積經驗值的固定門檻(實測 10202 / 4641 = 4.35% 對得上)
//!
//! 走過的彎路(避免後人重蹈):
//! 1. 誤把 `[G_PLAYER_PTR]+0x04` 當作 level — 那其實是 entity 內 heap pointer。
//! 2. 改用 `0x00C31E7B` 一個穩定的 byte,跨 session 讀都是 0x0A。但**升級後它不更新**
//!    (推測是「初始等級」/ class base level),total_exp 跨入下一級時 within > range
//!    → to_levelup = 0、to_next_pct 錯位。
//! 3. 最後改用「從 total_exp 反推 level」 — 穩定、不需要 memory level field。

use anyhow::Result;
use windows::Win32::Foundation::HANDLE;

/// 等級 1~65 累積經驗值門檻表
///
/// `LEVEL_THRESHOLDS[n]` = 累積 EXP 達到等級 (n+1) 所需的總值。
/// 數值為遊戲內部寫死,3.8 client 與 server 同此表;反推等級時用二分搜尋。
const LEVEL_THRESHOLDS: [u64; 65] = [
    0, 125, 300, 500, 750, 1296, 2401, 4096, 6561, 10000, 14641, 20736, 28561, 38416, 50625, 65536,
    83521, 104976, 130321, 160000, 194481, 234256, 279841, 331776, 390625, 456976, 531441, 614656,
    707281, 810000, 923521, 1048576, 1185921, 1336336, 1500625, 1679616, 1874161, 2085136, 2313441,
    2560000, 2825761, 3111696, 3418801, 3748096, 4100625, 4829985, 6338401, 9833664, 19745853,
    31292598, 44473900, 59289759, 75740173, 93825145, 113544672, 134898756, 157887397, 182510594,
    208768347, 236660657, 266187523, 297348946, 330144925, 364575461, 400640553,
];

/// 等級 >65 每級固定 EXP range — 高等級採線性公式而非門檻表
const HIGH_RANGE: u64 = 36_065_092;

/// 等級 65 起點的 cumulative threshold(銜接門檻表與線性公式的接點)
const HIGH_BASE: u64 = 400_640_553;

/// 累積 EXP 達到 `level` 所需的閾值(該等級的起點)。
pub fn level_start_threshold(level: u32) -> u64 {
    if level <= 1 {
        0
    } else if (level as usize) <= LEVEL_THRESHOLDS.len() {
        LEVEL_THRESHOLDS[(level - 1) as usize]
    } else {
        HIGH_BASE + (level as u64 - 65) * HIGH_RANGE
    }
}

/// `level` 整級所需的 EXP range。
pub fn level_range(level: u32) -> u64 {
    if level == 0 {
        return 0;
    }
    let cur = level_start_threshold(level);
    let next = level_start_threshold(level + 1);
    next.saturating_sub(cur)
}

/// 從累積 EXP 反推當前等級(1~~)。
///
/// 演算法:找出最大的 `level` 使得 `level_start_threshold(level) <= total`。
/// 等級 65+ 走 HIGH_BASE/HIGH_RANGE 公式直接算。
pub fn level_from_total_exp(total: u64) -> u32 {
    if total >= HIGH_BASE {
        return 65 + ((total - HIGH_BASE) / HIGH_RANGE) as u32;
    }
    // total 在 LEVEL_THRESHOLDS 範圍內 → 線性掃從高到低(<=65 entries,實際成本忽略)
    for level in (1..=65u32).rev() {
        if total >= level_start_threshold(level) {
            return level;
        }
    }
    1
}

/// 「到下一個整數 % 還差多少 EXP」。
pub fn exp_to_next_pct(within: u64, range: u64) -> u64 {
    if range == 0 {
        return 0;
    }
    let pct_now = (within * 100) / range;
    let next_pct = pct_now + 1;
    let target = (range * next_pct + 99) / 100; // ceil(range * next_pct / 100)
    target.saturating_sub(within)
}

/// 一次 tick 計算結果(無副作用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickReport {
    pub level: u32,
    pub delta_kill: u64,
    pub to_next_pct: u64,
    pub to_levelup: u64,
    pub session_total: u64,
    pub leveled_up: bool,
}

/// 內部 EXP tracker(無副作用)。
#[derive(Default)]
pub struct ExpTracker {
    pub enabled: bool,
    last_total_exp: Option<u32>,
    last_level: Option<u32>,
    session_baseline: Option<u32>,
}

impl ExpTracker {
    /// 啟用追蹤 — 抓 baseline,不輸出。
    pub fn enable(&mut self, total_exp: u32) {
        let level = level_from_total_exp(total_exp as u64);
        self.enabled = true;
        self.session_baseline = Some(total_exp);
        self.last_total_exp = Some(total_exp);
        self.last_level = Some(level);
    }

    /// 停用追蹤 — 清狀態,下次 enable 重新抓 baseline。
    pub fn disable(&mut self) {
        self.enabled = false;
        self.last_total_exp = None;
        self.last_level = None;
        self.session_baseline = None;
    }

    /// 一次 polling tick — 不變動 → None。等級從 `total_exp` 反推,
    /// 不依賴遊戲記憶體 level field(那個 byte 升級後不更新)。
    pub fn tick(&mut self, total_exp: u32) -> Option<TickReport> {
        if !self.enabled {
            return None;
        }
        let last_total = self.last_total_exp?;
        let last_level = self.last_level?;
        let baseline = self.session_baseline?;

        if total_exp == last_total {
            return None;
        }

        let level = level_from_total_exp(total_exp as u64);
        let leveled_up = level != last_level;
        let delta_kill = (total_exp as u64).saturating_sub(last_total as u64);
        let range = level_range(level);
        let within = (total_exp as u64).saturating_sub(level_start_threshold(level));
        let to_next_pct = exp_to_next_pct(within, range);
        let to_levelup = range.saturating_sub(within);
        let session_total = (total_exp as u64).saturating_sub(baseline as u64);

        self.last_total_exp = Some(total_exp);
        self.last_level = Some(level);

        Some(TickReport {
            level,
            delta_kill,
            to_next_pct,
            to_levelup,
            session_total,
            leveled_up,
        })
    }
}

/// `0x00C31EA4` = total cumulative exp(u32 即足夠 — 滿等累積值未滿 2^32)
pub const G_TOTAL_EXP: u32 = 0x00C31EA4;

pub fn read_total_exp(h: HANDLE) -> Result<u32> {
    crate::memory::read_u32(h, G_TOTAL_EXP)
}

/// 把 [`TickReport`] 組成對話框文字。
///
/// `{該怪} / {到下一%} / {到升級} / {累積}` — 純四個數字,不論是否升級。
/// 升級時 `to_levelup` / `to_next_pct` 會自動切到新等級的範圍,玩家從數字
/// 突然變大就知道升級了,不需要另外一行特別訊息。
pub fn format_chat_line(r: &TickReport) -> Vec<u8> {
    format!(
        "{} / {} / {} / {}",
        r.delta_kill, r.to_next_pct, r.to_levelup, r.session_total
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_threshold_known_values() {
        // 等級 N 的起點 EXP = LEVEL_THRESHOLDS[N-1](陣列 0-index 對應等級 1-index)
        assert_eq!(level_start_threshold(1), 0);
        assert_eq!(level_start_threshold(10), 10_000);
        assert_eq!(level_start_threshold(11), 14_641);
        assert_eq!(level_start_threshold(64), 364_575_461);
        assert_eq!(level_start_threshold(65), 400_640_553); // = HIGH_BASE,銜接高等線性公式
                                                            // level 66 = HIGH_BASE + 1 * HIGH_RANGE
        assert_eq!(level_start_threshold(66), 400_640_553 + 36_065_092);
        // level 67 = HIGH_BASE + 2 * HIGH_RANGE
        assert_eq!(level_start_threshold(67), 400_640_553 + 2 * 36_065_092);
    }

    #[test]
    fn level_range_basics() {
        assert_eq!(level_range(10), 4_641);
        assert_eq!(level_range(1), 125);
        assert_eq!(level_range(70), 36_065_092);
    }

    /// 真實 snapshot:level=10、total_exp=10202 → 4.35%(使用者實測值)
    #[test]
    fn real_snapshot_matches_user_observation() {
        let total = 10_202u64;
        let level = 10u32;
        let within = total - level_start_threshold(level);
        let range = level_range(level);
        assert_eq!(within, 202);
        assert_eq!(range, 4_641);
        let pct_int = within * 100 / range;
        assert_eq!(pct_int, 4);
        let to_next = exp_to_next_pct(within, range);
        // 5% 目標 = ceil(4641 * 5 / 100) = 233 → 233 - 202 = 31
        assert_eq!(to_next, 31);
    }

    #[test]
    fn tracker_first_tick_after_enable_no_report() {
        let mut t = ExpTracker::default();
        t.enable(10_202);
        assert!(t.tick(10_202).is_none());
    }

    #[test]
    fn tracker_kill_produces_report() {
        let mut t = ExpTracker::default();
        t.enable(10_202);
        let r = t.tick(10_277).expect("應該產生報告");
        assert_eq!(r.delta_kill, 75);
        assert_eq!(r.session_total, 75);
        assert_eq!(r.level, 10);
        assert!(!r.leveled_up);
    }

    #[test]
    fn tracker_disabled_no_report() {
        let mut t = ExpTracker::default();
        assert!(t.tick(10_202).is_none());
    }

    #[test]
    fn tracker_levelup_detected_from_exp() {
        // 14500 仍是 lv10(level_start[11]=14641),14700 跨進 lv11
        let mut t = ExpTracker::default();
        t.enable(14_500);
        let r = t.tick(14_700).expect("升級報告");
        assert!(r.leveled_up);
        assert_eq!(r.level, 11);
        let lv11_within = 14_700 - 14_641;
        assert_eq!(r.to_levelup, level_range(11) - lv11_within);
    }

    #[test]
    fn tracker_session_total_accumulates() {
        let mut t = ExpTracker::default();
        t.enable(10_000);
        let r1 = t.tick(10_075).unwrap();
        assert_eq!(r1.delta_kill, 75);
        assert_eq!(r1.session_total, 75);
        let r2 = t.tick(10_175).unwrap();
        assert_eq!(r2.delta_kill, 100);
        assert_eq!(r2.session_total, 175);
    }

    #[test]
    fn level_from_total_exp_basics() {
        assert_eq!(level_from_total_exp(0), 1);
        assert_eq!(level_from_total_exp(124), 1);
        assert_eq!(level_from_total_exp(125), 2);
        assert_eq!(level_from_total_exp(9_999), 9);
        assert_eq!(level_from_total_exp(10_000), 10);
        assert_eq!(level_from_total_exp(14_640), 10);
        assert_eq!(level_from_total_exp(14_641), 11);
        assert_eq!(level_from_total_exp(18_141), 11); // 實機 screenshot 值
        assert_eq!(level_from_total_exp(400_640_553), 65);
        assert_eq!(level_from_total_exp(400_640_553 + 36_065_092), 66);
    }

    /// 實機 bug 情境:total_exp=18141(等級 11)
    /// 舊版誤把 level 當 10 → to_levelup=0;新版反推 level=11 → 正確。
    #[test]
    fn screenshot_lv11_bug_regression() {
        let mut t = ExpTracker::default();
        t.enable(17_454);
        let r = t.tick(18_141).expect("應該產生報告");
        assert_eq!(r.level, 11);
        let within = 18_141 - 14_641;
        assert_eq!(r.to_levelup, 6_095 - within);
        assert!(r.to_levelup > 0);
    }
}
