//! Entity 掃描 — `/IT=<entity名>` 全自動所需的玩家/召喚物 heap 搜尋。
//!
//! ## 結構驗證(2026-05-03,Frida + heap pattern search + 真機驗證)
//!
//! 目標 entity 結構(vfptr `0x008DC08C` 涵蓋 LOCAL + REMOTE + 自身 avatar + 召喚物):
//!
//! | Offset | 內容 |
//! |--------|------|
//! | `+0x00` | vfptr `0x008DC08C` |
//! | `+0x0C` | **target_id**(送 packet 用,本機 = 小 char_id;伺服器 avatar = 大 object_id) |
//! | `+0x60` | name 候選 1 — 通常 LOCAL 是 ASCII / REMOTE 是「位置 label」 |
//! | `+0x64` | name 候選 2 — REMOTE 玩家:`name+的`(例:"Qwqqq456456的"),召喚物用相同 |
//! | `+0x6C` | name 候選 3 — REMOTE 玩家:**clean name**(例:"Qwqqq456456") |
//!
//! 三個 offset 都試,任一精確匹配就吃。實機觀察:
//! - 召喚物的 `+0x6C` 是「擁有者名字」(因為遊戲 render 時組合 `<owner>的 <species>`,
//!   heap 只存 owner 名)。所以 `/IT=自己名字` 會找到自己的召喚物(我們跳過 LOCAL player)。
//! - 玩家的 species `魔熊` 沒存在 entity name 裡,要靠 sprite_id 查表(未做)。
//!
//! ## 跳過 LOCAL player
//!
//! `[0xC2D2B8]` 那個 entity 的 `+0x0C` 是 **client-only 小 ID**(例如 `0xBF`),
//! 直接送網路會被 server 拒絕。要對自己施放就用 `/IT`(半自動),由遊戲彈出
//! 選目標對話框讓玩家手動點(這時遊戲會用對的 server-side avatar object_id)。
//!
//! ## Diagnostic dump
//!
//! [`find_entity_by_name`] 失敗時 caller 應呼叫 [`dump_entity_candidates`] 把
//! heap 找到的全部 entity(各 offset 名字 + target_id)印到 log,user 看真實
//! heap 內容後就能調 `/IT=<正確的字串>`。

use anyhow::{anyhow, Result};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Memory::{
    VirtualQueryEx, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_EXECUTE_READWRITE, PAGE_READONLY,
    PAGE_READWRITE, PAGE_WRITECOPY,
};

use crate::log_line;
use crate::memory::{read_bytes, read_u32};

/// Player class vfptr — local + remote + 自己 avatar + 召喚物(2026-05-03 RE 驗證)
const PLAYER_VFPTR: u32 = 0x008D_C08C;
/// Local player ptr 全域 — 從 `find_remote_entities.py` 沿用
const G_PLAYER_PTR: u32 = 0x00C2_D2B8;
/// Entity `+0x0C` = target_id(送 `cdd 0xA4` packet 第二個 d)
const OFFSET_TARGET_ID: u32 = 0x0C;
/// 三個 name offset 候選 — 不同 entity 類型 / 不同 deref 解出的字串可能在不同位置
const OFFSET_NAME_PTRS: [u32; 3] = [0x60, 0x64, 0x6C];
/// 名字最多 32 bytes(Lineage 3.8 角色名 16 Big5 char ≈ 32 bytes 上限)
const NAME_BUF_LEN: usize = 64;
/// VirtualQueryEx 起始位址 — heap 通常從 0x01000000 之後配置
const HEAP_SCAN_START: u32 = 0x0100_0000;
/// VirtualQueryEx 上限 — 32-bit 進程使用者空間頂(留 0x10000 邊界)
const HEAP_SCAN_END: u32 = 0x7FFF_0000;
/// 單次 region 讀取上限(避免一次吃太多記憶體;heap 通常每 region < 16MB)
const MAX_REGION_READ: usize = 0x100_0000;
/// 失敗時 dump 的 entity 上限
const MAX_DUMP_ENTITIES: usize = 12;

/// 找到的 entity — caller 用 `target_id` 組 packet。
#[derive(Debug, Clone)]
pub struct ScannedEntity {
    pub addr: u32,
    pub target_id: u32,
    /// 命中的名字字串(三個 offset 中任一精確 / starts_with 匹配的那個)
    pub name: String,
}

/// 從 `+0x60`、`+0x64`、`+0x6C` 三個欄位讀出來的 name 候選 — diagnostic 用。
#[derive(Debug, Clone)]
pub struct EntityNames {
    pub addr: u32,
    pub target_id: u32,
    pub names: [Option<String>; 3],
}

