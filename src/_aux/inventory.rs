//! 物品欄走訪 — 從遊戲記憶體讀玩家當前背包
//!
//! 為什麼自己走訪而非 hook:走訪是「read-only snapshot」,不需要等遊戲觸發
//! 任何 callback,launcher 想看背包狀態(撿物 / 喝水 / 找特定物品)隨時可讀。
//!
//! INVENTORY_BASE = `[0x009A7230]`(見 address.rs / game_database.md §2.2d)
//! 走訪參考靜態反組譯 FUN_004B1E50 / FUN_004B41C0:
//! ```text
//! inv = read_u32(0x9A7230)
//! n   = read_i32(inv + 0x2C)
//! arr = read_u32(inv + 0x58)
//! for i in 0..n:
//!     item = read_u32(arr + i*4)
//! ```

use anyhow::{bail, Context, Result};
use windows::Win32::Foundation::HANDLE;

use crate::aux::address::{item_offset, INVENTORY_BASE};
use crate::memory::{read_bytes, read_u32};

#[cfg(test)]
mod tests {
    #[test]
    fn decodes_traditional_big5_item_names() {
        let (bytes, _, had_errors) = encoding_rs::BIG5.encode("銀劍 (揮舞)");
        assert!(!had_errors);

        assert_eq!(super::decode_item_name_bytes(&bytes), "銀劍 (揮舞)");
    }

    #[test]
    fn decodes_simplified_gbk_item_names() {
        let (bytes, _, had_errors) = encoding_rs::GBK.encode("银剑 (挥舞)");
        assert!(!had_errors);

        assert_eq!(super::decode_item_name_bytes(&bytes), "银剑 (挥舞)");
    }

    #[test]
    fn decodes_ascii_item_names_without_legacy_codepage() {
        assert_eq!(
            super::decode_item_name_bytes(b"red potion\0tail"),
            "red potion"
        );
    }
}

pub(crate) fn decode_item_name_bytes(raw: &[u8]) -> String {
    crate::legacy_text::decode_zstr(raw)
}

/// 單一物品快照(讀完就離開遊戲記憶體,後續使用安全)
#[derive(Debug, Clone)]
pub struct Item {
    /// item_entry 在遊戲堆上的位址(供 use_item 函數呼叫用)
    pub entry_addr: u32,
    /// server-assigned 物品 ID
    pub item_param: u32,
    /// 物品類型(switch dispatcher 的 case key)
    pub item_type: u8,
    /// 動畫 / icon 編號
    pub icon: u16,
    /// 是否裝備中
    pub equipped: bool,
    /// 堆疊數量 — stack 物 = 當前數量(例 365),非 stack = 0 或 1。
    /// 送 SendPacketData("cdd", 0x8A, ...) delete 封包時當 quantity 用。
    pub count: u32,
    /// 物品名稱(big5/utf-8 視遊戲設定,讀到的原始 bytes 留給上層解碼)
    pub name_raw: Vec<u8>,
}

impl Item {
    /// 解碼名稱 — Lineage 3.8 用 Big5(CP950),走 encoding_rs。
    pub fn name_lossy(&self) -> String {
        decode_item_name_bytes(&self.name_raw)
    }
}

/// 取得物品欄基址(`[INVENTORY_BASE]` 解一層指標)
///
/// 回傳 `Ok(None)` 當位址常數未設定;
/// 回傳 `Err` 當遊戲尚未進場(指標值為 0 或不合理小)。
pub fn read_inventory_ptr(h: HANDLE) -> Result<Option<u32>> {
    let Some(base_addr) = INVENTORY_BASE else {
        return Ok(None);
    };
    let inv = read_u32(h, base_addr)
        .with_context(|| format!("read INVENTORY_BASE @ 0x{base_addr:08X}"))?;
    if inv < 0x0010_0000 {
        bail!("INVENTORY_BASE pointer 未初始化(讀到 0x{inv:08X}),請先進入遊戲世界");
    }
    Ok(Some(inv))
}

