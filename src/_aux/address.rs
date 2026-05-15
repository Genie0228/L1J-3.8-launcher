//! 3.8 版輔助功能位址常數
//!
//! 為什麼集中在這檔案:避免位址 hardcode 到各 hook 模組裡造成搜尋困難。
//! 每個常數附信度等級,呼叫端可決定是否套用(★★ 以下禁止 Release 啟用)。
//!
//! 信度標示：
//!   ★★★★★ 多次運行時驗證
//!   ★★★★  靜態 + 動態驗證
//!   ★★★   單次驗證或反組譯確認
//!   ★★    候選位址(未經運行時驗證,禁止 Release 啟用)
//!   ★     純猜測(禁止使用)
//!
//! 已驗證位址來源:memory/game_database.md + hp_mp_patch.rs 啟動鏈
//! Option<u32> = None 表示位址尚未抓到,呼叫端需做 None 守衛 fail-soft

// ════════════════════════════════════════════════
// 已驗證 3.8 位址（直接套用，不需重抓）
// ════════════════════════════════════════════════

/// g_mapId：當前地圖 ID（dword）★★★★
pub const G_MAP_ID: u32 = 0x00965B60;

/// g_gameState：遊戲狀態（1=登入 2=選角 3=遊戲中 9=斷線）★★★★
pub const G_GAME_STATE: u32 = 0x009AB5E8;

/// g_lightLevel：光照等級 byte（全白天用，寫 0xFA）★★★★★
pub const G_LIGHT_LEVEL: u32 = 0x00C31EAE;

/// maxHP：最大 HP（dword，hp_mp_patch 已修為 32-bit）★★★★★
pub const MAX_HP: u32 = 0x00C31E90;

/// maxMP：最大 MP（dword）★★★★★
pub const MAX_MP: u32 = 0x00C31E8C;

// .data 角色屬性區塊(0xC31E80~0xC31E93) ★★★★★ (2026-04-28 驗證)
// 連續 6 byte 為六大屬性,實機 attach 比對使用者報告值已對齊
pub const STAT_STR: u32 = 0x00C31E80;
pub const STAT_INT: u32 = 0x00C31E81;
pub const STAT_WIS: u32 = 0x00C31E82;
pub const STAT_DEX: u32 = 0x00C31E83;
pub const STAT_CON: u32 = 0x00C31E84;
pub const STAT_CHA: u32 = 0x00C31E85;
/// 正義值 Lawful — 2-byte signed short,負值代表 chaotic
pub const STAT_LAWFUL: u32 = 0x00C31E88;

/// g_playerPtr：玩家物件指標 ★★★★
pub const G_PLAYER_PTR: u32 = 0x00C2D2B8;

/// g_hasteBuffTable：加速 buff 表（4 bytes）★★★★
pub const G_HASTE_BUFF_TABLE: u32 = 0x00ABF4C8;

/// 玩家職業 byte ★★★★★ (0=君主, 1=皇族, 2=戰士, 3=法師, 4=妖精, 6=幻術師)
///
/// 為什麼需要:apply_buff(0x4EE400) 在 ~10 個 packet 路徑會依職業套不同 byte_table state_id。
/// 例 sub_type=4 buff(行走加速):戰士寫 byte[24],法師寫 byte[42],其他職業寫 byte[37]。
/// 來源 INI 寫的「行走加速=37」是「其他職業」視角,法師玩家上需 remap 成 42 才能正確
/// 偵測既有 buff 避免重複施放。完整對映表見 [`crate::aux::class_remap`]。
pub const G_CLASS: u32 = 0x00C31544;

/// SendPacketData：高層封包發送函數 ★★★★★
pub const SEND_PACKET_DATA: u32 = 0x00580E50;

/// ProcessPacket：封包分發入口 ★★★★★
pub const PROCESS_PACKET: u32 = 0x00539333;

/// cast_magic：施法 dispatcher(高層) ★★★★★ (2026-04-28 驗證,加速術 packed=42)
///
/// 原型:`void __cdecl cast_magic(u32 packed_skill_id, u8 byte_flag)`
/// - `packed_skill_id` = spell DB 陣列 index(見 [`SPELL_DB_PTR`])
/// - `byte_flag` = 0 一般單體/自身,非 0 為 AOE/範圍模式
///
/// 內部依遊戲狀態自動分派(無目標→自身,鎖目標→指定目標,範圍→AOE)。
/// **launcher 不直接 call 此函數** — self path 在 0x73C449 走 `is_ready` 檢查
/// `[magic_info+8]` 必須非 0(玩家點過技能書才會),launcher 條件不符會被擋下。
/// 改走 [`DO_CAST`] 直通方案。
pub const CAST_MAGIC: u32 = 0x0073C260;