/// 掃 heap 找名字符合 `query` 的 entity。
///
/// 匹配優先序(LOCAL player 優先,讓 `/IT=自己名字` 打到自己而不是召喚物):
/// 1. **LOCAL player 精確匹配** — name == query → 回 LOCAL(target_id 是 client char_id)
/// 2. **REMOTE entity 精確匹配** — 任一 offset name == query
/// 3. **REMOTE entity starts_with fallback** — query 以某個 offset name 為前綴
///    (處理「Qwqqq456456的 魔熊」這種 user 輸入完整 display name 但 heap 只存 prefix)
///
/// 為什麼 LOCAL 優先:`/IT=自己名字` 通常是想對自己施放,但 LOCAL 跟自己的召喚物
/// `+0x6C` 同名(都是擁有者名)。LOCAL 優先後,要對召喚物施放就用 prefix 形式
/// `/IT=自己名字的<species>` 或挑指定召喚物名(starts_with fallback 處理)。
///
/// 失敗回 `Ok(None)`,caller 應呼叫 [`dump_entity_candidates`] 印 diagnostic。
pub fn find_entity_by_name(h: HANDLE, query: &str) -> Result<Option<ScannedEntity>> {
    let local_player = read_u32(h, G_PLAYER_PTR).unwrap_or(0);
    let needle = query.trim();
    if needle.is_empty() {
        return Err(anyhow!("entity name 不能空字串"));
    }

    // Pass 0:LOCAL player 精確匹配優先(自己永遠擺第一順位)
    if local_player != 0 {
        let info = read_entity_names(h, local_player);
        if let Some(matched) = info.names.iter().flatten().find(|n| n.trim() == needle) {
            return Ok(Some(ScannedEntity {
                addr: info.addr,
                target_id: info.target_id,
                name: matched.clone(),
            }));
        }
    }

    let candidates = enumerate_player_entities(h)?;

    // Pass 1:REMOTE entity 精確匹配
    for &entity_addr in &candidates {
        if entity_addr == local_player {
            continue;
        }
        let info = read_entity_names(h, entity_addr);
        if let Some(matched) = info.names.iter().flatten().find(|n| n.trim() == needle) {
            return Ok(Some(ScannedEntity {
                addr: info.addr,
                target_id: info.target_id,
                name: matched.clone(),
            }));
        }
    }

    // Pass 2:REMOTE starts_with fallback(query 以 heap 名為前綴)
    // 例:query "Qwqqq456456的 魔熊" 對應 heap "Qwqqq456456的"(+0x64)
    // min length 2 避免 "" / 單字 collision
    for &entity_addr in &candidates {
        if entity_addr == local_player {
            continue;
        }
        let info = read_entity_names(h, entity_addr);
        for n in info.names.iter().flatten() {
            let nt = n.trim();
            if nt.chars().count() >= 2 && needle.starts_with(nt) {
                return Ok(Some(ScannedEntity {
                    addr: info.addr,
                    target_id: info.target_id,
                    name: nt.to_string(),
                }));
            }
        }
    }

    Ok(None)
}

/// 失敗時 dump heap 找到的全部 entity 各 offset name(diagnostic)。
///
/// User 看 log 就能知道:
/// - 該玩家 entity 在不在 heap(在 → name 欄位錯位 / 不在 → vfptr 不同)
/// - 真正的 heap name 字串應該打什麼
pub fn dump_entity_candidates(h: HANDLE) {
    let local_player = read_u32(h, G_PLAYER_PTR).unwrap_or(0);
    let candidates = match enumerate_player_entities(h) {
        Ok(v) => v,
        Err(e) => {
            log_line!("[entity_scan] dump 失敗: enumerate 錯誤 {e:#}");
            return;
        }
    };

    log_line!(
        "[entity_scan] heap 找到 {} 個 vfptr=0x{:08X} entity(LOCAL player skipped),前 {} 筆 dump:",
        candidates.len(),
        PLAYER_VFPTR,
        MAX_DUMP_ENTITIES
    );
    let mut shown = 0;
    for &entity_addr in &candidates {
        if entity_addr == local_player {
            continue;
        }
        if shown >= MAX_DUMP_ENTITIES {
            break;
        }
        let info = read_entity_names(h, entity_addr);
        let n0 = info.names[0].as_deref().unwrap_or("(none)");
        let n1 = info.names[1].as_deref().unwrap_or("(none)");
        let n2 = info.names[2].as_deref().unwrap_or("(none)");
        log_line!(
            "[entity_scan]   @0x{:08X} target_id=0x{:08X}  +0x60={:?}  +0x64={:?}  +0x6C={:?}",
            info.addr,
            info.target_id,
            n0,
            n1,
            n2
        );
        shown += 1;
    }
    if shown == 0 {
        log_line!(
            "[entity_scan]   (heap 沒有 vfptr 0x{:08X} entity 或全是 LOCAL player)",
            PLAYER_VFPTR
        );
    }
}

