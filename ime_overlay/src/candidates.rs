//! IMM32 候選字 / 組字字串 讀取
//!
//! 兩個結構需要小心:
//!   1. CANDIDATELIST 是變長 — header 28 bytes + dwOffset[dwCount] + 字串資料區
//!      字串透過 dwOffset[i] 是相對 candlist 起始的 byte offset 找到 wide-char string
//!   2. ImmGetCompositionStringW 回傳的是 byte 長度(不是 char 數),要 / 2 換 char 數

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::Ime::{
    ImmGetCandidateListW, ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, HIMC,
    IME_COMPOSITION_STRING,
};
use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetParent, GA_ROOT};

use crate::dbg_log;

const GCS_COMPSTR: IME_COMPOSITION_STRING = IME_COMPOSITION_STRING(0x0008);

/// 從 IMM32 拉到的「候選字 + 組字」即時狀態
///
/// 每次 WM_IME_NOTIFY(IMN_CHANGECANDIDATE) 觸發時重新拉。
#[derive(Default, Clone)]
pub struct ImeState {
    /// 組字字串(注音 / 拼音 — IME 還沒提交的那串)
    pub composition: String,
    /// 候選字總數(可能跨多頁)
    pub total: u32,
    /// 目前選到第幾個(0-based,絕對 index 不是頁內 index)
    pub selection: u32,
    /// 目前頁起始 index(0-based)
    pub page_start: u32,
    /// 每頁多少個(通常 9)
    pub page_size: u32,
    /// 完整候選 list(最多取前 64 個避免過大)
    pub items: Vec<String>,
}

impl ImeState {
    /// 拿目前頁要顯示的候選 slice
    pub fn page_items(&self) -> &[String] {
        let start = self.page_start as usize;
        let end = (start + self.page_size as usize).min(self.items.len());
        if start <= end && end <= self.items.len() {
            &self.items[start..end]
        } else {
            &[]
        }
    }

    /// 算頁內被選到的 index(0-based,沒選到回 None)
    pub fn page_selection(&self) -> Option<usize> {
        if self.selection >= self.page_start && self.selection < self.page_start + self.page_size {
            Some((self.selection - self.page_start) as usize)
        } else {
            None
        }
    }
}

/// 拉一份完整 IME 狀態(組字 + 候選 list)
///
/// HIMC 拉取策略 — 子視窗常常回 NULL,fallback:
///   1. ImmGetContext(原 hwnd)
///   2. ImmGetContext(GetParent(hwnd))
///   3. ImmGetContext(GA_ROOT 主視窗)
pub unsafe fn fetch_ime_state(hwnd: HWND) -> Option<ImeState> {
    let himc = resolve_himc(hwnd)?;
    // 注意 himc 屬於 himc.owner_hwnd,釋放時要用同一個 hwnd
    let owner = himc.1;

    let result = (|| -> Option<ImeState> {
        let composition = read_composition(himc.0, GCS_COMPSTR);
        let (total, selection, page_start, page_size, items) = read_candidate_list(himc.0)?;
        Some(ImeState {
            composition,
            total,
            selection,
            page_start,
            page_size,
            items,
        })
    })();

    let _ = ImmReleaseContext(owner, himc.0);
    result
}

/// 嘗試多個 hwnd 拿 HIMC,回 (himc, owner_hwnd)
///
/// 不 log 成功 case(會被 IME polling 灌爆 log)。失敗時才 log。
unsafe fn resolve_himc(hwnd: HWND) -> Option<(HIMC, HWND)> {
    let himc = ImmGetContext(hwnd);
    if !himc.0.is_null() {
        return Some((himc, hwnd));
    }
    let parent = GetParent(hwnd).ok();
    if let Some(p) = parent {
        if !p.0.is_null() {
            let himc = ImmGetContext(p);
            if !himc.0.is_null() {
                return Some((himc, p));
            }
        }
    }
    let root = GetAncestor(hwnd, GA_ROOT);
    if !root.0.is_null() {
        let himc = ImmGetContext(root);
        if !himc.0.is_null() {
            return Some((himc, root));
        }
    }
    dbg_log!("[ime] HIMC FAIL hwnd=0x{:X}", hwnd.0 as usize);
    None
}

