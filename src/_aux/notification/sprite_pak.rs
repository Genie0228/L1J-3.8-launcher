//! Sprite.pak 索引讀取 + PNG decode
//!
//! 啟動時掃描 `<game_dir>` 內所有 `Sprite*.idx`,建立 `filename → (pak_path, offset, size)`
//! 索引。 .idx 整體 layout(對齊 PakViewer `OldL1Handler` 無 magic / 無 protection 版):
//!   +0x00: int32 LE record_count(檔頭)
//!   +0x04 起:28-byte entries × record_count,每個 entry:
//!     +0x00: u32 LE offset(在對應 `.pak` 內的 file offset)
//!     +0x04: 20-byte filename(null-padded, ASCII)
//!     +0x18: u32 LE size(實際 entry 在 pak 內的 byte 大小)
//!
//! .pak 內存的是 standard PNG 檔(`89 50 4E 47 ...`),用 `png` crate decode 即可。

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use once_cell::sync::Lazy;

use crate::logger::log_line;

pub struct DecodedPng {
    pub width: u32,
    pub height: u32,
    /// RGBA8 row-major, top-down(配合 GDI top-down DIB)。
    pub rgba: Vec<u8>,
}

struct IdxEntry {
    pak_path: PathBuf,
    offset: u32,
    size: u32,
}

static INDEX: Lazy<Mutex<Option<HashMap<String, IdxEntry>>>> = Lazy::new(|| Mutex::new(None));

const HEADER_SIZE: usize = 4;     // int32 LE record_count
const ENTRY_SIZE: usize = 28;
const FILENAME_LEN: usize = 20;
const FILENAME_OFFSET: usize = 4; // entry +0x04 起 20 bytes 是 filename
const SIZE_OFFSET: usize = 24;    // entry +0x18 起 4 bytes 是 size

/// 初始化:掃 `game_dir` 內所有 `Sprite*.idx`,建索引。 idempotent — 重複呼叫只重建一次。
pub fn init(game_dir: &Path) -> Result<()> {
    let mut map: HashMap<String, IdxEntry> = HashMap::new();

    let entries = std::fs::read_dir(game_dir)
        .with_context(|| format!("讀 {} 失敗", game_dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        let fname = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if !fname.starts_with("Sprite") || !fname.to_ascii_lowercase().ends_with(".idx") {
            continue;
        }
        let pak_path = path.with_extension("pak");
        if !pak_path.exists() {
            continue;
        }

        let idx_data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                log_line!("[sprite_pak] 讀 {:?} 失敗: {e}", path);
                continue;
            }
        };

        // PakViewer OldL1Handler 格式:檔頭 4-byte record_count,之後 28-byte entries。
        if idx_data.len() < HEADER_SIZE {
            log_line!("[sprite_pak] {:?} 太短,跳過", path);
            continue;
        }
        let body = &idx_data[HEADER_SIZE..];
        let n = body.len() / ENTRY_SIZE;
        for i in 0..n {
            let base = i * ENTRY_SIZE;
            let offset = u32::from_le_bytes(body[base..base + 4].try_into().unwrap());
            let name_bytes = &body[base + FILENAME_OFFSET..base + FILENAME_OFFSET + FILENAME_LEN];
            let nul = name_bytes.iter().position(|&b| b == 0).unwrap_or(FILENAME_LEN);
            let name = match std::str::from_utf8(&name_bytes[..nul]) {
                Ok(s) => s.to_string(),
                Err(_) => continue,
            };
            if name.is_empty() {
                continue;
            }
            let size = u32::from_le_bytes(
                body[base + SIZE_OFFSET..base + SIZE_OFFSET + 4]
                    .try_into()
                    .unwrap(),
            );
            // 同名檔遇到不同 .pak 時保留先掃到的 — 跟 game 的 Sprite00 → Sprite12 search 順序對齊。
            map.entry(name).or_insert(IdxEntry {
                pak_path: pak_path.clone(),
                offset,
                size,
            });
        }
    }

    let count = map.len();
    *INDEX.lock().unwrap() = Some(map);
    log_line!("[sprite_pak] indexed {} entries from {}", count, game_dir.display());
    Ok(())
}