/// 列出 heap 所有看起來像 player class entity 的位址(vfptr 對齊 + 命中)。
///
/// 不做進一步驗證 —— caller 用 `read_entity_names` 取詳細欄位。
fn enumerate_player_entities(h: HANDLE) -> Result<Vec<u32>> {
    let mut hits = Vec::new();
    let vfptr_le = PLAYER_VFPTR.to_le_bytes();
    let mut addr = HEAP_SCAN_START;

    while addr < HEAP_SCAN_END {
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        let ret = unsafe {
            VirtualQueryEx(
                h,
                Some(addr as *const _),
                &mut mbi,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if ret == 0 {
            // 失敗就跳一個 page 繼續(避免無限迴圈)
            addr = addr.checked_add(0x1000).unwrap_or(HEAP_SCAN_END);
            continue;
        }

        let region_base = mbi.BaseAddress as u32;
        let region_size = mbi.RegionSize.min(usize::MAX / 2);

        let is_committed = mbi.State == MEM_COMMIT;
        let is_readable = matches!(
            mbi.Protect,
            PAGE_READWRITE | PAGE_EXECUTE_READWRITE | PAGE_READONLY | PAGE_WRITECOPY
        );

        if is_committed && is_readable && region_size > 0 {
            let read_size = region_size.min(MAX_REGION_READ);
            if let Ok(buf) = read_bytes(h, region_base, read_size) {
                let mut i = 0;
                while i + 4 <= buf.len() {
                    if buf[i..i + 4] == vfptr_le {
                        let entity_addr = region_base.wrapping_add(i as u32);
                        // 4-byte alignment(vfptr 一定 aligned)
                        if entity_addr % 4 == 0 {
                            hits.push(entity_addr);
                        }
                    }
                    i += 4;
                }
            }
        }

        let next = region_base.wrapping_add(region_size as u32);
        if next <= addr {
            // 防止 region 報告 0 size 導致無限迴圈
            addr = addr.checked_add(0x1000).unwrap_or(HEAP_SCAN_END);
        } else {
            addr = next;
        }
    }

    Ok(hits)
}

/// 讀 entity 的 target_id + 三個 offset 的 name 字串(每個都嘗試 deref + decode)。
///
/// target_id = 0 時 names 還是會試(diagnostic 仍有用)— 但實際上 target_id=0
/// 通常是空 entity slot,name 也會 deref 失敗。
fn read_entity_names(h: HANDLE, entity_addr: u32) -> EntityNames {
    let target_id = read_u32(h, entity_addr + OFFSET_TARGET_ID).unwrap_or(0);
    let mut names: [Option<String>; 3] = Default::default();
    for (i, &off) in OFFSET_NAME_PTRS.iter().enumerate() {
        let name_ptr = read_u32(h, entity_addr + off).unwrap_or(0);
        if name_ptr == 0 {
            continue;
        }
        names[i] = decode_name_at(h, name_ptr);
    }
    EntityNames {
        addr: entity_addr,
        target_id,
        names,
    }
}

/// 讀 ASCII/Big5/GBK name 字串(null-terminated,最多 NAME_BUF_LEN bytes)。
///
/// 不純 ASCII 就用 legacy codepage auto detect(角色名常含中文)。
fn decode_name_at(h: HANDLE, str_ptr: u32) -> Option<String> {
    let raw = read_bytes(h, str_ptr, NAME_BUF_LEN).ok()?;
    let null_pos = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    if null_pos == 0 {
        return None;
    }
    let slice = &raw[..null_pos];
    // 先嘗試純 ASCII(快路徑)
    if slice.iter().all(|&b| (0x20..0x7F).contains(&b)) {
        return Some(String::from_utf8_lossy(slice).into_owned());
    }
    let decoded = crate::legacy_text::decode_zstr(slice);
    if decoded.chars().all(|c| c == char::REPLACEMENT_CHARACTER) {
        return None;
    }
    Some(decoded)
}
