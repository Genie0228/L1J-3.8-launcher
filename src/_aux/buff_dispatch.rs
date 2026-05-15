//! Buff/Skill/Item dispatch 輔助 — 從 `buff_tick` 抽出供 `timer_tick` 共用。
//!
//! 不負責: state byte 檢查、cooldowns、skill_cast_this_tick 標記。
//! 那些是 buff-specific gates，留在 caller。
//! 這裡只做「拿到 BuffItem，實際送出對應 game action」。

use parking_lot::RwLock;
use std::sync::Arc;
use windows::Win32::Foundation::HANDLE;

use crate::aux::drink_hook::DrinkHandle;
use crate::aux::runtime::BuffItem;
use crate::aux::spell_book::SpellBook;
use crate::aux::spell_db::SpellDb;
use crate::log_line;

/// Dispatch 共享 context — caller 拿一次給整 tick 用。
pub struct DispatchCtx<'a> {
    pub h: HANDLE,
    pub dh: &'a DrinkHandle,
    pub spell_book: &'a Arc<RwLock<Option<SpellBook>>>,
    pub spell_db: &'a Arc<RwLock<Option<SpellDb>>>,
}

/// Dispatch 結果 — caller 用此判斷要不要 set cooldown / skill_cast_this_tick。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// 物品 USE_ITEM 已送 / Info 已印 / DropItem fallback 已嘗試
    Done,
    /// 真有送技能 packet — caller 應 set skill_cast_this_tick
    SkillCast,
    /// silent skip(技能未學、背包找不到、cast_target 不支援等)
    Skipped(&'static str),
}

/// 執行 BuffItem — 不檢查 state byte / cooldown，純 dispatch。
///
/// caller 應自行做時間/狀態 gate。
pub fn execute_buff_item(ctx: &DispatchCtx<'_>, bi: &BuffItem) -> DispatchOutcome {
    match bi.item_type {
        'I' => dispatch_item(ctx, bi),
        'S' => dispatch_skill(ctx, bi),
        'K' => {
            // /KEY=Fn / /DKEY=Fn 屬於狀態頁 F1~F4 巨集系統,輔助/timer 都不觸發
            log_line!(
                "[dispatch] item={:?} cast_target={:?} (按鍵巨集屬狀態頁,不觸發)",
                bi.name,
                bi.cast_target
            );
            DispatchOutcome::Skipped("key_macro_skipped")
        }
        other => {
            log_line!("[dispatch] 未知 item_type {:?},skip", other);
            DispatchOutcome::Skipped("unknown_item_type")
        }
    }
}

