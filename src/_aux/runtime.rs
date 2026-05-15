//! 輔助功能執行緒框架 — AuxScheduler
//!
//! 為什麼:每個輔助功能(自動喝水/補 buff/解毒/吃肉/磨刀石/變身)都需要 polling
//! 才能即時反應遊戲狀態,單條 thread 跑全部 tick 比一功能一 thread 簡單且耗能低。
//! GUI 修改設定 → 寫 AuxSettings(RwLock)→ polling thread 下次 tick 自動讀新值,
//! user 不需要重啟 launcher 也不需要重 attach 遊戲。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::RwLock;
use windows::Win32::Foundation::HANDLE;

use crate::log_line;

/// 喝水分頁的單一 row：HP 閾值 + 物品
#[derive(Default, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PotionRow {
    pub enabled: bool,
    pub threshold: u32,
    pub item: String,
}

/// 洗魔規則：HP >= lower && MP <= upper → 用 item
#[derive(Default, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MpWhenSafe {
    pub enabled: bool,
    pub hp_lower: u32,
    pub mp_upper: u32,
    pub item: String,
}

/// 施法/物品目標的 suffix 系統 — 描述「對誰使用」的列舉。
///
/// INI 條目格式:`<name>_<id_or_target>_<suffix>`(3 段底線分隔)。
///
/// 為什麼用 enum 而非字串:user 設定階段就把字串解析掉,buff_tick 拿到 CastTarget
/// 直接 match,避免 polling thread 反覆 parse 同一條 INI(每 tick 100ms 級頻率)。
///
/// 第二段(id_or_target)雙重身分:
/// - 一般情況:state byte 索引(`-1` = 未指定,`buff_array[id]` 0/1)
/// - `MI` `II` `IP`:目標物品/玩家名稱(放在這個位置給 dispatcher 使用)
///
/// 範例:
/// - `保護罩_-1_MME` (對自己施法)
/// - `提煉魔石_紅魔石_MI` (對紅魔石施提煉魔石,target name 在 id 位置)
/// - `肉_-1_I` (吃肉)
///
/// suffix → behavior 對照表:
///
/// **魔法系**:
/// - `M`    → [`CastTarget::NoSpec`]      不指定 target(packet 不送 target 欄位)
/// - `MME`  → [`CastTarget::Self_`]       對自己(target = self char_id)
/// - `MT`   → [`CastTarget::HoverTarget`] 對鼠標當下目標
/// - `MIA`  → [`CastTarget::OnInUseItem`] 對「(使用中)」物品施法
/// - `MIW`  → [`CastTarget::OnWieldedItem`] 對「(揮舞)」物品施法
/// - `MI`   → [`CastTarget::OnNamedItem`](name) 對指定名稱物品施法
///
/// **物品系**:
/// - `I`(或無 suffix) → [`CastTarget::Item`] 普通物品 USE_ITEM(自喝藥水/卷軸/補品)
/// - `ID`   → [`CastTarget::DropItem`]       銷毀/丟棄物品
///
/// **debug**:
/// - `INFO` → [`CastTarget::Info`] 印 spr/buff state 到對話框
///
/// **狀態頁(輔助 buff_tick 不觸發)**:
/// - `KEY=F<n>`  → [`CastTarget::Key`](n)
/// - `DKEY=F<n>` → [`CastTarget::DelayKey`](n)
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum CastTarget {
    /// `I` 或無 suffix — 普通物品 USE_ITEM,從背包找同名物品
    Item,
    /// `M` 不指定 target — packet 不送 target 欄位,server 由 session 推斷
    NoSpec,
    /// `MME` 對自己 — 設 `[0x97C910]=[0xABF4B4]`(自己 char_id)後 spell_book_cast
    Self_,
    /// `MT` 對鼠標當下目標 — **3.8 未實作,退化成 NoSpec**。
    /// 為什麼:3.8 找不到「滑鼠 hover entity」全域可抓(`[0x97C910]` 全 process 沒人
    /// 寫,dispatcher 0x73C260 也不走)。替代方案:用 `/IT=<entity名>` 全自動,
    /// 或 `/ME` 對自身 buff。
    HoverTarget,
    /// `IT=<entity名>` 對「指定名字的玩家/召喚物/變身玩家」全自動施放
    ///
    /// 走 entity scan(vfptr `0x008DC08C`)+ name 比對(REMOTE entity `+0x6C`),
    /// 拿到 target_id(`+0x0C`)後送 `cdd 0xA4` packet。
    /// 對齊「右鍵卷軸 → 使用 → 點目標」但不需要玩家手動點。
    OnNamedEntity(String),
    /// `IME` 對自己施放卷軸 — `/IT=<self>` 的捷徑,target=自己 char_id
    ///
    /// **Why**:`/I` USE_ITEM 0x12 對需 target 的卷軸(治癒卷軸等)只進「目標選擇模式」,
    /// 不送施放 packet,server 看到沒完成的 cast 會回 `施咒失敗`。
    /// `IME` 直接走 `cdd 0xA4` II packet,target = `[0xABF4B4]`(自己 char_id)讀來,
    /// 對齊技能 `/ME` 的 self-cast 概念,適用所有需 target 但只想對自己用的卷軸。
    SelfItem,
    /// `MIA` / `IA` 對「(使用中)」物品施法/施放
    ///
    /// `Option<String>` = 名字過濾:
    /// - `None` → 找背包第一件含 `(使用中)` 的物品
    /// - `Some(name)` → 找名字 = `name` 且狀態 `(使用中)` 的物品
    OnInUseItem(Option<String>),
    /// `MIW` / `IW` 對「(揮舞)」物品施法/施放(同 [`Self::OnInUseItem`] 命名語意)
    OnWieldedItem(Option<String>),
    /// `MI` 對指定名稱物品施法 — name 來自 INI 第 2 段
    OnNamedItem(String),
    /// `ID` 銷毀/丟棄物品 — 送 C_DELETE_ITEM packet 把物品從背包移除
    DropItem,
    /// `INFO` debug — dump spr / mouseSpr / buff state
    Info,
    /// `KEY=F<n>` 模擬按 Fn 鍵(1~12)— 屬狀態頁
    Key(u8),
    /// `DKEY=F<n>` 同 Key 但加 delay — 屬狀態頁
    DelayKey(u8),
}

impl Default for CastTarget {
    fn default() -> Self {
        Self::Item
    }
}

/// 輔助分頁的條目(物品 / 技能 / 指令 / 按鍵)
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BuffItem {
    /// state_id (`[buff_array + id]` 0/1)— -1 = 未指定
    pub id: i32,
    /// 顯示用的乾淨名稱(suffix 已剝掉,`/M=xxx` 的 xxx 在 [`Self::cast_target`] 內)
    pub name: String,
    /// 概略類型:`'I'`=物品,`'S'`=技能,`'K'`=按鍵
    /// (跟 `cast_target` 一致;保留是為了不破壞舊 UI 邏輯)
    pub item_type: char,
    /// suffix 解析後的施法目標路徑
    pub cast_target: CastTarget,
}

impl Default for BuffItem {
    fn default() -> Self {
        Self {
            id: -1,
            name: String::new(),
            item_type: 'I',
            cast_target: CastTarget::Item,
        }
    }
}

/// 狀態分頁的 F1-F4 巨集
#[derive(Default, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FKeyMacro {
    pub enabled: bool,
    pub command: String,
}

/// 「其他」分頁 24 項 toggle
#[derive(Default, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MiscToggles {
    pub all_day: bool,
    pub underwater_pump: bool,
    pub low_cpu: bool,
    pub monster_level_color: bool,
    pub show_clock: bool,
    pub show_attack_dmg: bool,
    #[serde(default)]
    pub damage_at_feet: bool,
}

/// 定時分頁的單一 row
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TimerRow {
    pub enabled: bool,
    pub interval_sec: u32,
    pub command: String,
}

impl Default for TimerRow {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_sec: 5,
            command: String::new(),
        }
    }
}

/// 所有輔助功能的設定（對應 LinHelperZ 8 tabs）
#[derive(Default, Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AuxSettings {
    // ─── 通用 ───
    pub current_profile: String, // 階段 5 (D 模式) 才用,本階段保留 ""

    // ─── tab1: 喝水 ───
    pub potion_rows: [PotionRow; 7],
    pub mp_when_safe: MpWhenSafe,
    pub potion_use_percent: bool,
    pub potion_show_inventory: bool,

    // ─── tab2: 輔助 ───
    pub buff_enabled: bool,
    pub buff_items: Vec<BuffItem>,
    pub buff_inventory_items: Vec<BuffItem>,

    // ─── tab3: 狀態 ───
    pub status_show_exp: bool,
    pub status_whetstone: bool,
    pub status_eat_meat: bool,
    pub status_transform_enabled: bool,
    pub status_transform_item: String,
    pub status_transform_cond: String,
    pub status_antidote_enabled: bool,
    pub status_antidote_item: String,
    pub fkey_macros: [FKeyMacro; 4],

    // ─── tab4: 刪物 ───
    #[serde(default)]
    pub delete_enabled: bool,
    /// 直接刪除清單 — 走 C_DELETE_ITEM opcode
    #[serde(default)]
    pub delete_list: Vec<String>,
    /// 溶解清單 — 走 0xA4 II opcode,需要背包有「溶解劑」
    #[serde(default)]
    pub dissolve_list: Vec<String>,

    // ─── tab5: 喊話 ───
    pub shout_enabled: bool,
    pub shout_interval_sec: u32,
    pub shout_messages: Vec<String>,

    // ─── tab6: 其他（24 項 toggle） ───
    pub misc: MiscToggles,

    // ─── tab7: 定時 ───
    pub timer_master_enabled: bool,
    pub timer_rows: [TimerRow; 6],
}

/// 控制 handle — 由 main 持有，可發信讓所有 thread 退出
pub struct AuxControl {
    pub cancel: Arc<AtomicBool>,
    pub settings: Arc<RwLock<AuxSettings>>,
    /// 自動喝水 codecave handle(HOME 第一次按下時 install,跟 scheduler 共享)
    pub drink: Arc<RwLock<Option<Arc<crate::aux::drink_hook::DrinkHandle>>>>,
    /// Spell DB(進場後 lazy build,buff_tick 'S' 路徑用 name → packed_skill_id)
    pub spell_db: Arc<RwLock<Option<crate::aux::spell_db::SpellDb>>>,
    /// Spell Book(玩家已學技能,ForceSelfPacket 路徑用 — 拿玩家實際 level 的 packed)
    pub spell_book: Arc<RwLock<Option<crate::aux::spell_book::SpellBook>>>,
    /// EXP 追蹤狀態 — LinHelperZ status_show_exp toggle 共享
    pub exp_tracker: Arc<RwLock<crate::aux::exp_tracker::ExpTracker>>,
    /// 定時分頁 6 row 的重計 epoch counter — UI 點重計就 fetch_add(1),
    /// timer_tick 比對到變動就重設該 row 的 last_fire(重新計時)。
    pub timer_resets: Arc<[AtomicU64; 6]>,
}