/// spell_book::cast (thiscall) ★★★★★ (2026-04-28 實機追蹤鎖定)
///
/// 原型:`void __thiscall spell_book_cast(spell_book* this, u32 packed, u32 byte_flag)`
///   ret 8 (callee 自清)
///
/// 這是**手動施法的真正入口** — 跟玩家點技能書再點目標走的同一條路徑:
/// 1. 走訪 `this->m_spells[i]`(`this+0x58` 陣列,`this+0x2C` 數量)找 `entry+4 == packed`
/// 2. 驗證 MP/range/狀態
/// 3. 設定 `[magic_info+5/+C/+10]` 等 begin_cast 欄位
/// 4. 透過 `SendPacketData` 送出 cast request 封包到 server
/// 5. server 套 buff + 廣播動畫封包回來 — 客戶端動畫由那條 path 觸發
///
/// 直接 call do_cast (0x734A90) 不會觸發以上整條鏈;它只是 setter,
/// 主迴圈 magic_tick (0x734AC0) 處理時若 `[+8]` (target_id) 為 0 就 reset 走人。
///
/// `byte_flag = 1` 是手動 manual cast 用的標誌;`= 0` 是 magic_tick 自呼叫(begin_cast 後)。
/// launcher 應該用 1。
pub const SPELL_BOOK_CAST: u32 = 0x0073ECE0;

/// 玩家的 spell_book 物件指標 (heap)— `this` 參數來源 ★★★★★
///
/// `[0xC31324]` 在進場後填入,跨重啟 heap addr 會變但**陣列內容不變**。
pub const SPELL_BOOK_PTR: u32 = 0x00C31324;

/// spell DB 指標 ★★★★★ (2026-04-28 驗證)
///
/// `[SPELL_DB_PTR]` = heap 上的指標陣列基址,index = packed_skill_id,
/// 每 entry 4 bytes 指向 spell record(以 Big5 技能名開頭,後接 metadata)。
///
/// 用法:scan 0..256,讀名稱配對 INI [AllState] 條目,得 `state_id → packed` 對映。
pub const SPELL_DB_PTR: u32 = 0x009A8ED4;

/// 玩家物件偏移（從 g_playerPtr 取指標後）
pub mod player_offset {
    /// action_state（49=idle, 4=walk, 0=between, 8=transparent）★★★★★
    pub const ACTION_STATE: u32 = 0x14;
    /// direction（0~7 八方向）★★★★★
    pub const DIRECTION: u32 = 0x15;
    /// anim_frame（動畫幀計數）★★★★★
    pub const ANIM_FRAME: u32 = 0x17;
    /// sprite_id（精靈 ID）★★★★★
    pub const SPRITE_ID: u32 = 0x18;
    /// haste_low：綠水加速 buff ★★★★★
    pub const HASTE_LOW: u32 = 0x24;
    /// haste_high：高段加速 buff ★★★★★
    pub const HASTE_HIGH: u32 = 0x29;
    /// map_id：與 g_mapId 同步 ★★★★★
    pub const MAP_ID: u32 = 0x80;
}

// ════════════════════════════════════════════════
// 角色狀態位址(補水系統用)
// ════════════════════════════════════════════════

/// 當前 HP — XOR 加密物件(12 bytes)位址 ★★★★★(2026-04-28 驗證)
///
/// 物件結構:
/// - this+0 (4B) = encrypted_index(XOR with [`STAT_XOR_MAGIC`] 取得 0..15 範圍 plain_index)
/// - this+4 (4B) = key_array_ptr(指向 heap 上的 16 個 dword 金鑰陣列)
/// - this+8 (4B) = salt(每次 setter 呼叫重新隨機)
///
/// 解密公式:
/// ```text
/// plain_idx = read_u32(this+0) ^ STAT_XOR_MAGIC
/// key_ptr   = read_u32(this+4)
/// salt      = read_u32(this+8)
/// enc_value = read_u32(key_ptr + plain_idx*4)
/// value     = enc_value ^ salt
/// ```
///
/// XOR 加密 setter 在 0x579E10,reflection 自 hp_mp_patch.rs (S_STATUS curHP→setter)。
pub const CURRENT_HP_OBJ: u32 = 0x00BDC828;
/// 當前 MP — 同上 XOR 加密物件 ★★★★★(2026-04-28 驗證)
pub const CURRENT_MP_OBJ: u32 = 0x00BDC834;
/// XOR 解密用的常數 magic value
pub const STAT_XOR_MAGIC: u32 = 0xC001_7921;

/// 飽食度 raw byte ★★★★★(2026-04-28 驗證:127→52%, 130→57% 對應公式)
///
/// 顯示百分比 = `raw * 100 / 225`
/// 在 .data 區的角色屬性區塊內,跟 STR/INT/WIS/DEX/CON/CHA 同連續 6 byte 之後
pub const FOOD_LEVEL: Option<u32> = Some(0x00C31E8B);
/// 飽食度顯示用分母 — `raw / 225 * 100` 得百分比顯示
pub const FOOD_LEVEL_DIVISOR: u32 = 225;

/// 負重度 raw byte ★★★★★(2026-04-28 驗證)
///
/// 顯示百分比 = `raw * 100 / 240`
pub const CURRENT_WEIGHT: Option<u32> = Some(0x00C31E8A);
/// 負重度顯示用分母
pub const WEIGHT_DIVISOR: u32 = 240;