/// 列舉物品欄所有物品(快照,不持有遊戲記憶體)
pub fn list_items(h: HANDLE) -> Result<Vec<Item>> {
    let inv = match read_inventory_ptr(h)? {
        Some(p) => p,
        None => return Ok(Vec::new()),
    };

    let count = read_u32(h, inv + 0x2C)
        .with_context(|| format!("read count @ 0x{:08X}+0x2C = 0x{:08X}", inv, inv + 0x2C))?
        as i32;
    if !(0..=512).contains(&count) {
        bail!(
            "inventory count 不合理: {count} @ 0x{:08X}+0x2C(可能 INVENTORY_BASE 偏移錯誤)",
            inv
        );
    }
    let array_ptr = read_u32(h, inv + 0x58)
        .with_context(|| format!("read array_ptr @ 0x{:08X}+0x58 = 0x{:08X}", inv, inv + 0x58))?;
    if array_ptr < 0x0010_0000 {
        bail!("inventory array_ptr 未初始化: 0x{array_ptr:08X}");
    }

    let mut items = Vec::with_capacity(count as usize);
    for i in 0..count as u32 {
        let entry = match read_u32(h, array_ptr + i * 4) {
            Ok(p) if p >= 0x0010_0000 => p,
            _ => continue,
        };
        // 一次讀 256 bytes 涵蓋 +0x98 type / +0x9A icon
        let head = match read_bytes(h, entry, 256) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let item_param = u32::from_le_bytes([head[0x04], head[0x05], head[0x06], head[0x07]]);
        let equipped = head[item_offset::EQUIPPED as usize];
        let item_type = head[item_offset::ITEM_TYPE as usize];
        let icon = u16::from_le_bytes([
            head[item_offset::ICON_NUM as usize],
            head[item_offset::ICON_NUM as usize + 1],
        ]);
        let count = {
            let o = item_offset::ITEM_COUNT as usize;
            u32::from_le_bytes([head[o], head[o + 1], head[o + 2], head[o + 3]])
        };
        let name_ptr = u32::from_le_bytes([head[0x0C], head[0x0D], head[0x0E], head[0x0F]]);

        let name_raw = if (0x0010_0000..0x4000_0000).contains(&name_ptr) {
            read_bytes(h, name_ptr, 64).unwrap_or_default()
        } else {
            Vec::new()
        };

        items.push(Item {
            entry_addr: entry,
            item_param,
            item_type,
            icon,
            equipped: equipped != 0,
            count,
            name_raw,
        });
    }

    Ok(items)
}

/// 找出第一個符合條件的物品 — 給後續 drink_hp/mp 用
#[allow(dead_code)]
pub fn find_item<F>(h: HANDLE, pred: F) -> Result<Option<Item>>
where
    F: Fn(&Item) -> bool,
{
    Ok(list_items(h)?.into_iter().find(|it| pred(it)))
}

/// 透過 item_param 查物品(對應 FUN_004B1E50)
#[allow(dead_code)]
pub fn find_by_param(h: HANDLE, item_param: u32) -> Result<Option<Item>> {
    find_item(h, |it| it.item_param == item_param)
}