impl AuxControl {
    /// 用初始 AuxSettings 建立(會新開 Arc — 注意:跟 LHX window 不同步!)
    /// 推薦改用 [`AuxControl::from_shared`] 共享 Arc。
    #[allow(dead_code)]
    pub fn new(initial: AuxSettings) -> Self {
        Self::from_shared(Arc::new(RwLock::new(initial)))
    }

    /// 跟 LHX window 共享同一個 settings Arc — UI 即時生效。
    pub fn from_shared(settings: Arc<RwLock<AuxSettings>>) -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            settings,
            drink: Arc::new(RwLock::new(None)),
            spell_db: Arc::new(RwLock::new(None)),
            spell_book: Arc::new(RwLock::new(None)),
            exp_tracker: Arc::new(RwLock::new(crate::aux::exp_tracker::ExpTracker::default())),
            timer_resets: Arc::new([
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ]),
        }
    }

    pub fn shutdown(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// 輔助功能總排程器
pub struct AuxScheduler {
    pub h_process: HANDLE,
    pub pid: u32,
    pub control: Arc<AuxControl>,
}

impl AuxScheduler {
    pub fn new(h_process: HANDLE, pid: u32, control: Arc<AuxControl>) -> Self {
        Self {
            h_process,
            pid,
            control,
        }
    }

    /// 啟動所有 polling thread。每個輔助功能對應一條 polling thread。
    /// 階段 1：僅佈線 thread 框架，內部邏輯為 stub。
    pub fn spawn_all(&self) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();
        // HANDLE = *mut c_void 不是 Send，跨 thread 必須先轉為 usize，thread 內再轉回
        let h_raw = self.h_process.0 as usize;
        let _ = self.pid;

        // state_poll (1Hz) — 玩家狀態(只在變化時 log,避免 spam 蓋掉 [inv]/[scan])
        // + 第一次成功讀到狀態時 dump 一次物品欄(驗證 INVENTORY_BASE)
        {
            let cancel = self.control.cancel.clone();
            let inv_dumped = Arc::new(AtomicBool::new(false));
            handles.push(std::thread::spawn(move || {
                let h = HANDLE(h_raw as *mut _);
                let mut last: Option<crate::aux::player_state::PlayerState> = None;
                tick_loop("state_poll", Duration::from_secs(1), cancel, move |_| {
                    match crate::aux::player_state::read_player_state(h) {
                        Ok(s) if s.max_hp > 0 => {
                            let changed = match &last {
                                None => true,
                                Some(p) => {
                                    p.hp != s.hp
                                        || p.max_hp != s.max_hp
                                        || p.mp != s.mp
                                        || p.max_mp != s.max_mp
                                        || p.food != s.food
                                        || p.weight != s.weight
                                        || p.map_id != s.map_id
                                }
                            };
                            if changed {
                                log_line!(
                                    "[state] HP={}/{} MP={}/{} food={}% weight={}% map={}",
                                    s.hp,
                                    s.max_hp,
                                    s.mp,
                                    s.max_mp,
                                    s.food,
                                    s.weight,
                                    s.map_id
                                );
                                last = Some(s);
                            }
                            // 第一次成功讀到狀態 → dump 物品欄一次
                            if !inv_dumped.swap(true, Ordering::Relaxed) {
                                dump_inventory(h);
                            }
                        }
                        Ok(_) => {} // 角色未進場(gameState != 3 或 max_hp=0),靜默
                        Err(e) => log_line!("[state] 讀取失敗: {e}"),
                    }
                });
            }));
        }

        // timer_drink (~500ms) — 補水 / 補魔 tick(獨立 thread)
        {
            let cancel = self.control.cancel.clone();
            let settings = self.control.settings.clone();
            handles.push(std::thread::spawn(move || {
                let h = HANDLE(h_raw as *mut _);
                let mut last_want: Option<bool> = None;
                let mut last_error: Option<String> = None;
                tick_loop(
                    "all_day_sync",
                    Duration::from_millis(500),
                    cancel,
                    move |_| {
                        all_day_sync_tick(h, &settings, &mut last_want, &mut last_error);
                    },
                );
            }));
        }

        {
            let cancel = self.control.cancel.clone();
            let settings = self.control.settings.clone();
            handles.push(std::thread::spawn(move || {
                let h = HANDLE(h_raw as *mut _);
                let mut last_want: Option<bool> = None;
                let mut last_error: Option<String> = None;
                tick_loop(
                    "underwater_pump_sync",
                    Duration::from_millis(500),
                    cancel,
                    move |_| {
                        underwater_pump_sync_tick(h, &settings, &mut last_want, &mut last_error);
                    },
                );
            }));
        }

        {
            let cancel = self.control.cancel.clone();
            let settings = self.control.settings.clone();
            let drink = self.control.drink.clone();
            let spell_book = self.control.spell_book.clone();
            handles.push(std::thread::spawn(move || {
                let h = HANDLE(h_raw as *mut _);
                tick_loop(
                    "timer_drink",
                    Duration::from_millis(500),
                    cancel,
                    move |_| {
                        drink_tick(h, &settings, &drink, &spell_book);
                    },
                );
            }));
        }

        // timer_buff (~500ms) — buff 自動補 tick(獨立 thread,自帶 cooldown HashMap)
        {
            let cancel = self.control.cancel.clone();
            let settings = self.control.settings.clone();
            let drink = self.control.drink.clone();
            let spell_db = self.control.spell_db.clone();
            let spell_book = self.control.spell_book.clone();
            handles.push(std::thread::spawn(move || {
                let h = HANDLE(h_raw as *mut _);
                // buff 觸發 cooldown(state_id → 上次觸發時間),per-thread 持有
                // 2 秒 cooldown:遊戲端 RTT 通常 < 1 秒,給 buggy 網路一點 buffer
                // key = (item_type, state_id) — item 跟 skill 共用同一 state_id 時必須分開
                // 計 cooldown(否則 item 先 fire 會以 ITEM cooldown 寫入,skill 緊接著看到
                // 還沒過 SKILL cooldown 就被擋下 180s)。
                let mut buff_cooldowns: std::collections::HashMap<(char, i32), std::time::Instant> =
                    std::collections::HashMap::new();
                tick_loop(
                    "timer_buff",
                    Duration::from_millis(500),
                    cancel,
                    move |_| {
                        buff_tick(
                            h,
                            &settings,
                            &drink,
                            &spell_db,
                            &spell_book,
                            &mut buff_cooldowns,
                        );
                    },
                );
            }));
        }

        // timer_status (~500ms) — 自動吃肉 / 解毒 tick(獨立 thread,自帶 cooldown HashMap)
        {
            let cancel = self.control.cancel.clone();
            let settings = self.control.settings.clone();
            let drink = self.control.drink.clone();
            let spell_book = self.control.spell_book.clone();
            handles.push(std::thread::spawn(move || {
                let h = HANDLE(h_raw as *mut _);
                let mut status_cooldowns: std::collections::HashMap<
                    &'static str,
                    std::time::Instant,
                > = std::collections::HashMap::new();
                tick_loop(
                    "timer_status",
                    Duration::from_millis(500),
                    cancel,
                    move |_| {
                        status_tick(h, &settings, &drink, &spell_book, &mut status_cooldowns);
                    },
                );
            }));
        }

        // timer_delete (~500ms) — 刪物 / 溶解 tick(獨立 thread)
        {
            let cancel = self.control.cancel.clone();
            let settings = self.control.settings.clone();
            let drink = self.control.drink.clone();
            handles.push(std::thread::spawn(move || {
                let h = HANDLE(h_raw as *mut _);
                tick_loop(
                    "timer_delete",
                    Duration::from_millis(500),
                    cancel,
                    move |_| {
                        delete_tick(h, &settings, &drink);
                    },
                );
            }));
        }

        // timer_timer (~500ms) — 定時分頁 tick(獨立 thread,跨 row 共享 last_fire / last_seen)
        {
            let cancel = self.control.cancel.clone();
            let settings = self.control.settings.clone();
            let drink = self.control.drink.clone();
            let spell_book = self.control.spell_book.clone();
            let spell_db = self.control.spell_db.clone();
            let resets = self.control.timer_resets.clone();
            handles.push(std::thread::spawn(move || {
                let h = HANDLE(h_raw as *mut _);
                let mut last_fire: [Option<std::time::Instant>; 6] = [None; 6];
                let mut last_seen: [u64; 6] = [0; 6];
                tick_loop(
                    "timer_timer",
                    Duration::from_millis(500),
                    cancel,
                    move |_| {
                        timer_tick(
                            h,
                            &settings,
                            &drink,
                            &spell_book,
                            &spell_db,
                            &resets,
                            &mut last_fire,
                            &mut last_seen,
                        );
                    },
                );
            }));
        }

        // timer_shout (~500ms) — 喊話分頁一般對話 tick(獨立 thread,輪播訊息)
        // 真正觸發節奏由使用者設定的 shout_interval_sec 控制(last_fire / next_idx 跨 tick 持有)
        {
            let cancel = self.control.cancel.clone();
            let settings = self.control.settings.clone();
            let drink = self.control.drink.clone();
            handles.push(std::thread::spawn(move || {
                let h = HANDLE(h_raw as *mut _);
                let mut last_fire: Option<std::time::Instant> = None;
                let mut next_idx: usize = 0;
                tick_loop(
                    "timer_shout",
                    Duration::from_millis(500),
                    cancel,
                    move |_| {
                        shout_tick(h, &settings, &drink, &mut last_fire, &mut next_idx);
                    },
                );
            }));
        }

        // timer_8 (100ms) — EXP 追蹤(LinHelperZ 顯示經驗值)
        {
            let cancel = self.control.cancel.clone();
            let exp_tracker = self.control.exp_tracker.clone();
            let settings = self.control.settings.clone();
            handles.push(std::thread::spawn(move || {
                let h = HANDLE(h_raw as *mut _);
                tick_loop("timer_8", Duration::from_millis(100), cancel, move |_| {
                    exp_tick(h, &settings, &exp_tracker);
                });
            }));
        }

        // F1-F4 全域 hotkey — 獨立模組,不參與 timer 系統
        {
            let h = HANDLE(h_raw as *mut _);
            let pid = self.pid;
            let settings = self.control.settings.clone();
            let drink = self.control.drink.clone();
            let spell_book = self.control.spell_book.clone();
            let spell_db = self.control.spell_db.clone();
            let cancel = self.control.cancel.clone();
            handles.extend(crate::aux::hotkey::install(
                h, pid, settings, drink, spell_book, spell_db, cancel,
            ));
        }

        log_line!(
            "[aux] AuxScheduler 啟動 {} 個 polling thread",
            handles.len()
        );
        handles
    }
}