/// 當前經驗值 — 3.8 位址待重抓
///
/// 為什麼空著:exp_tracker 目前讀 `0x00C31EA4` cumulative,即時 delta exp 可以
/// 從 cumulative 差分算出,不一定需要此常數。若未來要做 exp/小時即時顯示再補。
pub const CURRENT_EXP: Option<u32> = None;

/// 物品欄基址 ★★★★ (2026-04-28 Ghidra 靜態驗證)
///
/// 結構:`[INVENTORY_BASE]` = 物品欄物件指標(heap)
/// - inv+0x2C (i32) = 物品數量
/// - inv+0x58 (ptr) = 物品指標陣列(每元素 4 bytes,指向 item_entry)
/// - 每個 item_entry 結構參考 `inventory.rs::ITEM_*` 偏移
///
/// 出處:0x40F915 `MOV ECX, [0x9A7230]` → 0x40F91B `CALL 0x4B1E50`(item lookup),
/// 0x4B1E50 內 `[this+0x2C]` 走訪 `[this+0x58][i*4]` 比對 `[item+4]==item_param`
pub const INVENTORY_BASE: Option<u32> = Some(0x009A9250);

/// item_entry 結構偏移
pub mod item_offset {
    /// 物品 ID(server-assigned param,4 bytes)— FUN_004B1E50 用此找物品
    pub const ITEM_PARAM: u32 = 0x04;
    /// 是否存在(1=有效)
    pub const VALID: u32 = 0x08;
    /// 是否裝備中(1=已裝備)— FUN_004B41C0 過濾條件 `[+8]!=0 && [+9]==0`
    pub const EQUIPPED: u32 = 0x09;
    /// 物品名稱字串指標
    pub const NAME_PTR: u32 = 0x0C;
    /// 物品類型 byte(switch dispatcher 的 case key,例如 0x01=藥水)
    pub const ITEM_TYPE: u32 = 0x98;
    /// 動畫 / icon 編號(short)
    pub const ICON_NUM: u32 = 0x9A;
    /// 堆疊數量 dword ★★★★★(2026-05-02 Frida 驗證)
    ///
    /// 對 stack 物品:= 當前堆疊數量(例 綠色藥水 365 → +0xA0 = 0x16D)。
    /// 對非堆疊物品:= 0 或 1,送 delete 時直接代進 quantity 欄位。
    ///
    /// **3.8 踩過的坑**:`entry+0xA4` 在 3.8 是 enchant level / charges 不是 count,
    /// 若誤用 +0xA4 當數量送 delete 會刪錯數量。必須用 +0xA0 才會被 server
    /// 正確識別為「整疊刪」:`SendPacketData("cdd", 0x8A, obj_id, [+0xA0])`。
    pub const ITEM_COUNT: u32 = 0xA0;
}

// ════════════════════════════════════════════════
// 封包 Opcode（C→S）
// ════════════════════════════════════════════════
// 來源：memory/opcode_tables.md

/// C_USE_ITEM 物品使用 opcode（待從 opcode_tables.md 確認）
pub const C_USE_ITEM: Option<u8> = None;
/// C_SKILL 技能施放 opcode
pub const C_SKILL: Option<u8> = None;
/// C_DELETE_ITEM 刪除物品 opcode ★★★★★ (2026-05-02 Frida capture 確認)
///
/// 觸發路徑:玩家把背包道具拖到背包視窗右下角的「垃圾桶」icon → 直接從 server 移除。
/// (跟「丟到地上」是不同封包 — 丟地上是 drop,垃圾桶是 delete。)
///
/// SendPacketData 格式:
/// ```text
/// SendPacketData("cdd", 0x8A, item.item_param, 0)
/// ```
/// - c (1B) = opcode 0x8A
/// - d (4B) = item.item_param (obj_id)
/// - d (4B) = 0 (推測為 quantity,垃圾桶整疊刪所以填 0)
///
/// Frida capture 範例(2026-05-02):
///   `#13 fmt='cdd' opcode=0x8a args=[0x1dcd6aa8 0x00000000 ...]`
pub const C_DELETE_ITEM: Option<u8> = Some(0x8A);
/// C_REFINE 精煉/合成 opcode（已知）★★★★★
pub const C_REFINE: u8 = 0x4E;

/// C_CHAT 喊話 / 一般聊天 opcode ★★★★★(2026-05-02 Frida capture 確認)
///
/// fmt `"ccs"` — opcode + channel byte + null-terminated 訊息字串(Big5 編碼)。
/// channel 值:
/// - `0x00` = 一般訊息(綠字,區域內) ← `shout_tick` 實際使用此 channel
/// - `0x02` = 喊話(全頻廣播)
///
/// 同 opcode 也用在 dialog reply(`0x88 ccs [button, typed_text]`),
/// 是泛用「client text input」封包,server 依 channel/context byte 分流。
pub const C_CHAT_OPCODE: u8 = 0x88;

/// 喊話 channel byte
pub const CHAT_CHANNEL_SHOUT: u8 = 0x02;

/// 一般對話 channel byte ← shout_tick 實際使用此 channel
pub const CHAT_CHANNEL_NORMAL: u8 = 0x00;