/// 完整 raw dump — 不過濾,印每個 item_entry 前 64 bytes 含 hex + ASCII + UTF-16 name 嘗試。
/// 給診斷反向工程結構用。
pub fn dump_raw(h: HANDLE) -> Result<()> {
    use crate::log_line;
    let inv = match read_inventory_ptr(h)? {
        Some(p) => p,
        None => {
            log_line!("[inv-raw] INVENTORY_BASE 未設定");
            return Ok(());
        }
    };
    let count =
        read_u32(h, inv + 0x2C).with_context(|| format!("read count @ 0x{inv:08X}+0x2C"))? as i32;
    let array_ptr =
        read_u32(h, inv + 0x58).with_context(|| format!("read array_ptr @ 0x{inv:08X}+0x58"))?;
    log_line!("[inv-raw] inv=0x{inv:08X} count={count} array_ptr=0x{array_ptr:08X}");

    if !(0..=200).contains(&count) {
        log_line!("[inv-raw] count={count} 不合理,放棄");
        return Ok(());
    }

    for i in 0..count as u32 {
        let entry = match read_u32(h, array_ptr + i * 4) {
            Ok(p) => p,
            Err(e) => {
                log_line!("[inv-raw] #{i:02} read entry_ptr 失敗: {e:#}");
                continue;
            }
        };
        if !(0x0010_0000..0x4000_0000).contains(&entry) {
            log_line!("[inv-raw] #{i:02} entry=0x{entry:08X} 不合理,跳過");
            continue;
        }
        let bytes = match read_bytes(h, entry, 64) {
            Ok(b) => b,
            Err(e) => {
                log_line!("[inv-raw] #{i:02} entry=0x{entry:08X} 讀 64 bytes 失敗: {e:#}");
                continue;
            }
        };
        log_line!("[inv-raw] ── #{i:02} entry=0x{entry:08X} ──");
        for chunk_idx in 0..4 {
            let s = chunk_idx * 16;
            let e = s + 16;
            let line: String = bytes[s..e]
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ");
            let ascii: String = bytes[s..e]
                .iter()
                .map(|&b| {
                    if (0x20..=0x7E).contains(&b) {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            log_line!("[inv-raw]   +{:02X}: {line}  |{ascii}|", s);
        }
        // 嘗試解 +0x0C 的 name_ptr 為 UTF-8 + UTF-16-LE
        let name_ptr = u32::from_le_bytes([bytes[0x0C], bytes[0x0D], bytes[0x0E], bytes[0x0F]]);
        if (0x0010_0000..0x4000_0000).contains(&name_ptr) {
            if let Ok(nb) = read_bytes(h, name_ptr, 64) {
                let utf8_end = nb.iter().position(|&c| c == 0).unwrap_or(nb.len()).min(48);
                let legacy = decode_item_name_bytes(&nb[..utf8_end]);

                let utf16_units: Vec<u16> = nb
                    .chunks_exact(2)
                    .take(24)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&u| u != 0)
                    .collect();
                let utf16 = String::from_utf16_lossy(&utf16_units);

                log_line!("[inv-raw]     name@0x{name_ptr:08X} legacy={legacy:?} utf16={utf16:?}");
            }
        }
    }
    Ok(())
}

/// 嘗試多個 INVENTORY_BASE 候選位址,回傳每個的「合理度」評分。
///
/// 用於 INVENTORY_BASE 位址不確定時的診斷:讀 candidate → 解一層 → 看 [+0x2C]/[+0x58]
/// 是否符合 inventory layout(count 1..=512, array_ptr 在 heap)。
pub fn probe_candidates(h: HANDLE, candidates: &[(u32, &str)]) -> Vec<String> {
    let mut out = Vec::new();
    for &(addr, name) in candidates {
        match probe_one(h, addr) {
            Ok(report) => out.push(format!("[probe] {name} @ 0x{addr:08X} → {report}")),
            Err(e) => out.push(format!("[probe] {name} @ 0x{addr:08X} → 失敗: {e:#}")),
        }
    }
    out
}

/// 掃描 .data 區段(0x900000~0xD00000)找符合 inventory fingerprint 的指標。
///
/// Fingerprint:
/// - `[addr]` = ptr 在 heap range(0x01000000~0x40000000)
/// - `[ptr+0x2C]` = count 在 1..=200(過濾掉常見的 19/373 雜訊)
/// - `[ptr+0x58]` = array_ptr 在 heap range
/// - `[[ptr+0x58]]` = item0 指標,在 heap range
/// - `[item0+0x0C]` = name_ptr 指向可讀字串(關鍵 — 確認是 item_entry)
///
/// 只回傳通過「name 預覽」的候選,大幅降噪。
pub fn scan_data_for_inventory(h: HANDLE) -> Vec<String> {
    let mut hits = Vec::new();
    let mut seen_ptrs = std::collections::HashSet::new();
    let start = 0x0090_0000u32;
    let end = 0x00D0_0000u32;
    let mut addr = start;
    while addr < end {
        if let Ok(ptr) = read_u32(h, addr) {
            if (0x0100_0000..0x4000_0000).contains(&ptr) && seen_ptrs.insert(ptr) {
                if let (Ok(count), Ok(array_ptr)) =
                    (read_u32(h, ptr + 0x2C), read_u32(h, ptr + 0x58))
                {
                    let count_i = count as i32;
                    if (1..=200).contains(&count_i)
                        && (0x0100_0000..0x4000_0000).contains(&array_ptr)
                    {
                        if let Ok(item0) = read_u32(h, array_ptr) {
                            if (0x0100_0000..0x4000_0000).contains(&item0) {
                                // 讀第一個 item 的 +0x0C(name_ptr)和 +0x98(type)
                                let name_ptr = read_u32(h, item0 + 0x0C).unwrap_or(0);
                                let type_byte =
                                    read_bytes(h, item0 + 0x98, 1).map(|b| b[0]).unwrap_or(0);
                                let item_param = read_u32(h, item0 + 0x04).unwrap_or(0);

                                let name_preview = if (0x0100_0000..0x4000_0000).contains(&name_ptr)
                                {
                                    read_bytes(h, name_ptr, 32)
                                        .map(|b| decode_item_name_bytes(&b))
                                        .unwrap_or_else(|_| String::from("<read fail>"))
                                } else {
                                    format!("<name_ptr=0x{name_ptr:08X}>")
                                };

                                let printable_score = name_preview
                                    .bytes()
                                    .take(8)
                                    .filter(|&b| (0x20..=0x7E).contains(&b) || b >= 0x80)
                                    .count();
                                let star = if printable_score >= 4 { "★" } else { "·" };

                                hits.push(format!(
                                    "[scan] {star} @ 0x{addr:08X} → ptr=0x{ptr:08X} count={count_i} item0=0x{item0:08X} param=0x{item_param:08X} type=0x{type_byte:02X} name={name_preview:?}"
                                ));
                                if hits.len() >= 40 {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        addr += 4;
    }
    hits
}

fn probe_one(h: HANDLE, base_addr: u32) -> Result<String> {
    let inv = read_u32(h, base_addr).with_context(|| format!("read [{:08X}]", base_addr))?;
    if inv < 0x0010_0000 {
        return Ok(format!("ptr=0x{inv:08X} (太小,跳過)"));
    }
    let count = read_u32(h, inv + 0x2C).with_context(|| format!("read [0x{inv:08X}+0x2C]"))? as i32;
    let array_ptr = read_u32(h, inv + 0x58).with_context(|| format!("read [0x{inv:08X}+0x58]"))?;
    let valid_count = (0..=512).contains(&count);
    let valid_array = array_ptr >= 0x0010_0000;
    let verdict = match (valid_count, valid_array) {
        (true, true) => "★ 候選",
        (true, false) => "count OK, array_ptr 不合理",
        (false, true) => "array_ptr OK, count 不合理",
        (false, false) => "兩者都不合理",
    };
    Ok(format!(
        "ptr=0x{inv:08X} count={count} array_ptr=0x{array_ptr:08X} → {verdict}"
    ))
}