/// Lookup + open + 讀 raw bytes(不做 PNG decode)— 用於 .tbt 之類非 PNG 內容。
/// idx size 在新 parser 下可靠,直接用;64KB 上限擋掉異常 entry(typical .tbt 在 50KB 以下)。
pub fn load_raw(filename: &str) -> Option<Vec<u8>> {
    let (pak_path, offset, size) = {
        let guard = INDEX.lock().ok()?;
        let map = guard.as_ref()?;
        let entry = map.get(filename)?;
        (entry.pak_path.clone(), entry.offset, entry.size)
    };
    let mut f = File::open(&pak_path).ok()?;
    f.seek(SeekFrom::Start(offset as u64)).ok()?;
    let cap = (size as usize).min(65_536);
    let mut buf = vec![0u8; cap];
    use std::io::Read as _;
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// Lookup + open + stream decode。 idx 內的 size 欄位不可信(實測過長或過短皆有),
/// 直接 stream-read 讓 png decoder 自己看到 IEND chunk 就停。
pub fn load_png(filename: &str) -> Option<DecodedPng> {
    load_png_internal(filename, /*auto_key=*/ true)
}

/// 取原始 RGB(不跑 auto-key)— paired alpha extraction 用。
pub fn load_png_raw(filename: &str) -> Option<DecodedPng> {
    load_png_internal(filename, /*auto_key=*/ false)
}

fn load_png_internal(filename: &str, auto_key: bool) -> Option<DecodedPng> {
    let (pak_path, offset) = {
        let guard = INDEX.lock().ok()?;
        let map = guard.as_ref()?;
        let entry = match map.get(filename) {
            Some(e) => e,
            None => {
                let sample: Vec<&String> = map.keys().take(3).collect();
                log_line!(
                    "[sprite_pak] load_png '{filename}' 不在索引(size={}), sample={:?}",
                    map.len(),
                    sample
                );
                return None;
            }
        };
        (entry.pak_path.clone(), entry.offset)
    };
    let mut f = match File::open(&pak_path) {
        Ok(f) => f,
        Err(e) => {
            log_line!(
                "[sprite_pak] load_png '{filename}': open {pak_path:?} 失敗 {e}"
            );
            return None;
        }
    };
    if let Err(e) = f.seek(SeekFrom::Start(offset as u64)) {
        log_line!("[sprite_pak] load_png '{filename}': seek 失敗 {e}");
        return None;
    }
    let decoded = decode_png_stream(BufReader::new(f), auto_key);
    if decoded.is_none() {
        log_line!(
            "[sprite_pak] load_png '{filename}': decode stream 失敗(pak={pak_path:?} off=0x{offset:08X})"
        );
    }
    decoded
}

/// 對應一張黑底 + 一張白底 PNG,用 paired-sprite alpha extraction 還原 straight alpha + RGB。
/// 公式:per channel `alpha = 255 - (white - black)`,straight RGB 取 black / alpha。
/// 用三通道 min(alpha) 當最終 alpha,避免邊緣半透明像素被誤判全不透明。
pub fn load_png_paired(black_name: &str, white_name: &str) -> Option<DecodedPng> {
    let black = load_png_raw(black_name)?;
    let white = load_png_raw(white_name)?;
    if black.width != white.width || black.height != white.height {
        log_line!(
            "[sprite_pak] paired size 不一致 '{black_name}' {}x{} vs '{white_name}' {}x{}",
            black.width, black.height, white.width, white.height
        );
        return None;
    }
    let pixels = (black.width * black.height) as usize;
    let mut rgba = Vec::with_capacity(pixels * 4);
    for i in 0..pixels {
        let off = i * 4;
        let br = black.rgba[off] as i32;
        let bg = black.rgba[off + 1] as i32;
        let bb = black.rgba[off + 2] as i32;
        let wr = white.rgba[off] as i32;
        let wg = white.rgba[off + 1] as i32;
        let wb = white.rgba[off + 2] as i32;
        // alpha 從 R/G/B 各推一份,取最不透明(最低 diff = 最高 alpha)
        let ar = 255 - (wr - br).max(0);
        let ag = 255 - (wg - bg).max(0);
        let ab = 255 - (wb - bb).max(0);
        let alpha = ar.min(ag).min(ab).clamp(0, 255) as u32;
        let (rr, gg, bb_out) = if alpha == 0 {
            (0, 0, 0)
        } else {
            // straight color = premultiplied black / (alpha/255)
            let r = ((br as u32 * 255 + alpha / 2) / alpha).min(255);
            let g = ((bg as u32 * 255 + alpha / 2) / alpha).min(255);
            let b = ((bb as u32 * 255 + alpha / 2) / alpha).min(255);
            (r as u8, g as u8, b as u8)
        };
        rgba.push(rr);
        rgba.push(gg);
        rgba.push(bb_out);
        rgba.push(alpha as u8);
    }
    Some(DecodedPng {
        width: black.width,
        height: black.height,
        rgba,
    })
}

fn decode_png_stream<R: Read>(data: R, auto_key: bool) -> Option<DecodedPng> {
    use png::ColorType;
    let decoder = png::Decoder::new(data);
    let mut reader = decoder.read_info().ok()?;
    let mut frame = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut frame).ok()?;
    let w = info.width;
    let h = info.height;
    let pixels = (w * h) as usize;
    let had_alpha = matches!(info.color_type, ColorType::Rgba | ColorType::GrayscaleAlpha);

    let mut rgba: Vec<u8> = match info.color_type {
        ColorType::Rgba => frame,
        ColorType::Rgb => {
            let mut out = Vec::with_capacity(pixels * 4);
            for chunk in frame.chunks_exact(3) {
                out.push(chunk[0]);
                out.push(chunk[1]);
                out.push(chunk[2]);
                out.push(0xFF);
            }
            out
        }
        ColorType::Grayscale => {
            let mut out = Vec::with_capacity(pixels * 4);
            for &g in &frame {
                out.push(g);
                out.push(g);
                out.push(g);
                out.push(0xFF);
            }
            out
        }
        ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(pixels * 4);
            for chunk in frame.chunks_exact(2) {
                out.push(chunk[0]);
                out.push(chunk[0]);
                out.push(chunk[0]);
                out.push(chunk[1]);
            }
            out
        }
        ColorType::Indexed => {
            // 罕見 — 沒處理 palette,先放棄
            return None;
        }
    };

    // Lineage sprite transparency convention:純 (0,0,0) 當透明 key。
    // 只在 4 角都是純黑時才觸發,避免誤 key 1888 道具框(角落 (15,9,2))/ 1889 bar(灰階)。
    // paired alpha extraction 路徑會關掉 auto_key 以保留 black-bg raw RGB。
    if auto_key && !had_alpha && w >= 2 && h >= 2 {
        let stride = w as usize * 4;
        let is_black = |x: usize, y: usize| {
            let off = y * stride + x * 4;
            rgba[off] == 0 && rgba[off + 1] == 0 && rgba[off + 2] == 0
        };
        let all_corners_black = is_black(0, 0)
            && is_black((w - 1) as usize, 0)
            && is_black(0, (h - 1) as usize)
            && is_black((w - 1) as usize, (h - 1) as usize);
        if all_corners_black {
            for i in 0..pixels {
                let off = i * 4;
                if rgba[off] == 0 && rgba[off + 1] == 0 && rgba[off + 2] == 0 {
                    rgba[off + 3] = 0;
                }
            }
        }
    }

    Some(DecodedPng {
        width: w,
        height: h,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_layout_sanity() {
        // size(4) + offset(4) + filename(20) = 28 bytes
        assert_eq!(ENTRY_SIZE, 4 + 4 + FILENAME_LEN);
    }

    #[test]
    fn decode_8x8_png() {
        // minimal 8x8 RGBA PNG manually crafted via the png crate
        let mut buf = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut buf, 8, 8);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            let pixels: Vec<u8> = (0..8 * 8 * 4).map(|i| (i & 0xFF) as u8).collect();
            w.write_image_data(&pixels).unwrap();
        }
        let p = decode_png_stream(buf.as_slice(), true).unwrap();
        assert_eq!(p.width, 8);
        assert_eq!(p.height, 8);
        assert_eq!(p.rgba.len(), 8 * 8 * 4);
    }
}