/// 從清單跟 inventory name 列表挑出第一個要動作的(mode, name)。
///
/// **可單元測試的純函式** — `delete_tick` 走遊戲記憶體取資料,這個只負責 match logic。
///
/// 規則:
/// 1. 先掃 `delete_list`(直接刪)— 第一個在 inventory 找到的就回傳 ("delete", name)
/// 2. 都沒 match 才掃 `dissolve_list`(用溶解劑)— 同樣回第一個 match
/// 3. 名稱含 `(使用中)` / `(揮舞)` 一律忽略(雙保險:UI 加入時已擋,這裡再擋一次)
fn all_day_sync_tick(
    h: HANDLE,
    settings: &Arc<RwLock<AuxSettings>>,
    last_want: &mut Option<bool>,
    last_error: &mut Option<String>,
) {
    let want = settings.read().misc.all_day;
    let patch = crate::aux::toggle::all_day::AllDay;
    let result = if want {
        crate::aux::toggle::Toggle::enable(&patch, h)
    } else {
        crate::aux::toggle::Toggle::disable(&patch, h)
    };

    match result {
        Ok(()) => {
            if *last_want != Some(want) {
                log_line!("[all_day] sync enabled={want}");
            }
            *last_want = Some(want);
            *last_error = None;
        }
        Err(err) => {
            let msg = err.to_string();
            if last_error.as_deref() != Some(msg.as_str()) {
                log_line!("[all_day] sync failed: {msg}");
            }
            *last_error = Some(msg);
        }
    }
}

fn underwater_pump_sync_tick(
    h: HANDLE,
    settings: &Arc<RwLock<AuxSettings>>,
    last_want: &mut Option<bool>,
    last_error: &mut Option<String>,
) {
    let want = settings.read().misc.underwater_pump;
    let patch = crate::aux::toggle::underwater_pump::UnderwaterPump;
    let result = if want {
        crate::aux::toggle::Toggle::enable(&patch, h)
    } else {
        crate::aux::toggle::Toggle::disable(&patch, h)
    };

    match result {
        Ok(()) => {
            if *last_want != Some(want) {
                log_line!("[underwater_pump] sync enabled={want}");
            }
            *last_want = Some(want);
            *last_error = None;
        }
        Err(err) => {
            let msg = err.to_string();
            if last_error.as_deref() != Some(msg.as_str()) {
                log_line!("[underwater_pump] sync failed: {msg}");
            }
            *last_error = Some(msg);
        }
    }
}

fn pick_delete_action(
    delete_list: &[String],
    dissolve_list: &[String],
    inv_names: &[String],
) -> Option<(&'static str, String)> {
    let safe = |n: &str| -> bool { !n.contains("(使用中)") && !n.contains("(揮舞)") };
    for needle in delete_list {
        let needle = needle.as_str();
        if inv_names.iter().any(|n| n == needle && safe(n)) {
            return Some(("delete", needle.to_string()));
        }
    }
    for needle in dissolve_list {
        let needle = needle.as_str();
        if inv_names.iter().any(|n| n == needle && safe(n)) {
            return Some(("dissolve", needle.to_string()));
        }
    }
    None
}

/// timer_delete 刪物 tick — 獨立 thread 跑(不寄生 timer_2)。
///
/// 流程:
/// 1. guard:`delete_enabled` + 兩 list 不全空 + game_state==3 + DrinkHandle ready
/// 2. 讀 inventory → 用 [`pick_delete_action`] 拿 (mode, name)
/// 3. 找對應 item entry(如果是 dissolve 還要找溶解劑)
/// 4. fire packet:
///    - delete   → `dh.execute_delete(h, target.item_param, target.count)`
///    - dissolve → `dh.execute_use_on_wielded(h, 溶解劑.item_param, target.item_param)`
/// 5. 失敗 silent log,下個 tick 再試
fn delete_tick(
    h: HANDLE,
    settings: &Arc<RwLock<AuxSettings>>,
    drink: &Arc<RwLock<Option<Arc<crate::aux::drink_hook::DrinkHandle>>>>,
) {
    let s = settings.read().clone();
    if !s.delete_enabled || (s.delete_list.is_empty() && s.dissolve_list.is_empty()) {
        return;
    }

    let game_state = crate::memory::read_u32(h, crate::aux::address::G_GAME_STATE).unwrap_or(0);
    if game_state != 3 {
        return;
    }

    let dh = match drink.read().as_ref() {
        Some(h) => h.clone(),
        None => return,
    };

    let items = match crate::aux::inventory::list_items(h) {
        Ok(v) => v,
        Err(_) => return,
    };
    if items.is_empty() {
        return;
    }

    let inv_names: Vec<String> = items.iter().map(|it| it.name_lossy()).collect();
    let Some((mode, name)) = pick_delete_action(&s.delete_list, &s.dissolve_list, &inv_names)
    else {
        return;
    };

    let target = match items.iter().find(|it| it.name_lossy() == name) {
        Some(it) => it,
        None => return,
    };

    match mode {
        "delete" => {
            log_line!(
                "[delete] 刪除「{}」(param=0x{:08X}, count={})",
                target.name_lossy(),
                target.item_param,
                target.count
            );
            if let Err(e) = dh.execute_delete(h, target.item_param, target.count) {
                log_line!("[delete] execute_delete 失敗: {e:#}");
            }
        }
        "dissolve" => {
            let solvent = items
                .iter()
                .find(|it| it.name_lossy().starts_with("溶解劑"));
            let Some(sv) = solvent else {
                log_line!(
                    "[delete] 想溶解「{}」但背包沒「溶解劑」",
                    target.name_lossy()
                );
                return;
            };
            log_line!(
                "[delete] 溶解「{}」(溶解劑 param=0x{:08X} → 0x{:08X})",
                target.name_lossy(),
                sv.item_param,
                target.item_param
            );
            if let Err(e) = dh.execute_use_on_wielded(h, sv.item_param, target.item_param) {
                log_line!("[delete] 溶解 execute 失敗: {e:#}");
            }
        }
        _ => unreachable!(),
    }
}

/// 從喊話訊息清單中依 round-robin 順序挑下一則,回 (本次發送的 message, 下一個 idx)。
///
/// **可單元測試的純函式** — `shout_tick` 走遊戲記憶體 + 計時器,這個只做輪播 logic。
///
/// 規則:
/// 1. `messages` 為空 → `None`(caller 跳過 fire)
/// 2. 否則回 `Some((messages[next_idx % len].clone(), (next_idx + 1) % len))`
fn pick_shout_message(messages: &[String], next_idx: usize) -> Option<(String, usize)> {
    if messages.is_empty() {
        return None;
    }
    let len = messages.len();
    let i = next_idx % len;
    Some((messages[i].clone(), (i + 1) % len))
}

/// 從 6 個 row 中挑出第一個該觸發的 idx。
///
/// **可單元測試的純函式** — `timer_tick` 走遊戲記憶體,這個只做選擇 logic。
///
/// 規則:
/// 1. `master_enabled=false` → None
/// 2. 走訪 row 0..6,挑第一個滿足:
///    - `enabled` && `command 非空`
///    - `last_fire = None`(從沒觸發過,視為 due)
///    - 或 `last_fire.elapsed() >= interval_sec`
fn pick_timer_action(
    rows: &[TimerRow; 6],
    last_fire: &[Option<std::time::Instant>; 6],
    master_enabled: bool,
    now: std::time::Instant,
) -> Option<usize> {
    if !master_enabled {
        return None;
    }
    for i in 0..6 {
        let row = &rows[i];
        if !row.enabled || row.command.is_empty() {
            continue;
        }
        let due = match last_fire[i] {
            None => true,
            Some(t) => {
                now.duration_since(t) >= std::time::Duration::from_secs(row.interval_sec as u64)
            }
        };
        if due {
            return Some(i);
        }
    }
    None
}

/// timer_timer 定時 tick — 獨立 thread 跑(不寄生 timer_2)。
///
/// 流程:
/// 1. guards:`timer_master_enabled` + `game_state==3` + DrinkHandle ready
/// 2. 處理 reset epoch(全 6 row,獨立於選擇邏輯)— 偵測 LhxWindow 重計按鈕 bump
///    → 重設該 row 的 `last_fire`,等同重新從現在開始計時
/// 3. `pick_timer_action` 挑該觸發的 idx(每 tick 最多一個 row)
/// 4. `parse_buff_item(row.command)` → BuffItem
/// 5. `buff_dispatch::execute_buff_item` 派 dispatch
/// 6. 設 `last_fire[idx] = Some(now)`
fn timer_tick(
    h: HANDLE,
    settings: &Arc<RwLock<AuxSettings>>,
    drink: &Arc<RwLock<Option<Arc<crate::aux::drink_hook::DrinkHandle>>>>,
    spell_book: &Arc<RwLock<Option<crate::aux::spell_book::SpellBook>>>,
    spell_db: &Arc<RwLock<Option<crate::aux::spell_db::SpellDb>>>,
    resets: &Arc<[std::sync::atomic::AtomicU64; 6]>,
    last_fire: &mut [Option<std::time::Instant>; 6],
    last_seen: &mut [u64; 6],
) {
    use std::sync::atomic::Ordering::Relaxed;

    let s = settings.read().clone();
    if !s.timer_master_enabled {
        return;
    }
    let game_state = crate::memory::read_u32(h, crate::aux::address::G_GAME_STATE).unwrap_or(0);
    if game_state != 3 {
        return;
    }
    let dh = match drink.read().as_ref() {
        Some(h) => h.clone(),
        None => return,
    };

    let now = std::time::Instant::now();

    // 處理 reset epoch — UI 點重計 → bump → 重設 last_fire
    for i in 0..6 {
        let cur = resets[i].load(Relaxed);
        if cur != last_seen[i] {
            last_fire[i] = Some(now);
            last_seen[i] = cur;
            log_line!("[timer] row {} 收到重計信號,從現在開始計時", i);
        }
    }

    let Some(idx) = pick_timer_action(&s.timer_rows, last_fire, s.timer_master_enabled, now) else {
        return;
    };
    let bi = crate::aux::lhx_window::parse_buff_item(&s.timer_rows[idx].command);
    let ctx = crate::aux::buff_dispatch::DispatchCtx {
        h,
        dh: &dh,
        spell_book,
        spell_db,
    };
    log_line!(
        "[timer] row {} 觸發 → 指令「{}」(間隔 {}s)",
        idx,
        s.timer_rows[idx].command,
        s.timer_rows[idx].interval_sec
    );
    let _ = crate::aux::buff_dispatch::execute_buff_item(&ctx, &bi);
    last_fire[idx] = Some(now);
}