/// 讀組字字串(GCS_COMPSTR)或最終確認字串(GCS_RESULTSTR)
pub unsafe fn read_composition(himc: HIMC, kind: IME_COMPOSITION_STRING) -> String {
    // 第一次 NULL buffer 拿 byte length
    let need = ImmGetCompositionStringW(himc, kind, None, 0);
    if need <= 0 || need as usize > 4096 {
        return String::new();
    }
    let mut buf = vec![0u16; (need as usize) / 2];
    let got = ImmGetCompositionStringW(himc, kind, Some(buf.as_mut_ptr() as *mut _), need as u32);
    if got <= 0 {
        return String::new();
    }
    let chars = (got as usize) / 2;
    String::from_utf16_lossy(&buf[..chars.min(buf.len())])
}

/// 讀候選 list — 回 (total, selection, page_start, page_size, items)
unsafe fn read_candidate_list(himc: HIMC) -> Option<(u32, u32, u32, u32, Vec<String>)> {
    // 先拿 size
    let need = ImmGetCandidateListW(himc, 0, None, 0);
    if need == 0 {
        dbg_log!(
            "[ime] ImmGetCandidateListW size=0 (himc=0x{:X})",
            himc.0 as usize
        );
        return None;
    }
    if need > 32768 {
        dbg_log!("[ime] ImmGetCandidateListW size={need} too large");
        return None;
    }
    let mut buf = vec![0u8; need as usize];
    let got = ImmGetCandidateListW(himc, 0, Some(buf.as_mut_ptr() as *mut _), need);
    if got == 0 {
        dbg_log!("[ime] ImmGetCandidateListW fill returned 0");
        return None;
    }
    let parsed = parse_candlist(&buf);
    if parsed.is_none() {
        dbg_log!("[ime] parse_candlist returned None (need={need})");
    }
    parsed
}

/// 解析 CANDIDATELIST 結構
///
/// header 佈局:
///   +0x00 dwSize:u32   整體 size
///   +0x04 dwStyle:u32
///   +0x08 dwCount:u32
///   +0x0C dwSelection:u32
///   +0x10 dwPageStart:u32
///   +0x14 dwPageSize:u32
///   +0x18 dwOffset[0]:u32   字串相對 buffer 起始的 byte offset
///   +0x18+4*i dwOffset[i]
///   字串區: buf[off..] 是 null-terminated wide string
fn parse_candlist(buf: &[u8]) -> Option<(u32, u32, u32, u32, Vec<String>)> {
    if buf.len() < 0x18 {
        return None;
    }
    let read_u32 = |off: usize| -> Option<u32> {
        if off + 4 > buf.len() {
            return None;
        }
        Some(u32::from_le_bytes([
            buf[off],
            buf[off + 1],
            buf[off + 2],
            buf[off + 3],
        ]))
    };

    let _dw_size = read_u32(0x00)?;
    let _dw_style = read_u32(0x04)?;
    let dw_count = read_u32(0x08)?;
    let dw_selection = read_u32(0x0C)?;
    let dw_page_start = read_u32(0x10)?;
    let dw_page_size = read_u32(0x14)?;

    if dw_count == 0 || dw_count > 1024 {
        return None;
    }

    let visible = dw_count.min(64) as usize;
    let mut items = Vec::with_capacity(visible);
    for i in 0..visible {
        let off_pos = 0x18 + i * 4;
        let off = match read_u32(off_pos) {
            Some(o) => o as usize,
            None => break,
        };
        if off >= buf.len() {
            items.push(String::new());
            continue;
        }
        // 從 off 開始讀直到 null 或 buffer 結束
        let mut wide: Vec<u16> = Vec::new();
        let mut p = off;
        while p + 2 <= buf.len() {
            let ch = u16::from_le_bytes([buf[p], buf[p + 1]]);
            if ch == 0 {
                break;
            }
            wide.push(ch);
            p += 2;
            if wide.len() > 64 {
                break;
            } // 安全上限
        }
        items.push(String::from_utf16_lossy(&wide));
    }

    Some((dw_count, dw_selection, dw_page_start, dw_page_size, items))
}