fn dispatch_item(ctx: &DispatchCtx<'_>, bi: &BuffItem) -> DispatchOutcome {
    use crate::aux::runtime::CastTarget;
    let h = ctx.h;

    // Info debug：不執行任何 packet，只 dump 玩家狀態到 log
    if matches!(bi.cast_target, CastTarget::Info) {
        match crate::aux::player_state::read_player_state(h) {
            Ok(s) => log_line!(
                "[INFO] HP={}/{} MP={}/{} food={}% weight={}% map={}",
                s.hp,
                s.max_hp,
                s.mp,
                s.max_mp,
                s.food,
                s.weight,
                s.map_id
            ),
            Err(e) => log_line!("[INFO] read_player_state 失敗: {e:#}"),
        }
        return DispatchOutcome::Done;
    }

    // 對既有物品施放(IA/IW/I=name)— 走 SendPacketData("cdd", 0xA4, scroll, target)
    if matches!(
        bi.cast_target,
        CastTarget::OnInUseItem(_) | CastTarget::OnWieldedItem(_) | CastTarget::OnNamedItem(_)
    ) {
        return dispatch_use_on_item(ctx, bi);
    }

    // /IT=<entity名>(全自動)— scan heap 找 player class entity → cdd 0xA4 packet
    if matches!(bi.cast_target, CastTarget::OnNamedEntity(_)) {
        return dispatch_use_on_named_entity(ctx, bi);
    }

    // /IME(對自己施放卷軸)— 走 II packet target=self_char_id,跟 /IT=name 同 shellcode
    if matches!(bi.cast_target, CastTarget::SelfItem) {
        return dispatch_use_on_self(ctx, bi);
    }

    // /IT(無名)= 普通 USE_ITEM 快捷鍵:送 USE_ITEM(opcode 0x12),
    // game 進入目標選擇模式,玩家手動 click 目標完成施放。
    // 等同於右鍵卷軸 → Use,只是不用跑 menu。
    // 這裡刻意不處理 — 直接 fall through 走後面的普通 USE_ITEM 邏輯。

    // DropItem 警告後 fallback 到 USE_ITEM
    if matches!(bi.cast_target, CastTarget::DropItem) {
        log_line!(
            "[dispatch] item={:?} DropItem (ID) — 3.8 drop 函數未逆向，fallback USE_ITEM(可能無效)",
            bi.name
        );
    }

    // Item / DropItem / 其他：找背包同名物品，執行 USE_ITEM
    let items = match crate::aux::inventory::list_items(h) {
        Ok(v) => v,
        Err(e) => {
            log_line!("[dispatch] 讀背包失敗: {e:#}");
            return DispatchOutcome::Skipped("inventory_read_failed");
        }
    };

    let needle = crate::aux::lhx_window::clean_item_name(&bi.name);
    let it = match items
        .iter()
        .find(|it| crate::aux::lhx_window::clean_item_name(&it.name_lossy()) == needle)
    {
        Some(it) => it,
        None => {
            log_line!(
                "[dispatch] item={:?} 背包找不到對應物品(背包 {} 件)",
                bi.name,
                items.len()
            );
            return DispatchOutcome::Skipped("inventory_not_found");
        }
    };

    log_line!(
        "[dispatch] item={:?} → USE_ITEM entry=0x{:08X}",
        bi.name,
        it.entry_addr
    );

    if let Err(e) = ctx.dh.execute_use_item(h, it.entry_addr) {
        log_line!("[dispatch] execute_use_item 失敗: {e:#}");
        return DispatchOutcome::Skipped("execute_use_item_err");
    }

    DispatchOutcome::Done
}