/// timer_shout 喊話 tick — 獨立 thread 跑(不寄生 timer_2)。
///
/// 流程:
/// 1. guard:`shout_enabled` + `shout_messages` 不為空 + `shout_interval_sec > 0`
///          + game_state==3 + DrinkHandle ready
/// 2. 距離上次發送未滿 `shout_interval_sec` → 跳過
/// 3. 用 [`pick_shout_message`] 拿下一則訊息(round-robin)
/// 4. fire packet:`dh.execute_chat(h, CHAT_CHANNEL_NORMAL, &msg)`
///    依使用者澄清,channel 為 0x00 一般對話(不是 0x02 喊話)
/// 5. 失敗 silent log,下個 tick 再試
fn shout_tick(
    h: HANDLE,
    settings: &Arc<RwLock<AuxSettings>>,
    drink: &Arc<RwLock<Option<Arc<crate::aux::drink_hook::DrinkHandle>>>>,
    last_fire: &mut Option<std::time::Instant>,
    next_idx: &mut usize,
) {
    let s = settings.read().clone();
    if !s.shout_enabled || s.shout_messages.is_empty() || s.shout_interval_sec == 0 {
        return;
    }

    let game_state = crate::memory::read_u32(h, crate::aux::address::G_GAME_STATE).unwrap_or(0);
    if game_state != 3 {
        return;
    }

    let dh = match drink.read().as_ref() {
        Some(h) => h.clone(),
        None => return,
    };

    // 還沒到 interval → 跳過(last_fire=None 第一次直接 fire)
    let interval = Duration::from_secs(s.shout_interval_sec as u64);
    if let Some(t) = last_fire {
        if t.elapsed() < interval {
            return;
        }
    }

    let Some((msg, new_idx)) = pick_shout_message(&s.shout_messages, *next_idx) else {
        return;
    };

    log_line!(
        "[shout] 發送一般對話「{}」(interval={}s, idx={}/{})",
        msg,
        s.shout_interval_sec,
        *next_idx % s.shout_messages.len(),
        s.shout_messages.len()
    );
    if let Err(e) = dh.execute_chat(h, crate::aux::address::CHAT_CHANNEL_NORMAL, &msg) {
        log_line!("[shout] execute_chat 失敗: {e:#}");
    }

    *next_idx = new_idx;
    *last_fire = Some(std::time::Instant::now());
}

fn mp_when_safe_triggered(s: &AuxSettings, state: &crate::aux::player_state::PlayerState) -> bool {
    let rule = &s.mp_when_safe;
    if !rule.enabled || rule.item.trim().is_empty() {
        return false;
    }

    if s.potion_use_percent {
        let hp_pct = state.hp.saturating_mul(100) / state.max_hp.max(1);
        let mp_pct = state.mp.saturating_mul(100) / state.max_mp.max(1);
        hp_pct >= rule.hp_lower && mp_pct <= rule.mp_upper
    } else {
        state.hp >= rule.hp_lower && state.mp <= rule.mp_upper
    }
}

/// thin wrapper — 委派給 [`crate::aux::spell_book::ensure_fresh`],統一換角偵測。
///
/// 保留名字是為了相容 `drink_tick` / `mp-safe` 既有 caller 不用全部改名。
fn ensure_spell_book_ready(
    h: HANDLE,
    spell_book: &Arc<RwLock<Option<crate::aux::spell_book::SpellBook>>>,
    tag: &str,
) -> bool {
    crate::aux::spell_book::ensure_fresh(h, spell_book, tag)
}

/// timer_2 喝水 tick — 檢查 7 個 row,符合條件 + 對應藥水在背包就 queue。
///
/// 目前只實作 row[0](HP threshold);其他 row + MP/safe-MP 後續補。
fn drink_tick(
    h: HANDLE,
    settings: &Arc<RwLock<crate::aux::runtime::AuxSettings>>,
    drink: &Arc<RwLock<Option<Arc<crate::aux::drink_hook::DrinkHandle>>>>,
    spell_book: &Arc<RwLock<Option<crate::aux::spell_book::SpellBook>>>,
) {
    let s = settings.read().clone();
    let has_potion_rows = s
        .potion_rows
        .iter()
        .any(|r| r.enabled && !r.item.trim().is_empty());
    let has_mp_when_safe = s.mp_when_safe.enabled && !s.mp_when_safe.item.trim().is_empty();
    if !has_potion_rows && !has_mp_when_safe {
        return; // 沒人勾喝水
    }

    // 必須在遊戲世界內(G_GAME_STATE == 3)才允許喝水。
    // 退選角 / 切換伺服器 / 角色未進場時,inventory 指標和 USE_ITEM 內部 context
    // 可能尚未就緒或已被釋放,呼叫 USE_ITEM 會 crash。
    let game_state = crate::memory::read_u32(h, crate::aux::address::G_GAME_STATE).unwrap_or(0);
    if game_state != 3 {
        return; // silent skip — 退選角 / 載入中 / 連線斷
    }

    // hook 還沒裝就不能 queue
    let dh = match drink.read().as_ref() {
        Some(h) => h.clone(),
        None => return,
    };

    // 讀玩家狀態
    let state = match crate::aux::player_state::read_player_state(h) {
        Ok(s) if s.max_hp > 0 => s,
        _ => return, // max_hp=0,可能剛進場狀態還沒填,跳過
    };
    let hp_pct = state.hp.saturating_mul(100) / state.max_hp.max(1);

    // 走訪 row(由上而下,優先順序高的先試)
    for row in s.potion_rows.iter() {
        if !row.enabled || row.item.is_empty() {
            continue;
        }
        // 觸發判斷:勾「使用百分比」用 hp%,否則用 raw HP
        let trigger = if s.potion_use_percent {
            hp_pct < row.threshold
        } else {
            state.hp < row.threshold
        };
        if !trigger {
            continue; // HP 還夠高,輪不到這 row
        }

        // 解析 row 字串(剝掉 /M /ME 等 suffix,或留為一般物品)
        let bi = crate::aux::lhx_window::parse_buff_item(&row.item);

        match bi.item_type {
            // 物品:走原本 USE_ITEM 路徑
            'I' => {
                let items = match crate::aux::inventory::list_items(h) {
                    Ok(v) => v,
                    Err(e) => {
                        log_line!("[drink] 讀背包失敗(可能剛進場 inventory 還沒 ready): {e:#}");
                        return;
                    }
                };
                if items.is_empty() {
                    log_line!(
                        "[drink] HP 觸發但背包是空的(item 數=0,inventory pointer 可能失效或 server 還沒下發)。row={:?}",
                        row.item
                    );
                    return;
                }
                let needle = crate::aux::lhx_window::strip_qty(&bi.name);
                let it = match items
                    .iter()
                    .find(|it| crate::aux::lhx_window::strip_qty(&it.name_lossy()) == needle)
                {
                    Some(it) => it,
                    None => {
                        log_line!(
                            "[drink] HP 觸發但背包找不到目標物品 needle={:?}(背包 {} 件)",
                            needle,
                            items.len()
                        );
                        return;
                    }
                };
                if s.potion_use_percent {
                    log_line!(
                        "[drink] execute entry=0x{:08X} param=0x{:08X} (HP={}/{} {}% < {}%, item={:?})",
                        it.entry_addr, it.item_param, state.hp, state.max_hp, hp_pct, row.threshold,
                        it.name_lossy()
                    );
                } else {
                    log_line!(
                        "[drink] execute entry=0x{:08X} param=0x{:08X} (HP={}/{} < {}, item={:?})",
                        it.entry_addr,
                        it.item_param,
                        state.hp,
                        state.max_hp,
                        row.threshold,
                        it.name_lossy()
                    );
                }
                let t0 = std::time::Instant::now();
                match dh.execute_drink(h, it.entry_addr) {
                    Ok(()) => log_line!("[drink] execute OK,耗時 {} ms", t0.elapsed().as_millis()),
                    Err(e) => log_line!("[drink] execute 失敗: {e:#}"),
                }
            }
            // 技能:走 spell_book + execute_skill — 對齊 fire_status_action 的 'S' 分支
            'S' => {
                if !ensure_spell_book_ready(h, spell_book, "drink") {
                    return;
                }
                let packed = match spell_book.read().as_ref().and_then(|b| b.lookup(&bi.name)) {
                    Some(p) => p,
                    None => {
                        log_line!(
                            "[drink] HP 觸發但技能「{}」未學會(spell_book 沒這個),skip",
                            bi.name
                        );
                        return;
                    }
                };
                let mode = match &bi.cast_target {
                    crate::aux::runtime::CastTarget::Self_ => {
                        crate::aux::drink_hook::SkillTargetMode::ForceSelfPacket
                    }
                    crate::aux::runtime::CastTarget::NoSpec => {
                        crate::aux::drink_hook::SkillTargetMode::NoSpec
                    }
                    other => {
                        log_line!(
                            "[drink] 技能「{}」cast_target={:?} 喝水流程不支援(只接 /M /ME)",
                            bi.name,
                            other
                        );
                        return;
                    }
                };
                if s.potion_use_percent {
                    log_line!(
                        "[drink] cast skill packed={} (HP={}/{} {}% < {}%, name={:?})",
                        packed,
                        state.hp,
                        state.max_hp,
                        hp_pct,
                        row.threshold,
                        bi.name
                    );
                } else {
                    log_line!(
                        "[drink] cast skill packed={} (HP={}/{} < {}, name={:?})",
                        packed,
                        state.hp,
                        state.max_hp,
                        row.threshold,
                        bi.name
                    );
                }
                if let Err(e) = dh.execute_skill(h, packed, mode) {
                    log_line!("[drink] execute_skill 失敗: {e:#}");
                }
            }
            other => {
                log_line!(
                    "[drink] row {:?} item_type={:?} 不支援(只接物品 / /M / /ME)",
                    row.item,
                    other
                );
                return;
            }
        }
        // 一個 tick 只 queue 一個 row,後面 row 等下次再說
        return;
    }

    if !mp_when_safe_triggered(&s, &state) {
        return;
    }

    let bi = crate::aux::lhx_window::parse_buff_item(&s.mp_when_safe.item);
    match bi.item_type {
        'I' => {
            let items = match crate::aux::inventory::list_items(h) {
                Ok(v) => v,
                Err(e) => {
                    log_line!("[drink/mp-safe] inventory read failed: {e:#}");
                    return;
                }
            };
            let needle = crate::aux::lhx_window::strip_qty(&bi.name);
            let it = match items
                .iter()
                .find(|it| crate::aux::lhx_window::strip_qty(&it.name_lossy()) == needle)
            {
                Some(it) => it,
                None => {
                    log_line!(
                        "[drink/mp-safe] item not found needle={:?}, inventory={}",
                        needle,
                        items.len()
                    );
                    return;
                }
            };
            log_line!(
                "[drink/mp-safe] execute item entry=0x{:08X} param=0x{:08X} (HP={}/{}, MP={}/{}, item={:?})",
                it.entry_addr,
                it.item_param,
                state.hp,
                state.max_hp,
                state.mp,
                state.max_mp,
                it.name_lossy()
            );
            if let Err(e) = dh.execute_drink(h, it.entry_addr) {
                log_line!("[drink/mp-safe] execute item failed: {e:#}");
            }
        }
        'S' => {
            if !ensure_spell_book_ready(h, spell_book, "drink/mp-safe") {
                return;
            }
            let packed = match spell_book.read().as_ref().and_then(|b| b.lookup(&bi.name)) {
                Some(p) => p,
                None => {
                    log_line!(
                        "[drink/mp-safe] spell not found in spell_book: {:?}",
                        bi.name
                    );
                    return;
                }
            };
            let mode = match &bi.cast_target {
                crate::aux::runtime::CastTarget::Self_ => {
                    crate::aux::drink_hook::SkillTargetMode::ForceSelfPacket
                }
                crate::aux::runtime::CastTarget::NoSpec => {
                    crate::aux::drink_hook::SkillTargetMode::NoSpec
                }
                other => {
                    log_line!(
                        "[drink/mp-safe] unsupported cast_target for {:?}: {:?}",
                        bi.name,
                        other
                    );
                    return;
                }
            };
            log_line!(
                "[drink/mp-safe] cast skill packed={} (HP={}/{}, MP={}/{}, name={:?})",
                packed,
                state.hp,
                state.max_hp,
                state.mp,
                state.max_mp,
                bi.name
            );
            if let Err(e) = dh.execute_skill(h, packed, mode) {
                log_line!("[drink/mp-safe] execute_skill failed: {e:#}");
            }
        }
        other => {
            log_line!(
                "[drink/mp-safe] item {:?} item_type={:?} unsupported",
                s.mp_when_safe.item,
                other
            );
        }
    }
}

