//! TBT 物品圖檔 disk decoder。
//!
//! 演算法 port 自 PakViewer `Lin.Helper.Core.Image.ImageConverter.LoadL1Image`
//! (LoadTbt 真正走的那條 — *不是* `LoadL1ImageRaw`),不依賴遊戲 memory。
//!
//! TBT format(.tbt / .img 共用 L1 image format):
//!   Header 4 bytes:[XOffset:u8, YOffset:u8, HeaderWidth:u8, HeaderHeight:u8]
//!   每 row:`[n_spans:u8]` 之後每 span `[skip_bytes:u8, n_pixels:u8, pixels[n_pixels]:u16 LE]`
//!   **skip 是 bytes 單位,實際 pixel 偏移 = skip_bytes / 2**(每 pixel 占 2 bytes)。
//!   像素 RGB555:`bit 14..10 = R, bit 9..5 = G, bit 4..0 = B`(bit 15 unused)
//!   像素 == 0 即透明,否則 5-bit channel 用 `(x<<3) | (x>>2)` 擴成 8-bit 全不透明。
//!
//! 解碼分兩 pass:
//!   pass 1 預掃 → actual_width = max(header_w, max per-row(skip/2 + n_px)),
//!                 容許 span 超出 header width(broken span)而不截斷。
//!   pass 2 實際填 RGB555 buffer,然後合成到 32x32 canvas(以 x_off/y_off 定位)。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use once_cell::sync::Lazy;
use windows::Win32::Foundation::HANDLE;

use crate::logger::log_line;

use super::sprite_pak::{self, DecodedPng};

/// cache:Some(Arc) = 已成功 render。 失敗(找不到檔 / 格式錯)不入 cache 但會 log 一次。
static CACHE: Lazy<Mutex<HashMap<u16, Arc<DecodedPng>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// 已 log 過失敗的 gfxid — 避免 polling spam。
static FAIL_LOGGED: Lazy<Mutex<std::collections::HashSet<u16>>> =
    Lazy::new(|| Mutex::new(std::collections::HashSet::new()));

/// 啟動鏈呼叫 — 目前 disk-based 不需 init,留 stub 保持對外 API。
pub fn init(_h: HANDLE) -> Result<()> {
    Ok(())
}

/// polling thread 呼叫:對目前 queue 中的 toast gfxid 嘗試載入(cache miss 才 decode)。
pub fn ensure_icon(h: HANDLE, gfxid: u16) {
    let _ = h;
    if gfxid == 0 {
        return;
    }
    {
        let cache = match CACHE.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        if cache.contains_key(&gfxid) {
            return;
        }
    }
    match load_and_render(gfxid) {
        Ok(icon) => {
            if let Ok(mut cache) = CACHE.lock() {
                cache.insert(gfxid, icon);
                log_line!("[tbt] cached icon gfxid={gfxid}");
            }
        }
        Err(reason) => {
            if let Ok(mut tried) = FAIL_LOGGED.lock() {
                if tried.insert(gfxid) {
                    log_line!("[tbt] load_and_render gfxid={gfxid} 失敗: {reason}");
                }
            }
        }
    }
}

/// overlay thread 呼叫 — 純讀 cache。
pub fn get_icon(gfxid: u16) -> Option<Arc<DecodedPng>> {
    CACHE.lock().ok()?.get(&gfxid).cloned()
}

fn load_and_render(gfxid: u16) -> Result<Arc<DecodedPng>> {
    use anyhow::{anyhow, Context};
    let filename = format!("{gfxid}.tbt");
    let buf = sprite_pak::load_raw(&filename)
        .with_context(|| format!("Sprite.pak 找不到 '{filename}'"))?;
    let (rgba, w, h) = decode_tbt(&buf).ok_or_else(|| anyhow!("decode_tbt 失敗"))?;
    Ok(Arc::new(DecodedPng {
        width: w,
        height: h,
        rgba,
    }))
}