/// 對既有物品施放卷軸 / 工具 — IA/IW/I=name 路徑(II packet, opcode 0xA4)。
///
/// 為什麼要這個 helper:卷軸/工具類道具不能直接 use_item,server 要求一個額外的
/// target item 參數(例如祝福卷軸要指定要祝福的武器)。封包格式為
/// `SendPacketData("cdd", 0xA4, scroll_param, target_param)`:
///   - 第一個 d = 來源(卷軸/工具 obj_id)
///   - 第二個 d = 目標(使用中防具 / 揮舞武器 / 指定物品 obj_id)
fn dispatch_use_on_item(ctx: &DispatchCtx<'_>, bi: &BuffItem) -> DispatchOutcome {
    use crate::aux::runtime::CastTarget;
    let h = ctx.h;

    let items = match crate::aux::inventory::list_items(h) {
        Ok(v) => v,
        Err(e) => {
            log_line!("[dispatch] 讀背包失敗(IA/IW/I=name): {e:#}");
            return DispatchOutcome::Skipped("inventory_read_failed");
        }
    };

    // 1. 找來源物品(卷軸/工具)— 用 bi.name 配對
    let needle = crate::aux::lhx_window::clean_item_name(&bi.name);
    let scroll = match items
        .iter()
        .find(|it| crate::aux::lhx_window::clean_item_name(&it.name_lossy()) == needle)
    {
        Some(it) => it,
        None => {
            log_line!(
                "[dispatch] item={:?}(IA/IW/I=) 背包找不到來源物品(背包 {} 件)",
                bi.name,
                items.len()
            );
            return DispatchOutcome::Skipped("source_item_not_found");
        }
    };

    // 2. 找目標物品 — 依 cast_target 走不同條件
    //    OnInUseItem/OnWieldedItem 的 Option<String> 是名字過濾:
    //      None    → 找第一件含「(使用中)」/「(揮舞)」的物品
    //      Some(n) → 找名字(clean_item_name 後)= n 且狀態符合的物品
    let target = match &bi.cast_target {
        CastTarget::OnInUseItem(filter) => {
            let in_use = |it: &&crate::aux::inventory::Item| it.name_lossy().contains("(使用中)");
            match filter {
                None => items.iter().find(in_use),
                Some(n) => {
                    let nd = crate::aux::lhx_window::clean_item_name(n);
                    items.iter().find(|it| {
                        in_use(it)
                            && crate::aux::lhx_window::clean_item_name(&it.name_lossy()) == nd
                    })
                }
            }
        }
        CastTarget::OnWieldedItem(filter) => {
            let wielded = |it: &&crate::aux::inventory::Item| it.name_lossy().contains("(揮舞)");
            match filter {
                None => items.iter().find(wielded),
                Some(n) => {
                    let nd = crate::aux::lhx_window::clean_item_name(n);
                    items.iter().find(|it| {
                        wielded(it)
                            && crate::aux::lhx_window::clean_item_name(&it.name_lossy()) == nd
                    })
                }
            }
        }
        CastTarget::OnNamedItem(n) => {
            let nd = crate::aux::lhx_window::clean_item_name(n);
            items
                .iter()
                .find(|it| crate::aux::lhx_window::clean_item_name(&it.name_lossy()) == nd)
        }
        _ => None,
    };
    let target = match target {
        Some(it) => it,
        None => {
            log_line!(
                "[dispatch] item={:?} cast_target={:?} 背包找不到目標物品(背包 {} 件)",
                bi.name,
                bi.cast_target,
                items.len()
            );
            return DispatchOutcome::Skipped("target_item_not_found");
        }
    };

    log_line!(
        "[dispatch] item={:?} ({:?}) → 對 {:?}(item_param=0x{:08X}) 使用 (II packet)",
        bi.name,
        bi.cast_target,
        target.name_lossy(),
        target.item_param
    );

    if let Err(e) = ctx
        .dh
        .execute_use_on_wielded(h, scroll.item_param, target.item_param)
    {
        log_line!("[dispatch] execute_use_on_wielded 失敗: {e:#}");
        return DispatchOutcome::Skipped("execute_use_on_wielded_err");
    }

    DispatchOutcome::Done
}

/// 對指定名字的 entity 施放卷軸 — `/IT=<entity名>` 全自動路徑。
///
/// 走 entity scan(`aux::entity_scan`)找 player class entity,讀 `+0x0C` target_id
/// 後送 `SendPacketData("cdd", 0xA4, scroll, target_id)`。重用 [`execute_use_on_wielded`]
/// shellcode — 同樣 II packet 格式,只是第二個 d 不是 item_param 而是 entity target_id。
///
/// 行為:
/// - 背包找不到來源卷軸 → silent skip(`source_item_not_found`)
/// - heap scan 找不到名字 entity → silent skip(`entity_not_found`)
/// - server 接受度視 entity class 而定(2026-05-03 已驗證 player class vfptr `0x008DC08C`
///   可送;召喚物 / 怪物的 packet 路徑可能不同,實機驗證再決定)
fn dispatch_use_on_named_entity(ctx: &DispatchCtx<'_>, bi: &BuffItem) -> DispatchOutcome {
    use crate::aux::runtime::CastTarget;
    let h = ctx.h;

    let entity_name = match &bi.cast_target {
        CastTarget::OnNamedEntity(n) => n,
        _ => unreachable!(),
    };

    // 1. 背包找來源卷軸
    let items = match crate::aux::inventory::list_items(h) {
        Ok(v) => v,
        Err(e) => {
            log_line!("[dispatch] 讀背包失敗(IT=name): {e:#}");
            return DispatchOutcome::Skipped("inventory_read_failed");
        }
    };
    let needle = crate::aux::lhx_window::clean_item_name(&bi.name);
    let scroll = match items
        .iter()
        .find(|it| crate::aux::lhx_window::clean_item_name(&it.name_lossy()) == needle)
    {
        Some(it) => it,
        None => {
            log_line!(
                "[dispatch] item={:?}(IT=) 背包找不到來源卷軸(背包 {} 件)",
                bi.name,
                items.len()
            );
            return DispatchOutcome::Skipped("source_item_not_found");
        }
    };

    // 2. heap 掃 entity by name
    let entity = match crate::aux::entity_scan::find_entity_by_name(h, entity_name) {
        Ok(Some(e)) => e,
        Ok(None) => {
            log_line!(
                "[dispatch] item={:?}/IT={} entity scan 找不到該名字 — dump heap 候選讓 user 看實際 name 字串",
                bi.name, entity_name
            );
            crate::aux::entity_scan::dump_entity_candidates(h);
            return DispatchOutcome::Skipped("entity_not_found");
        }
        Err(e) => {
            log_line!("[dispatch] entity scan 失敗: {e:#}");
            return DispatchOutcome::Skipped("entity_scan_failed");
        }
    };

    log_line!(
        "[dispatch] item={:?}/IT={} → entity@0x{:08X} target_id=0x{:08X} (II packet)",
        bi.name,
        entity.name,
        entity.addr,
        entity.target_id
    );

    if let Err(e) = ctx
        .dh
        .execute_use_on_wielded(h, scroll.item_param, entity.target_id)
    {
        log_line!("[dispatch] execute_use_on_wielded(entity) 失敗: {e:#}");
        return DispatchOutcome::Skipped("execute_use_on_wielded_err");
    }

    DispatchOutcome::Done
}