/// buff 自動補 tick — 偵測 buff 表 byte == 0 就觸發補
///
/// 邏輯:
/// 1. 必須勾「啟用」 + 在遊戲世界內 + DrinkHandle ready
/// 2. 走訪 user 勾選的 `s.buff_items`(每個有 state_id + name + item_type)
/// 3. 讀 `[BUFF_STATE_ARRAY + state_id]` 1 byte(server 套 buff 時會寫此 byte)
/// 4. byte == 0(buff 不在身上)→ 觸發補:
///    - `item_type='I'`(物品)→ 找背包 → execute_drink
///    - `item_type='S'`(技能)→ 走 spell_book_cast 或 ForceSelfPacket
/// 5. 每個 buff per-state-id cooldown 2 秒,避免 RTT 期間重複觸發
fn buff_tick(
    h: HANDLE,
    settings: &Arc<RwLock<crate::aux::runtime::AuxSettings>>,
    drink: &Arc<RwLock<Option<Arc<crate::aux::drink_hook::DrinkHandle>>>>,
    spell_db: &Arc<RwLock<Option<crate::aux::spell_db::SpellDb>>>,
    spell_book: &Arc<RwLock<Option<crate::aux::spell_book::SpellBook>>>,
    cooldowns: &mut std::collections::HashMap<(char, i32), std::time::Instant>,
) {
    let s = settings.read().clone();
    if !s.buff_enabled || s.buff_items.is_empty() {
        return;
    }

    // 必須在遊戲世界內(對齊 drink_tick 的 guard)
    let game_state = crate::memory::read_u32(h, crate::aux::address::G_GAME_STATE).unwrap_or(0);
    if game_state != 3 {
        return;
    }

    let dh = match drink.read().as_ref() {
        Some(h) => h.clone(),
        None => return,
    };

    // Spell DB lazy build — 進場後第一次 buff_tick 觸發,後面所有 tick 共用
    if spell_db.read().is_none() {
        match crate::aux::spell_db::SpellDb::build(h) {
            Ok(db) => {
                log_line!(
                    "[buff] spell DB 載入完成 — 共 {} 個 entries / {} 個唯一名稱",
                    db.entries(),
                    db.unique_names()
                );
                *spell_db.write() = Some(db);
            }
            Err(e) => {
                // 失敗只 log 一次,buff_tick 繼續跑('I' 路徑不受影響)
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    log_line!("[buff] spell DB 建表失敗 (技能類 buff 暫不可用): {e:#}");
                }
            }
        }
    }

    // Spell Book lazy build / 換角 invalidate — 玩家已學技能,ForceSelfPacket 路徑要拿
    // 玩家實際 level 的 packed。ensure_fresh 會比對 [SPELL_BOOK_PTR] fingerprint,
    // 換角後遊戲重新分配 spell_book object 即被偵測為 stale 並重建。
    let _ = crate::aux::spell_book::ensure_fresh(h, &spell_book, "buff");

    // 先讀 buff state byte array — 整段 0x1F0 = 496 bytes(state_id 0..495)
    // INI 寫的 id 是「效果類別 id」,不是「技能本身 id」 — 遊戲設計:
    //   id=0  : 加速類效果(自我加速藥水 / 加速術 / 強力加速術 / 綠色藥水...)共用
    //   id=2  : 壯膽類(勇敢藥水 / 精靈餅乾...)共用
    //   id=10 : 通暢氣脈(1:1 對應)
    // byte[id]=1 = 你身上有「該類別的任何 buff」,launcher 不再補同類。
    // 3.8 對少數 buff 做 per-class 編號(法師「行走加速」=42 而非 INI 寫的 37 等),
    // 透過 [`class_remap`] 修正;絕大多數 buff 是 class-agnostic 直接套 INI id。
    const BUFF_TABLE_SIZE: usize = 0x1F0;
    let buff_table = match crate::memory::read_bytes(
        h,
        crate::aux::address::G_HASTE_BUFF_TABLE,
        BUFF_TABLE_SIZE,
    ) {
        Ok(b) => b,
        Err(_) => return, // 讀失敗,下次再試
    };

    // Per-class state_id remap — 3.8 對 ~10 個 buff 在 server 送同一 packet 時依職業套不同
    // state_id(例:法師「行走加速」走 byte[42] 而非 INI 寫的 byte[37])。讀職業一次套整 tick。
    let class = crate::aux::class_remap::read_class(h);

    let now = std::time::Instant::now();

    // Cooldown 統一 5 秒 — 物品/技能都靠 INI id 查 state byte,cooldown 只防 RTT 重複
    const COOLDOWN_ITEM: std::time::Duration = std::time::Duration::from_secs(5);
    const COOLDOWN_SKILL: std::time::Duration = std::time::Duration::from_secs(5);

    // 一個 tick 內:
    //   - 物品(I)可連續使用多個(喝水/吃肉等是即時動作,不互相阻擋)
    //   - 技能(S)最多只 cast 一個(遊戲一次只能施放一個技能,連發後面的全部
    //     被 server 用 ERROR 拒絕 → byte 永遠不設 → 永遠循環)
    // 已 cast 一個 skill 之後,後面遇到的 skill 全部跳過,等下個 tick(500ms 後)再放。
    // dispatch 細節(Item/DropItem/Info、spell_book lookup、cast_target → SkillTargetMode、
    // ForceSelfPacket 名單)搬到 [`buff_dispatch`] 與 [`timer_tick`] 共用。
    let mut skill_cast_this_tick = false;
    for buff in s.buff_items.iter() {
        if buff.id < 0 || buff.id as usize >= buff_table.len() {
            continue;
        }
        let cooldown = match buff.item_type {
            'S' => COOLDOWN_SKILL,
            _ => COOLDOWN_ITEM,
        };
        let cd_key = (buff.item_type, buff.id);
        if let Some(last) = cooldowns.get(&cd_key) {
            if now.duration_since(*last) < cooldown {
                continue;
            }
        }

        // skill 一個 tick 只放一個
        if buff.item_type == 'S' && skill_cast_this_tick {
            continue;
        }

        // state byte 檢查 — 用 INI buff.id 套職業 remap
        let state_id = crate::aux::class_remap::remap(class, buff.id);
        let byte_a = buff_table.get(state_id as usize).copied().unwrap_or(0);
        let byte_b = crate::memory::read_bytes(h, 0x00ABF6B8 + state_id as u32, 1)
            .ok()
            .and_then(|v| v.first().copied())
            .unwrap_or(0);
        if byte_a != 0 || byte_b != 0 {
            continue; // buff 已生效(同類已有),不補
        }

        // 通過 gates → 派 dispatch
        let ctx = crate::aux::buff_dispatch::DispatchCtx {
            h,
            dh: &dh,
            spell_book,
            spell_db,
        };
        match crate::aux::buff_dispatch::execute_buff_item(&ctx, buff) {
            crate::aux::buff_dispatch::DispatchOutcome::Done => {
                cooldowns.insert(cd_key, now);
            }
            crate::aux::buff_dispatch::DispatchOutcome::SkillCast => {
                cooldowns.insert(cd_key, now);
                skill_cast_this_tick = true;
            }
            crate::aux::buff_dispatch::DispatchOutcome::Skipped(reason) => {
                // 對齊既有行為:dispatch 失敗也 cooldown 一下,避免下個 tick 立刻 spam log
                // (原 buff_tick 在「背包找不到」/「spell lookup 失敗」都 cooldowns.insert)
                cooldowns.insert(cd_key, now);
                let _ = reason; // log 已在 dispatch 內印過
            }
        }
    }
}