/// Decode TBT bytes → straight RGBA8 natural-size 影像(top-down)。
/// 對應 PakViewer `LoadTbt` → `LoadL1Image` 無 canvas 版:回傳 `actualWidth × headerHeight` 自然尺寸,
/// **不套用 x_off/y_off**(那是世界渲染的地面對齊資訊,不適用 inventory)。
/// inventory 用 sprite 自然大小,由 overlay 在 37×37 道具框內居中。
fn decode_tbt(buf: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    if buf.len() < 4 {
        return None;
    }
    // header byte 0/1 是 x_off/y_off — 世界渲染對齊用,inventory 一律忽略
    let _ = buf[0];
    let _ = buf[1];
    let header_w = buf[2] as usize;
    let header_h = buf[3] as usize;
    if header_w == 0 || header_h == 0 {
        return None;
    }

    let body_start = 4usize;

    // pass 1:預掃算 actual_width。 EOF 即視為剩餘列透明(野生 .tbt 常見 — 例如 912.tbt
    // 宣告 30 列但只塞 23 列,後 7 列由 canvas 透明補)— 不再 return None。
    let mut p = body_start;
    let mut actual_w = header_w;
    let mut decoded_rows = 0usize;
    'pass1: for _ in 0..header_h {
        if p >= buf.len() {
            break;
        }
        let n_spans = buf[p] as usize;
        p += 1;
        let mut x: usize = 0;
        for _ in 0..n_spans {
            if p + 1 >= buf.len() {
                break 'pass1;
            }
            let skip_px = (buf[p] as usize) / 2;
            let n_px = buf[p + 1] as usize;
            p += 2;
            x += skip_px;
            let end_x = x + n_px;
            if end_x > actual_w {
                actual_w = end_x;
            }
            p += n_px * 2;
            if p > buf.len() {
                break 'pass1;
            }
            x = end_x;
        }
        decoded_rows += 1;
    }

    // pass 2:實際填 RGB555 buffer,只填 pass 1 有確認跑完的列
    let mut pixels = vec![0u16; actual_w * header_h];
    p = body_start;
    for y in 0..decoded_rows {
        if p >= buf.len() {
            break;
        }
        let n_spans = buf[p] as usize;
        p += 1;
        let mut x: usize = 0;
        for _ in 0..n_spans {
            if p + 1 >= buf.len() {
                break;
            }
            let skip_px = (buf[p] as usize) / 2;
            let n_px = buf[p + 1] as usize;
            p += 2;
            x += skip_px;
            for i in 0..n_px {
                if p + 1 >= buf.len() {
                    break;
                }
                let px = u16::from_le_bytes([buf[p], buf[p + 1]]);
                p += 2;
                if x + i < actual_w {
                    pixels[y * actual_w + x + i] = px;
                }
            }
            x += n_px;
        }
    }

    // 直接轉 natural-size RGBA8(top-down,top-left = (0,0))
    let mut rgba = vec![0u8; actual_w * header_h * 4];
    for y in 0..header_h {
        for x in 0..actual_w {
            let px = pixels[y * actual_w + x];
            if px == 0 {
                continue; // 透明 — vec 預設 alpha=0
            }
            let off = (y * actual_w + x) * 4;
            let r5 = ((px >> 10) & 0x1F) as u32;
            let g5 = ((px >> 5) & 0x1F) as u32;
            let b5 = (px & 0x1F) as u32;
            // 5-bit → 8-bit:複位元拓寬 (x<<3)|(x>>2),0..31 → 0..255 平滑
            rgba[off] = ((r5 << 3) | (r5 >> 2)) as u8;
            rgba[off + 1] = ((g5 << 3) | (g5 >> 2)) as u8;
            rgba[off + 2] = ((b5 << 3) | (b5 >> 2)) as u8;
            rgba[off + 3] = 0xFF;
        }
    }

    Some((rgba, actual_w as u32, header_h as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_truncated_header() {
        assert!(decode_tbt(&[]).is_none());
        assert!(decode_tbt(&[0, 0, 0]).is_none());
    }

    #[test]
    fn decode_zero_dim() {
        assert!(decode_tbt(&[0, 0, 0, 5]).is_none());
        assert!(decode_tbt(&[0, 0, 10, 0]).is_none());
    }

    #[test]
    fn decode_single_red_pixel_at_origin() {
        // x_off=0 y_off=0 W=1 H=1, 1 span (skip=0, n=1), red = 0x7C00 (R=31, G=0, B=0)
        let buf = vec![0u8, 0, 1, 1, 1, 0, 1, 0x00, 0x7C];
        let (rgba, w, h) = decode_tbt(&buf).expect("應成功");
        // natural size = 1×1
        assert_eq!((w, h), (1, 1));
        assert_eq!(rgba.len(), 4);
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn decode_with_offset_ignored() {
        // x_off=5 y_off=3 但 inventory 不套用 offset → 像素就是 natural-size (1,1) @ (0,0)
        let buf = vec![5u8, 3, 1, 1, 1, 0, 1, 0x00, 0x7C];
        let (rgba, w, h) = decode_tbt(&buf).expect("應成功");
        assert_eq!((w, h), (1, 1));
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn decode_zero_pixel_is_transparent() {
        // pixel == 0 → alpha 留 0
        let buf = vec![0u8, 0, 1, 1, 1, 0, 1, 0x00, 0x00];
        let (rgba, w, h) = decode_tbt(&buf).expect("應成功");
        assert_eq!((w, h), (1, 1));
        assert_eq!(&rgba[0..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn decode_skip_is_bytes_divided_by_two() {
        // 4x1 圖,skip=4 bytes(= 2 個 pixel), n=2 pixels(白 + 紅)
        let buf = vec![
            0u8, 0, 4, 1, 1, // header w=4 h=1, 1 span
            4, 2,            // skip=4 bytes(/2 = 2 px), 2 pixels
            0xFF, 0x7F,      // pixel 0x7FFF = white
            0x00, 0x7C,      // pixel 0x7C00 = red
        ];
        let (rgba, _, _) = decode_tbt(&buf).expect("應成功");
        // canvas[0,2] = white
        assert_eq!(&rgba[8..12], &[255, 255, 255, 255]);
        // canvas[0,3] = red
        assert_eq!(&rgba[12..16], &[255, 0, 0, 255]);
        // canvas[0,0], [0,1] 透明(skip 跳過)
        assert_eq!(&rgba[0..4], &[0, 0, 0, 0]);
        assert_eq!(&rgba[4..8], &[0, 0, 0, 0]);
    }

    #[test]
    fn decode_span_extending_past_header_width() {
        // header_w=2 但 span 帶 4 pixels → actual_w 應變 4(不截斷)
        // 結果:row 0 的 0..2 在 canvas[0..2],2..4 在 canvas[2..4]
        let buf = vec![
            0u8, 0, 2, 1, 1, // header w=2 h=1, 1 span
            0, 4,            // skip 0, 4 pixels
            0xFF, 0x7F,      // 0x7FFF white
            0x00, 0x7C,      // 0x7C00 red
            0xE0, 0x03,      // 0x03E0 green
            0x1F, 0x00,      // 0x001F blue
        ];
        let (rgba, _, _) = decode_tbt(&buf).expect("應成功");
        // 4 個 pixel 都應出現在 canvas row 0(actual_w 動態擴張)
        assert_eq!(&rgba[0..4], &[255, 255, 255, 255]); // white
        assert_eq!(&rgba[4..8], &[255, 0, 0, 255]);     // red
        assert_eq!(&rgba[8..12], &[0, 255, 0, 255]);    // green
        assert_eq!(&rgba[12..16], &[0, 0, 255, 255]);   // blue
    }

    #[test]
    fn cache_get_empty_returns_none() {
        assert!(get_icon(0xFFFE).is_none());
    }

    #[test]
    fn decode_truncated_body_returns_partial() {
        // header 宣告 2 列,但只塞 1 列完整資料 → 第 2 列以後當透明處理
        // row 0: 1 span, skip=0, n=1 px=red(7 bytes total: hdr 4 + row 3)
        // row 1: 缺整列(EOF) — 不應 return None
        let buf = vec![0u8, 0, 1, 2, 1, 0, 1, 0x00, 0x7C];
        let (rgba, w, h) = decode_tbt(&buf).expect("EOF mid-image 應 fallback,非 None");
        // natural size = 1×2(header h=2,即使 row 1 沒資料還是回 2 列)
        assert_eq!((w, h), (1, 2));
        // row 0 col 0 = red
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255]);
        // row 1 整列透明(EOF skip)
        assert_eq!(&rgba[4..8], &[0, 0, 0, 0]);
    }
}