/// 對自己施放卷軸 — `/IME` 路徑。
///
/// 走跟 `/IT=name` 同一個 II packet shellcode(`execute_use_on_wielded`),只是
/// target_id 不掃 entity 直接讀 `[0xABF4B4]`(自己 char_id)。
///
/// **Why**:USE_ITEM 0x12 對需 target 的卷軸(治癒卷軸等)只進「目標選擇模式」,
/// 不送施放 packet → server 看到沒完成 cast 回 `施咒失敗`。直接送 0xA4 II packet
/// `(scroll_param, self_char_id)` 等同於玩家手動點完目標的最終 packet。
///
/// 行為:
/// - 背包找不到卷軸 → silent skip(`source_item_not_found`)
/// - 讀 `[0xABF4B4]` 失敗(進場前 / packer 還沒寫入)→ silent skip(`self_char_id_unavailable`)
/// - char_id 為 0 → silent skip(同上,進場前該位址內容是 0)
fn dispatch_use_on_self(ctx: &DispatchCtx<'_>, bi: &BuffItem) -> DispatchOutcome {
    let h = ctx.h;

    // 1. 背包找來源卷軸
    let items = match crate::aux::inventory::list_items(h) {
        Ok(v) => v,
        Err(e) => {
            log_line!("[dispatch] 讀背包失敗(IME): {e:#}");
            return DispatchOutcome::Skipped("inventory_read_failed");
        }
    };
    let needle = crate::aux::lhx_window::clean_item_name(&bi.name);
    let scroll = match items
        .iter()
        .find(|it| crate::aux::lhx_window::clean_item_name(&it.name_lossy()) == needle)
    {
        Some(it) => it,
        None => {
            log_line!(
                "[dispatch] item={:?}/IME 背包找不到來源卷軸(背包 {} 件)",
                bi.name,
                items.len()
            );
            return DispatchOutcome::Skipped("source_item_not_found");
        }
    };

    // 2. 讀自己 char_id ([0xABF4B4],進場後才有效)
    let self_char_id = match crate::memory::read_u32(h, 0x00ABF4B4) {
        Ok(v) => v,
        Err(e) => {
            log_line!("[dispatch] item={:?}/IME 讀自己 char_id 失敗: {e:#}", bi.name);
            return DispatchOutcome::Skipped("self_char_id_unavailable");
        }
    };
    if self_char_id == 0 {
        log_line!(
            "[dispatch] item={:?}/IME [0xABF4B4]=0(進場前/未填入),skip",
            bi.name
        );
        return DispatchOutcome::Skipped("self_char_id_unavailable");
    }

    log_line!(
        "[dispatch] item={:?}/IME → self char_id=0x{:08X} scroll_param=0x{:08X} (II packet)",
        bi.name,
        self_char_id,
        scroll.item_param
    );

    if let Err(e) = ctx
        .dh
        .execute_use_on_wielded(h, scroll.item_param, self_char_id)
    {
        log_line!("[dispatch] execute_use_on_wielded(self) 失敗: {e:#}");
        return DispatchOutcome::Skipped("execute_use_on_wielded_err");
    }

    DispatchOutcome::Done
}