/// 狀態頁 tick — 自動吃肉 + 解毒 + 磨刀石 + 變身
///
/// 為什麼分區 tick 而非每功能一個 thread:這四個動作共享 game_state guard +
/// DrinkHandle,合在一條 polling thread 跑省 thread context switch。
/// `cooldowns` 用 `&'static str` key(每個 feature 一條 cooldown,5 秒節流)。
fn status_tick(
    h: HANDLE,
    settings: &Arc<RwLock<crate::aux::runtime::AuxSettings>>,
    drink: &Arc<RwLock<Option<Arc<crate::aux::drink_hook::DrinkHandle>>>>,
    spell_book: &Arc<RwLock<Option<crate::aux::spell_book::SpellBook>>>,
    cooldowns: &mut std::collections::HashMap<&'static str, std::time::Instant>,
) {
    let s = settings.read().clone();
    if !s.status_eat_meat
        && !s.status_antidote_enabled
        && !s.status_whetstone
        && !s.status_transform_enabled
    {
        return; // 四個功能全關
    }

    // game_state guard(同 buff_tick / drink_tick)
    let game_state = crate::memory::read_u32(h, crate::aux::address::G_GAME_STATE).unwrap_or(0);
    if game_state != 3 {
        return;
    }

    let dh = match drink.read().as_ref() {
        Some(h) => h.clone(),
        None => return,
    };

    // SpellBook lazy build — buff_tick 在 buff 未啟用時 early-return 不會建表,
    // 但 status_tick(解毒術等技能類動作)需要靠它查 packed。在這裡也建一次,
    // 確保即使 user 沒勾任何 buff 也能跑技能解毒。換角後 ensure_fresh 偵測 stale 並重建。
    if s.status_antidote_enabled {
        let _ = crate::aux::spell_book::ensure_fresh(h, &spell_book, "status");
    }

    let now = std::time::Instant::now();
    // 解毒 / 卡毒走標準 5s(server RTT + 動畫時間)
    const COOLDOWN_POISON: std::time::Duration = std::time::Duration::from_secs(5);
    // 吃肉走 1s(USE_ITEM 即時動作不阻擋,要快點補滿)
    const COOLDOWN_EAT: std::time::Duration = std::time::Duration::from_secs(1);

    // 1. 自動吃肉 — 飽食度沒滿就吃(raw < FOOD_MAX 即觸發,1s cooldown 防 packet 連發)
    //
    // raw 是 0..225 的 byte,225 = 100%。沒設可調門檻 — 開了就要吃滿。
    if s.status_eat_meat {
        let due = cooldowns
            .get("eat_meat")
            .map(|t| now.duration_since(*t) >= COOLDOWN_EAT)
            .unwrap_or(true);
        if due {
            if let Some(addr) = crate::aux::address::FOOD_LEVEL {
                if let Ok(b) = crate::memory::read_bytes(h, addr, 1) {
                    let raw = b.first().copied().unwrap_or(0xFF) as u32;
                    let max = crate::aux::address::FOOD_LEVEL_DIVISOR;
                    if raw < max {
                        cooldowns.insert("eat_meat", now);
                        if let Ok(items) = crate::aux::inventory::list_items(h) {
                            let found = items.iter().find(|it| {
                                crate::aux::lhx_window::strip_qty(&it.name_lossy()) == "肉"
                            });
                            if let Some(it) = found {
                                let pct = raw * 100 / max;
                                log_line!(
                                    "[status] 自動吃肉:飽食度 {}%({}/{})→ 用 entry=0x{:08X}",
                                    pct,
                                    raw,
                                    max,
                                    it.entry_addr
                                );
                                if let Err(e) = dh.execute_use_item(h, it.entry_addr) {
                                    log_line!("[status] 吃肉 execute 失敗: {e:#}");
                                }
                            } else {
                                log_line!("[status] 飽食度未滿但背包沒「肉」");
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. 解毒 / 卡毒 — 偵測來源:poison_hook 讀 `player+0x20` bit 5
    //
    // 為什麼不用 byte_table:3.8 PoisonHandler 對毒呼叫 apply_buff(state_id, type=0),
    // type=0 路徑不寫 byte_table 只設 timer,所以 byte 永遠是 0,中毒偵測必須換來源。
    // poison_hook 改成直接讀 player struct 的 status bit(2026-05-02 真實怪物毒驗證),
    // 無 hook、無 patch,反偵測零風險。
    if s.status_antidote_enabled
        && !s.status_antidote_item.is_empty()
        && crate::aux::poison_hook::is_damage_poisoned(h)
    {
        let due = cooldowns
            .get("antidote")
            .map(|t| now.duration_since(*t) >= COOLDOWN_POISON)
            .unwrap_or(true);
        if due {
            cooldowns.insert("antidote", now);
            fire_status_action(h, &s.status_antidote_item, &dh, spell_book, "antidote");
        }
    }

    // 3. 自動磨刀石 — 偵測揮舞中武器的 description 含「損壞度」就磨
    //
    // 機制(2026-05-02 spy log + caller RE 驗證):
    //   - 揮舞中武器 entry: list_items() 找 name 含「(揮舞)」的 item
    //   - description string: item_entry+0xA8(3.8 偏移)
    //   - 觸發條件:description 含「損壞度」(Big5 B7 6C C3 61 AB D7)
    //   - 動作:**直接送 II packet**(opcode 0xA4, source=磨刀石.item_param,
    //     target=揮舞武器.item_param)。
    //     為什麼不 call 遊戲 0x00410570 wrapper:該函數從 RemoteThread 進入時 ECX
    //     結構未初始化,server 收到不完整 packet 會踢線。
    //
    // 1 秒 cooldown — tick 是 500ms,實際上每 1~1.5 秒磨一次。
    if s.status_whetstone {
        const COOLDOWN_WHETSTONE: std::time::Duration = std::time::Duration::from_secs(1);
        let due = cooldowns
            .get("whetstone")
            .map(|t| now.duration_since(*t) >= COOLDOWN_WHETSTONE)
            .unwrap_or(true);
        if due {
            if let Err(e) = whetstone_tick(h, &dh) {
                // log_line! 一次就好,避免每秒噴 — 用 cooldown 強制節流
                cooldowns.insert("whetstone", now);
                log_line!("[status][whetstone] {e:#}");
            } else {
                cooldowns.insert("whetstone", now);
            }
        }
    }

    // 4. 自動變身(普通變身藥水模式)
    //
    // 目前僅實作模式 1(普通 USE_ITEM):選單選一個變身物品(e.g. 狼人變身藥水),
    // 偵測到「沒在變身」就 USE_ITEM。
    //
    // 模式 2(變形卷軸_選項_IP):需要 RE 3.8 的 IP packet opcode + packet 結構,
    // 暫不實作 — `status_transform_cond` 欄位先保留 UI 但不影響行為。
    //
    // state byte:`buff_table[39]` 為變身 flag,進場後有變身 spr_id 時 server 會把
    // 該 byte 設為 1;偵測 byte == 0 才觸發 USE_ITEM。
    //
    // 5 秒 cooldown — 對齊 server 變身動畫 + RTT;每次只送一顆。
    if s.status_transform_enabled && !s.status_transform_item.is_empty() {
        const COOLDOWN_TRANSFORM: std::time::Duration = std::time::Duration::from_secs(5);
        let due = cooldowns
            .get("transform")
            .map(|t| now.duration_since(*t) >= COOLDOWN_TRANSFORM)
            .unwrap_or(true);
        if due {
            cooldowns.insert("transform", now);
            if let Err(e) =
                transform_tick(h, &s.status_transform_item, &s.status_transform_cond, &dh)
            {
                log_line!("[status][transform] {e:#}");
            }
        }
    }
}

/// 自動變身單次 tick — 兩種模式自動分流:
///
/// - **模式 1**(普通變身藥水):`option_string` 是空 → USE_ITEM(走 use_item_addr 函數)
/// - **模式 2**(變形卷軸 IP):`option_string` 非空(像 "death 80")→ 自組 II packet
///   `SendPacketData("cds", 0xA4, scroll.item_param, option_ptr)`(對齊 spy #139)
///
/// 共用觸發條件:`buff_table[39] == 0` 表示沒在變身。
fn transform_tick(
    h: HANDLE,
    item_str: &str,
    option_string: &str,
    dh: &Arc<crate::aux::drink_hook::DrinkHandle>,
) -> anyhow::Result<()> {
    // 解析後綴(剝掉 _xxx_I 之類)
    let bi = crate::aux::lhx_window::parse_buff_item(item_str);
    if bi.item_type != 'I' {
        // 技能路徑(_M / _MME 等)變身物品系統不接 — silent skip
        return Ok(());
    }

    // 讀 buff_table[39] — 變身狀態 flag(進場後變身時 server 寫 1)
    const TRANSFORM_STATE_ID: u32 = 39;
    let byte_a = crate::memory::read_bytes(
        h,
        crate::aux::address::G_HASTE_BUFF_TABLE + TRANSFORM_STATE_ID,
        1,
    )?;
    let byte_b = crate::memory::read_bytes(h, 0x00ABF6B8 + TRANSFORM_STATE_ID, 1)?;
    if byte_a.first().copied().unwrap_or(0) != 0 || byte_b.first().copied().unwrap_or(0) != 0 {
        return Ok(()); // 已變身,不重觸發
    }

    // 找背包同名物品
    let items = crate::aux::inventory::list_items(h)?;
    let needle = crate::aux::lhx_window::strip_qty(&bi.name);
    let it = items
        .iter()
        .find(|it| crate::aux::lhx_window::strip_qty(&it.name_lossy()) == needle);
    let Some(it) = it else {
        anyhow::bail!("變身物品「{}」不在背包(背包 {} 件)", bi.name, items.len());
    };

    let cond_raw = option_string.trim();
    if cond_raw.is_empty() {
        // 模式 1:普通變身藥水 — USE_ITEM
        log_line!(
            "[status][transform] state[{}]=0 → 用「{}」(entry=0x{:08X})",
            TRANSFORM_STATE_ID,
            bi.name,
            it.entry_addr
        );
        dh.execute_use_item(h, it.entry_addr)?;
    } else {
        // 模式 2:變形卷軸 IP packet — 帶 option string
        // INI 整行格式 `<中文>_<英文 option>_<spr_id>`,執行時抽英文進封包;
        // 玩家手填純英文(像 "re werewolf")也支援(extract_* 找不到合法格式回原值)。
        let opt = crate::aux::lhx_window::extract_polymorph_option(cond_raw);
        log_line!(
            "[status][transform] state[{}]=0 → 用「{}」+ option={:?} (param=0x{:08X})",
            TRANSFORM_STATE_ID,
            bi.name,
            opt,
            it.item_param
        );
        dh.execute_transform_scroll(h, it.item_param, opt)?;
    }
    Ok(())
}

/// 自動磨刀石單次 tick — 找揮舞中武器、檢查損壞度、call 遊戲 0x00410570。
///
/// 失敗回傳 Err(原因);成功(已 fire 或無需 fire)回傳 Ok(())。
fn whetstone_tick(h: HANDLE, dh: &Arc<crate::aux::drink_hook::DrinkHandle>) -> anyhow::Result<()> {
    let items = crate::aux::inventory::list_items(h)?;

    // 找揮舞中的武器(name 含「(揮舞)」)
    let weapon = items.iter().find(|it| it.name_lossy().contains("(揮舞)"));
    let Some(weapon) = weapon else {
        return Ok(()); // 沒揮舞武器,不報錯
    };

    // 讀 description string @ entry+0xA8(3.8 偏移)
    let desc_ptr = crate::memory::read_u32(h, weapon.entry_addr + 0xA8)?;
    if desc_ptr < 0x0010_0000 {
        return Ok(()); // description 還沒填好(剛裝備時可能空)
    }
    let desc_raw = crate::memory::read_bytes(h, desc_ptr, 256).unwrap_or_default();
    let end = desc_raw
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(desc_raw.len());
    let desc = &desc_raw[..end];
    // Big5「損壞度」= B7 6C C3 61 AB D7
    const DURABILITY_TAG: &[u8] = b"\xB7\x6C\xC3\x61\xAB\xD7";
    let needs_repair = desc
        .windows(DURABILITY_TAG.len())
        .any(|w| w == DURABILITY_TAG);
    if !needs_repair {
        return Ok(()); // 武器無 durability 或還沒掉血
    }

    // 找一顆磨刀石(name 完全等於「磨刀石」,strip 數量後比對)
    let whetstone = items
        .iter()
        .find(|it| crate::aux::lhx_window::strip_qty(&it.name_lossy()) == "磨刀石");
    let Some(stone) = whetstone else {
        anyhow::bail!("武器 {:?} 有損壞度,但背包沒磨刀石", weapon.name_lossy());
    };

    log_line!(
        "[status][whetstone] 磨 {}(0x{:08X}) ← {}(0x{:08X})",
        weapon.name_lossy(),
        weapon.item_param,
        stone.name_lossy(),
        stone.item_param
    );
    dh.execute_use_on_wielded(h, stone.item_param, weapon.item_param)?;
    Ok(())
}

/// 解毒 / 卡毒共用觸發 — 解析 INI 字串(可能是物品或 /ME 技能)後 dispatch。
fn fire_status_action(
    h: HANDLE,
    item_str: &str,
    dh: &Arc<crate::aux::drink_hook::DrinkHandle>,
    spell_book: &Arc<RwLock<Option<crate::aux::spell_book::SpellBook>>>,
    feature_tag: &str,
) {
    let bi = crate::aux::lhx_window::parse_buff_item(item_str);
    match bi.item_type {
        'I' => {
            let items = match crate::aux::inventory::list_items(h) {
                Ok(v) => v,
                Err(e) => {
                    log_line!("[status][{feature_tag}] 讀背包失敗: {e:#}");
                    return;
                }
            };
            let needle = crate::aux::lhx_window::strip_qty(&bi.name);
            let found = items
                .iter()
                .find(|it| crate::aux::lhx_window::strip_qty(&it.name_lossy()) == needle);
            match found {
                Some(it) => {
                    log_line!(
                        "[status][{feature_tag}] 偵測到中毒 → 用「{}」(entry=0x{:08X})",
                        bi.name,
                        it.entry_addr
                    );
                    if let Err(e) = dh.execute_use_item(h, it.entry_addr) {
                        log_line!("[status][{feature_tag}] execute_use_item 失敗: {e:#}");
                    }
                }
                None => log_line!("[status][{feature_tag}] 中毒但背包沒「{}」", bi.name),
            }
        }
        'S' => {
            let packed = match spell_book.read().as_ref().and_then(|b| b.lookup(&bi.name)) {
                Some(p) => p,
                None => {
                    log_line!("[status][{feature_tag}] 技能「{}」未學會,skip", bi.name);
                    return;
                }
            };
            let mode = crate::aux::drink_hook::SkillTargetMode::ForceSelfPacket;
            log_line!(
                "[status][{feature_tag}] 偵測到中毒 → 施放「{}」(packed={})",
                bi.name,
                packed
            );
            if let Err(e) = dh.execute_skill(h, packed, mode) {
                log_line!("[status][{feature_tag}] execute_skill 失敗: {e:#}");
            }
        }
        other => log_line!("[status][{feature_tag}] item_type {:?} 不支援", other),
    }
}

/// 物品欄一次性 dump(供 state_poll 第一輪呼叫)
/// 同時寫一份到 launcher.exe 旁的 `inventory_dump.txt`,讓使用者直接記事本打開看
fn dump_inventory(h: HANDLE) {
    use std::fmt::Write as _;
    let mut report = String::new();

    let _ = writeln!(
        &mut report,
        "=== Lineage 3.8 物品欄 dump @ {:?} ===",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );

    match crate::aux::inventory::list_items(h) {
        Ok(items) => {
            log_line!("[inv] 物品欄共 {} 件", items.len());
            let _ = writeln!(&mut report, "物品欄共 {} 件", items.len());
            for (i, it) in items.iter().enumerate() {
                let line = format!(
                    "#{:02}  entry=0x{:08X}  param=0x{:08X}  type=0x{:02X}  icon={}  eq={}  name={:?}",
                    i, it.entry_addr, it.item_param, it.item_type, it.icon, it.equipped, it.name_lossy()
                );
                log_line!("[inv] {line}");
                let _ = writeln!(&mut report, "{line}");
            }
        }
        Err(e) => {
            log_line!("[inv] 列舉失敗: {e:#}");
            let _ = writeln!(&mut report, "列舉失敗: {e:#}");
        }
    }

    // 寫一份 snapshot 到 launcher.exe 旁
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        let path = dir.join("inventory_dump.txt");
        if let Err(e) = std::fs::write(&path, &report) {
            log_line!("[inv] 寫 {path:?} 失敗: {e}");
        } else {
            log_line!("[inv] 完整列表已存到 {path:?}");
        }
    }
}

/// timer_8 EXP tick — LinHelperZ 顯示經驗值
///
/// 由 `AuxSettings::status_show_exp` 驅動 enable/disable;
/// settings off→on 時抓 baseline、推「啟動」綠字提示;on→off 時推「停止」白字提示。
///
/// 早期 return:settings 是 off 且 tracker 已 disable;或 game_state != 3。
fn exp_tick(
    h: HANDLE,
    settings: &Arc<RwLock<AuxSettings>>,
    tracker: &Arc<RwLock<crate::aux::exp_tracker::ExpTracker>>,
) {
    let want = settings.read().status_show_exp;
    let is_enabled = tracker.read().enabled;

    // settings off→on 與 on→off 的轉換都必須先驗證 game_state == 3,
    // 否則進場前 settings 變動會讀到 0 當 baseline、第一隻怪 delta 會大爆炸。
    let game_state = crate::memory::read_u32(h, crate::aux::address::G_GAME_STATE).unwrap_or(0);
    if game_state != 3 {
        // 在主畫面 / 選角時:若 settings 改 off,允許關掉(避免下次進場誤推延遲訊息);
        // 但 settings 改 on 必須等進場後才生效。
        if !want && is_enabled {
            tracker.write().disable();
        }
        return;
    }

    // 同步狀態 — 不推任何提示訊息,只內部抓 baseline / 清狀態
    if want && !is_enabled {
        if let Ok(total) = crate::aux::exp_tracker::read_total_exp(h) {
            tracker.write().enable(total);
        }
        return;
    }
    if !want && is_enabled {
        tracker.write().disable();
        return;
    }
    if !want {
        return;
    }

    let total = match crate::aux::exp_tracker::read_total_exp(h) {
        Ok(v) => v,
        Err(_) => return,
    };
    let report = {
        let mut t = tracker.write();
        t.tick(total)
    };
    if let Some(r) = report {
        // 推 in-game chat 走 path B(ChatDispatch + channel=-1),保留 auto-tail。
        // path A 直接寫 buffer 會破壞自動捲動到底,故不採用。\F2 = palette 綠。
        let mut line_bytes = b"\\F2".to_vec();
        line_bytes.extend_from_slice(&crate::aux::exp_tracker::format_chat_line(&r));
        if let Err(e) = crate::aux::chat::push_chat_via_dispatch(
            h,
            &line_bytes,
            0xFFFF,
            crate::aux::chat::color::GREEN,
        ) {
            log_line!("[exp_tracker] push chat 失敗: {e}");
        }
    }
}

/// 通用 tick 迴圈 — 直到 cancel 為止
fn tick_loop<F: FnMut(&str)>(name: &str, interval: Duration, cancel: Arc<AtomicBool>, mut work: F) {
    log_line!("[aux/{}] thread 啟動", name);
    while !cancel.load(Ordering::Relaxed) {
        work(name);
        std::thread::sleep(interval);
    }
    log_line!("[aux/{}] thread 結束", name);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn potion_row_default() {
        let r = PotionRow::default();
        assert!(!r.enabled);
        assert_eq!(r.threshold, 0);
        assert_eq!(r.item, "");
    }

    #[test]
    fn mp_when_safe_default() {
        let m = MpWhenSafe::default();
        assert!(!m.enabled);
        assert_eq!(m.hp_lower, 0);
        assert_eq!(m.mp_upper, 0);
        assert_eq!(m.item, "");
    }

    #[test]
    fn mp_when_safe_triggers_when_raw_hp_is_safe_and_mp_is_low() {
        let mut s = AuxSettings::default();
        s.mp_when_safe.enabled = true;
        s.mp_when_safe.hp_lower = 800;
        s.mp_when_safe.mp_upper = 20;
        s.mp_when_safe.item = "心靈轉換/M".to_string();

        let state = crate::aux::player_state::PlayerState {
            hp: 900,
            max_hp: 1000,
            mp: 10,
            max_mp: 100,
            ..Default::default()
        };

        assert!(mp_when_safe_triggered(&s, &state));
    }

    #[test]
    fn mp_when_safe_triggers_when_percent_hp_is_safe_and_mp_is_low() {
        let mut s = AuxSettings::default();
        s.potion_use_percent = true;
        s.mp_when_safe.enabled = true;
        s.mp_when_safe.hp_lower = 80;
        s.mp_when_safe.mp_upper = 20;
        s.mp_when_safe.item = "魂體轉換/M".to_string();

        let state = crate::aux::player_state::PlayerState {
            hp: 850,
            max_hp: 1000,
            mp: 19,
            max_mp: 100,
            ..Default::default()
        };

        assert!(mp_when_safe_triggered(&s, &state));
    }

    #[test]
    fn mp_when_safe_does_not_trigger_when_disabled_or_empty() {
        let mut s = AuxSettings::default();
        s.mp_when_safe.enabled = true;
        s.mp_when_safe.hp_lower = 800;
        s.mp_when_safe.mp_upper = 20;

        let state = crate::aux::player_state::PlayerState {
            hp: 900,
            max_hp: 1000,
            mp: 10,
            max_mp: 100,
            ..Default::default()
        };

        assert!(!mp_when_safe_triggered(&s, &state));

        s.mp_when_safe.item = "心靈轉換/M".to_string();
        s.mp_when_safe.enabled = false;
        assert!(!mp_when_safe_triggered(&s, &state));
    }

    #[test]
    fn misc_toggles_default_all_false() {
        let m = MiscToggles::default();
        assert!(!m.all_day);
        assert!(!m.underwater_pump);
        assert!(!m.low_cpu);
        assert!(!m.monster_level_color);
        assert!(!m.show_clock);
        assert!(!m.show_attack_dmg);
    }

    #[test]
    fn attack_damage_hook_module_is_available() {
        assert!(!crate::aux::attack_damage_hook::is_installed());
    }

    #[test]
    fn timer_row_default() {
        let t = TimerRow::default();
        assert!(!t.enabled);
        assert_eq!(t.interval_sec, 5);
        assert_eq!(t.command, "");
    }

    #[test]
    fn aux_settings_default_8tabs() {
        let s = AuxSettings::default();
        assert_eq!(s.current_profile, "");

        // tab1 喝水
        assert_eq!(s.potion_rows.len(), 7);
        assert!(!s.potion_rows[0].enabled);
        assert!(!s.mp_when_safe.enabled);
        assert!(!s.potion_use_percent);
        assert!(!s.potion_show_inventory);

        // tab2 輔助
        assert!(!s.buff_enabled);
        assert_eq!(s.buff_items.len(), 0);

        // tab3 狀態
        assert!(!s.status_show_exp);
        assert_eq!(s.fkey_macros.len(), 4);
        assert!(!s.fkey_macros[0].enabled);

        // tab4 刪物
        assert!(!s.delete_enabled);
        assert!(s.delete_list.is_empty());
        assert!(s.dissolve_list.is_empty());

        // tab5 喊話
        assert!(!s.shout_enabled);
        assert_eq!(s.shout_interval_sec, 0);

        // tab6 其他
        assert!(!s.misc.all_day);

        // tab7 定時
        assert!(!s.timer_master_enabled);
        assert_eq!(s.timer_rows.len(), 6);
        assert_eq!(s.timer_rows[0].interval_sec, 5);
    }

    #[test]
    fn aux_settings_is_clone() {
        let s = AuxSettings::default();
        let s2 = s.clone();
        assert_eq!(s.current_profile, s2.current_profile);
    }

    #[test]
    fn delete_lists_default_empty() {
        let s = AuxSettings::default();
        assert!(!s.delete_enabled);
        assert!(s.delete_list.is_empty());
        assert!(s.dissolve_list.is_empty());
    }

    #[test]
    fn delete_lists_serde_roundtrip() {
        let mut s = AuxSettings::default();
        s.delete_enabled = true;
        s.delete_list = vec!["+7 馬爾斯奇古劍".to_string(), "破布".to_string()];
        s.dissolve_list = vec!["+0 鋼刀".to_string()];
        let json = serde_json::to_string(&s).expect("serialize");
        let back: AuxSettings = serde_json::from_str(&json).expect("deserialize");
        assert!(back.delete_enabled);
        assert_eq!(back.delete_list, vec!["+7 馬爾斯奇古劍", "破布"]);
        assert_eq!(back.dissolve_list, vec!["+0 鋼刀"]);
    }

    #[test]
    fn delete_tick_picks_delete_list_first() {
        let delete_list = vec!["破布".to_string()];
        let dissolve_list = vec!["+0 鋼刀".to_string()];
        let inv = vec!["破布".to_string(), "+0 鋼刀".to_string()];
        let pick = pick_delete_action(&delete_list, &dissolve_list, &inv);
        assert_eq!(pick, Some(("delete", "破布".to_string())));
    }

    #[test]
    fn delete_tick_falls_back_to_dissolve_when_delete_list_empty() {
        let delete_list: Vec<String> = vec![];
        let dissolve_list = vec!["+0 鋼刀".to_string()];
        let inv = vec!["破布".to_string(), "+0 鋼刀".to_string()];
        let pick = pick_delete_action(&delete_list, &dissolve_list, &inv);
        assert_eq!(pick, Some(("dissolve", "+0 鋼刀".to_string())));
    }

    #[test]
    fn delete_tick_skips_equipped_items() {
        let delete_list = vec!["+7 馬爾斯奇古劍".to_string()];
        let dissolve_list: Vec<String> = vec![];
        let inv = vec!["+7 馬爾斯奇古劍 (揮舞)".to_string()];
        let pick = pick_delete_action(&delete_list, &dissolve_list, &inv);
        assert_eq!(pick, None);
    }

    #[test]
    fn delete_tick_no_match_returns_none() {
        let delete_list = vec!["不存在的物品".to_string()];
        let dissolve_list: Vec<String> = vec![];
        let inv = vec!["別的東西".to_string()];
        assert_eq!(pick_delete_action(&delete_list, &dissolve_list, &inv), None);
    }

    #[test]
    fn pick_shout_message_empty_returns_none() {
        let msgs: Vec<String> = vec![];
        assert_eq!(pick_shout_message(&msgs, 0), None);
        // next_idx 任意值都不該 panic
        assert_eq!(pick_shout_message(&msgs, 999), None);
    }

    #[test]
    fn pick_shout_message_round_robin_advances_idx() {
        let msgs = vec![
            "第一則".to_string(),
            "第二則".to_string(),
            "第三則".to_string(),
        ];
        // idx 0 → 拿第一則,下一個 idx 變 1
        assert_eq!(
            pick_shout_message(&msgs, 0),
            Some(("第一則".to_string(), 1))
        );
        // idx 1 → 拿第二則,下一個變 2
        assert_eq!(
            pick_shout_message(&msgs, 1),
            Some(("第二則".to_string(), 2))
        );
    }

    #[test]
    fn pick_shout_message_idx_wraps_modulo_len() {
        let msgs = vec!["A".to_string(), "B".to_string()];
        // idx 2 % 2 = 0 → 拿 A,下一個 wrap 到 1
        assert_eq!(pick_shout_message(&msgs, 2), Some(("A".to_string(), 1)));
        // idx = len-1 → 下一個 wrap 回 0
        assert_eq!(pick_shout_message(&msgs, 1), Some(("B".to_string(), 0)));
        // 超大 idx 也照 modulo 處理
        assert_eq!(pick_shout_message(&msgs, 1001), Some(("B".to_string(), 0)));
    }

    #[test]
    fn old_profile_json_without_delete_lists_still_loads() {
        // 模擬舊版 JSON(沒 delete_list / dissolve_list / delete_enabled)
        // 用最小可解析 JSON,加上 AuxSettings 裡其他必填欄位的預設值
        let s_default = AuxSettings::default();
        let mut json_value = serde_json::to_value(&s_default).expect("serialize default");
        // 模擬舊檔 — 把三個新欄位拿掉
        let obj = json_value.as_object_mut().expect("object");
        obj.remove("delete_enabled");
        obj.remove("delete_list");
        obj.remove("dissolve_list");
        let old_json = serde_json::to_string(&json_value).expect("re-serialize");

        // 反序列化必須成功(不是 fallback default,是 graceful)
        let s: AuxSettings = serde_json::from_str(&old_json)
            .expect("舊版 JSON 應能 deserialize 成功(透過 #[serde(default)])");
        assert!(!s.delete_enabled);
        assert!(s.delete_list.is_empty());
        assert!(s.dissolve_list.is_empty());
    }

    fn make_timer_row(enabled: bool, interval_sec: u32, command: &str) -> TimerRow {
        TimerRow {
            enabled,
            interval_sec,
            command: command.to_string(),
        }
    }

    #[test]
    fn pick_timer_action_master_off_returns_none() {
        let now = std::time::Instant::now();
        let rows: [TimerRow; 6] = std::array::from_fn(|_| make_timer_row(true, 5, "肉/I"));
        let last_fire: [Option<std::time::Instant>; 6] = [None; 6];
        assert_eq!(pick_timer_action(&rows, &last_fire, false, now), None);
    }

    #[test]
    fn pick_timer_action_all_disabled_returns_none() {
        let now = std::time::Instant::now();
        let rows: [TimerRow; 6] = std::array::from_fn(|_| make_timer_row(false, 5, "肉/I"));
        let last_fire: [Option<std::time::Instant>; 6] = [None; 6];
        assert_eq!(pick_timer_action(&rows, &last_fire, true, now), None);
    }

    #[test]
    fn pick_timer_action_row0_due_picks_0() {
        let now = std::time::Instant::now();
        let mut rows: [TimerRow; 6] = std::array::from_fn(|_| make_timer_row(false, 5, ""));
        rows[0] = make_timer_row(true, 5, "肉/I");
        let last_fire: [Option<std::time::Instant>; 6] = [None; 6];
        assert_eq!(pick_timer_action(&rows, &last_fire, true, now), Some(0));
    }

    #[test]
    fn pick_timer_action_multiple_due_picks_smallest_idx() {
        let now = std::time::Instant::now();
        let mut rows: [TimerRow; 6] = std::array::from_fn(|_| make_timer_row(false, 5, ""));
        rows[0] = make_timer_row(true, 5, "肉/I");
        rows[1] = make_timer_row(true, 5, "保護罩/ME");
        let last_fire: [Option<std::time::Instant>; 6] = [None; 6];
        assert_eq!(pick_timer_action(&rows, &last_fire, true, now), Some(0));
    }

    #[test]
    fn pick_timer_action_empty_command_skipped() {
        let now = std::time::Instant::now();
        let mut rows: [TimerRow; 6] = std::array::from_fn(|_| make_timer_row(false, 5, ""));
        rows[0] = make_timer_row(true, 5, ""); // enabled 但 command 空
        rows[1] = make_timer_row(true, 5, "肉/I");
        let last_fire: [Option<std::time::Instant>; 6] = [None; 6];
        assert_eq!(pick_timer_action(&rows, &last_fire, true, now), Some(1));
    }

    #[test]
    fn pick_timer_action_last_fire_none_treated_as_due() {
        let now = std::time::Instant::now();
        let mut rows: [TimerRow; 6] = std::array::from_fn(|_| make_timer_row(false, 5, ""));
        rows[2] = make_timer_row(true, 60, "強身術/M");
        let last_fire: [Option<std::time::Instant>; 6] = [None; 6];
        assert_eq!(pick_timer_action(&rows, &last_fire, true, now), Some(2));
    }

    #[test]
    fn pick_timer_action_not_yet_due_returns_none() {
        let now = std::time::Instant::now();
        let mut rows: [TimerRow; 6] = std::array::from_fn(|_| make_timer_row(false, 5, ""));
        rows[0] = make_timer_row(true, 60, "強身術/M");
        // last_fire 1 秒前(interval=60s,還差 59 秒)
        let mut last_fire: [Option<std::time::Instant>; 6] = [None; 6];
        last_fire[0] = Some(now - std::time::Duration::from_secs(1));
        assert_eq!(pick_timer_action(&rows, &last_fire, true, now), None);
    }
}
