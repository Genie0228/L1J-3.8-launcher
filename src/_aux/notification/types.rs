//! Notification 型別與輔助。

use std::time::Instant;

/// 上層送進來的 notification 種類。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    ToastBottomLeft { gfxid: u16, text: Vec<u8> },
    FloatingScreen { kind: FloatKind, amount: u32 },
}

/// 飄字種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatKind {
    Exp,
    Gold,
}

/// Queue 內活著的 toast 條目。
#[derive(Debug, Clone)]
pub(super) struct LiveToast {
    pub spawned_at: Instant,
    pub gfxid: u16,
    pub text: Vec<u8>,
}

/// Queue 內活著的飄字條目。
#[derive(Debug, Clone)]
pub(super) struct LiveFloat {
    pub spawned_at: Instant,
    pub kind: FloatKind,
    pub amount: u32,
    pub cascade_offset: u8,
}

/// Render list 一筆(polling 寫,Phase 3 codecave 讀)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderEntry {
    pub kind: u8,         // 0=blit img only, 1=text only
    pub img_id: u16,      // 0=不畫
    pub x: i16,
    pub y: i16,
    pub alpha: u8,        // 0~255
    pub text_ptr: u32,    // 0=不畫
    pub text_color: u32,
    pub font_id: u16,
}

/// 千分位逗號格式化(`6260` → `"6,260"`)。
pub fn with_commas(n: u32) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_commas_zero() {
        assert_eq!(with_commas(0), "0");
    }

    #[test]
    fn with_commas_two_digit() {
        assert_eq!(with_commas(99), "99");
    }

    #[test]
    fn with_commas_three_digit() {
        assert_eq!(with_commas(100), "100");
        assert_eq!(with_commas(999), "999");
    }

    #[test]
    fn with_commas_four_digit() {
        assert_eq!(with_commas(1000), "1,000");
        assert_eq!(with_commas(1234), "1,234");
    }

    #[test]
    fn with_commas_screenshot_value() {
        // 來自使用者 screenshot 的 EXP 數字
        assert_eq!(with_commas(6260), "6,260");
    }

    #[test]
    fn with_commas_seven_digit() {
        assert_eq!(with_commas(1_000_000), "1,000,000");
    }

    #[test]
    fn with_commas_max() {
        assert_eq!(with_commas(u32::MAX), "4,294,967,295");
    }
}