fn dispatch_skill(ctx: &DispatchCtx<'_>, bi: &BuffItem) -> DispatchOutcome {
    use crate::aux::drink_hook::SkillTargetMode;
    use crate::aux::runtime::CastTarget;
    let h = ctx.h;

    // 確保 spell_book / spell_db 都 build 過 — timer/hotkey/buff_tick 共用入口,
    // 任何一條路徑先進來都能觸發 build,後面的 caller 直接 hit cache。
    let _ = crate::aux::spell_book::ensure_fresh(h, ctx.spell_book, "dispatch");
    let _ = crate::aux::spell_db::ensure_built(h, ctx.spell_db, "dispatch");

    // packed lookup：spell_book 優先(玩家實際 level)，fallback spell_db(level 1)
    let packed = {
        let book = ctx
            .spell_book
            .read()
            .as_ref()
            .and_then(|b| b.lookup(&bi.name));
        match book {
            Some(p) => p,
            None => match ctx
                .spell_db
                .read()
                .as_ref()
                .and_then(|d| d.lookup(&bi.name))
            {
                Some(p) => p,
                None => {
                    let book_size = ctx
                        .spell_book
                        .read()
                        .as_ref()
                        .map(|b| b.unique_names())
                        .unwrap_or(0);
                    let db_size = ctx
                        .spell_db
                        .read()
                        .as_ref()
                        .map(|d| d.unique_names())
                        .unwrap_or(0);
                    let needle_first_char = bi.name.chars().next();
                    let book_candidates: Vec<String> = ctx
                        .spell_book
                        .read()
                        .as_ref()
                        .map(|b| {
                            b.names_iter()
                                .filter(|n| {
                                    needle_first_char.is_some_and(|c| n.starts_with(c))
                                        || n.contains(&bi.name[..])
                                })
                                .take(8)
                                .map(|s| s.to_string())
                                .collect()
                        })
                        .unwrap_or_default();
                    let db_candidates: Vec<String> = ctx
                        .spell_db
                        .read()
                        .as_ref()
                        .map(|d| {
                            d.names_iter()
                                .filter(|n| {
                                    needle_first_char.is_some_and(|c| n.starts_with(c))
                                        || n.contains(&bi.name[..])
                                })
                                .take(8)
                                .map(|s| s.to_string())
                                .collect()
                        })
                        .unwrap_or_default();
                    log_line!(
                        "[dispatch] skill={:?} 查不到 — spell_book(size={}) candidates={:?} | spell_db(size={}) candidates={:?}",
                        bi.name,
                        book_size,
                        book_candidates,
                        db_size,
                        db_candidates
                    );
                    return DispatchOutcome::Skipped("spell_lookup_failed");
                }
            },
        }
    };

    // 依 cast_target 選 SkillTargetMode
    let target_mode = match &bi.cast_target {
        CastTarget::Self_ => SkillTargetMode::ForceSelfPacket,
        CastTarget::NoSpec => SkillTargetMode::NoSpec,
        // /MT(鼠標當下目標)在 3.8 退化為 NoSpec —
        // 為什麼:3.8 沒有「滑鼠 hover entity」全域可抓(`[0x97C910]` 全 process 沒人寫,
        // dispatcher 0x73C260 也不走)。送 NoSpec 讓 server 視為「不指定 target」由
        // session context 推斷,避免回傳 invalid target error 把整條 cast 鎖死。
        CastTarget::HoverTarget => SkillTargetMode::NoSpec,
        // MIA / MIW / MI=name → 對物品施法，target = item_param(obj_id)
        CastTarget::OnInUseItem(_) | CastTarget::OnWieldedItem(_) | CastTarget::OnNamedItem(_) => {
            let items = match crate::aux::inventory::list_items(h) {
                Ok(v) => v,
                Err(e) => {
                    log_line!("[dispatch] 讀背包失敗(MI/MIA/MIW): {e:#}");
                    return DispatchOutcome::Skipped("inventory_read_failed");
                }
            };
            // 技能系 MIA/MIW 目前不支援 name 過濾(只挑第一件符合狀態)— 若需要再擴充
            let target_item = match &bi.cast_target {
                CastTarget::OnInUseItem(filter) => {
                    let in_use =
                        |it: &&crate::aux::inventory::Item| it.name_lossy().contains("(使用中)");
                    match filter {
                        None => items.iter().find(in_use),
                        Some(n) => {
                            let nd = crate::aux::lhx_window::clean_item_name(n);
                            items.iter().find(|it| {
                                in_use(it)
                                    && crate::aux::lhx_window::clean_item_name(&it.name_lossy())
                                        == nd
                            })
                        }
                    }
                }
                CastTarget::OnWieldedItem(filter) => {
                    let wielded =
                        |it: &&crate::aux::inventory::Item| it.name_lossy().contains("(揮舞)");
                    match filter {
                        None => items.iter().find(wielded),
                        Some(n) => {
                            let nd = crate::aux::lhx_window::clean_item_name(n);
                            items.iter().find(|it| {
                                wielded(it)
                                    && crate::aux::lhx_window::clean_item_name(&it.name_lossy())
                                        == nd
                            })
                        }
                    }
                }
                CastTarget::OnNamedItem(n) => {
                    let needle = crate::aux::lhx_window::clean_item_name(n);
                    items.iter().find(|it| {
                        crate::aux::lhx_window::clean_item_name(&it.name_lossy()) == needle
                    })
                }
                _ => None,
            };
            let it = match target_item {
                Some(it) => it,
                None => {
                    log_line!(
                        "[dispatch] skill={:?} cast_target={:?} 背包找不到目標物品(背包 {} 件)",
                        bi.name,
                        bi.cast_target,
                        items.len()
                    );
                    return DispatchOutcome::Skipped("target_item_not_found");
                }
            };
            log_line!(
                "[dispatch] skill={:?} cast_target={:?} → 對 {:?}(item_param=0x{:08X}) 施法",
                bi.name,
                bi.cast_target,
                it.name_lossy(),
                it.item_param
            );
            // 為什麼要 bypass spell_book_cast 直送 packet:
            // item_param 是 inventory namespace 的 obj_id,不是世界 entity id —
            // 走 spell_book_cast 會在 0x73C2EC 的 find_entity_by_id 失敗、掉到 UI hover
            // fallback 而 cast 不出去。直接組 C_SKILL packet 把 item_param 當 target_id
            // 送出去,讓 server 端 item 路徑解析,避開 client 的 entity lookup。
            SkillTargetMode::ForceTargetPacket(it.item_param)
        }
        other => {
            log_line!(
                "[dispatch] skill={:?} 不應有 cast_target={:?}",
                bi.name,
                other
            );
            return DispatchOutcome::Skipped("invalid_cast_target");
        }
    };

    log_line!(
        "[dispatch] skill={:?} packed=0x{:X} mode={:?} → 施法",
        bi.name,
        packed,
        target_mode
    );
    if let Err(e) = ctx.dh.execute_skill(h, packed, target_mode) {
        log_line!("[dispatch] execute_skill 失敗: {e:#}");
        return DispatchOutcome::Skipped("execute_skill_err");
    }
    DispatchOutcome::SkillCast
}
