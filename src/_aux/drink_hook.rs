//! 自動喝水 / 物品使用 / 技能施放 — CreateRemoteThread 一次性執行
//!
//! 為什麼選 CreateRemoteThread 而非 inline hook:每次要使用物品時 launcher 端
//! 動態建立一條 remote thread 跑 18-byte shellcode 呼叫 USE_ITEM,跑完立刻釋放
//! 記憶體。整個流程結束後遊戲記憶體乾淨,沒有殘留 hook,反偵測上比 inline hook 安全。
//!
//! 執行步驟:
//!   1. VirtualAllocEx 分配一塊 RWX
//!   2. WriteProcessMemory 寫入 shellcode(pushad / push item_entry / call USE_ITEM / cleanup / popad / ret)
//!   3. CreateRemoteThread 觸發
//!   4. WaitForSingleObject 等回來(5s timeout)
//!   5. CloseHandle + VirtualFreeEx
//!
//! 為什麼放棄 codecave hook 路線:
//! 1. ProcessPacket 0x539333 在 packed runtime 是 dead code(hook 從未觸發)
//! 2. PeekMessageW hook 觸發了,但 shellcode call 的 3 個遊戲函數位址在 packed
//!    runtime 全部錯位(DB 內是脫殼後的靜態位址,packer 把函數重新排列了)
//! 3. 已驗證遊戲對 remote thread 沒有偵測,長期跑 OK,反偵測風險低。
//!
//! 唯一參數:**3.8 USE_ITEM 函數位址**(cdecl, 1 參,已透過 spy hook backtrace
//! 鎖定 = `0x004B3EE0`,prologue `55 8B EC 83 EC 0C`,20+ caller)。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Memory::{VirtualFreeEx, MEM_RELEASE};
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeThread, WaitForSingleObject,
};

use crate::aux::address;
use crate::logger::log_line;
use crate::memory::{alloc_exec, read_bytes, write_code};

/// 喝水冷卻（Rust 端，不依賴遊戲內 GetTickCount）
const DRINK_COOLDOWN: Duration = Duration::from_millis(500);

/// CreateRemoteThread 等待逾時（ms）。USE_ITEM 應在 < 50ms 完成。
const THREAD_WAIT_MS: u32 = 5_000;

/// shellcode 分配大小(含 ret)— drink 18B、skill 42B、whetstone 42B、transform 50~120B
/// (transform IP packet 含可變長度 option string,最長預留 ~80B)。
const SHELLCODE_SIZE: usize = 128;

/// 目標 ID 全域(`[0x97C910]`)— cast_magic dispatcher 讀此值當 cast target;
/// 等價於玩家鼠標 hover 在某物件上(hover 時 mouse handler 會設此值)。
///
/// **`/ME` 路徑(launcher 自動施放自身 buff)** 必須在 call spell_book_cast 前
/// 把此值設成 [`SELF_CHAR_ID_ADDR`] 的內容,否則 dispatcher 走 self path 時
/// 因為 `[magic_info+8]` 為 0 通不過 ready check,封包不會送出。
const TARGET_ID_ADDR: u32 = 0x0097C910;

/// 玩家自己的 char_id (`[0xABF4B4]`,進場後填入,跨重啟值不變但 attach 後讀才知道)。
///
/// 為什麼需要:`/ME` 後綴(指定 target = 自己)的施放路徑必須在 cast_magic dispatcher
/// 之前先把 `[0x97C910] = [0xABF4B4]`,dispatcher 才會走 target path 拿玩家物件、
/// 送 op=6 self-cast 封包。若不寫 target_id 留 garbage,dispatcher 會 ERROR 把
/// 整條 cast 鎖死。
const SELF_CHAR_ID_ADDR: u32 = 0x00ABF4B4;

/// `spell_book_cast` 第二參 byte_flag — 1 = 手動 manual cast(玩家點技能書的路徑)。
const SPELL_BOOK_BYTE_FLAG: u32 = 1;

/// `execute_skill` 的 target 模式,決定 shellcode 怎麼處理 `[0x97C910]`(target_id)。
#[derive(Copy, Clone, Debug)]
pub enum SkillTargetMode {
    /// `/ME` — 指定自己,強制 `[0x97C910] = [SELF_CHAR_ID_ADDR]`(自己 char_id)
    /// 走 spell_book_cast 路徑。對「純自身 buff」(加速術/火焰武器/保護罩等)有效,
    /// 因為 spell_book_cast 內部對純自身 buff 強制 target=self,我們寫 [0x97C910] 沒影響。
    SelfCast,
    /// `/M` — 不指定 target,完全不動 `[0x97C910]`,讓 dispatcher 用當下值
    /// 注意:3.8 dispatcher 仍會讀 `[0x97C910]`,若殘留 garbage 會被 server ERROR;
    /// 自身 buff 建議用 `/ME` 顯式指定自己,避開此問題
    NoSpec,
    /// `/M=name` `/M?name` `/M??name` — 指定 char_id(已從 entity list 解析出)
    Explicit(u32),
    /// **bypass spell_book_cast,直接組 C_SKILL packet 送 SendPacketData,target=self_char_id**。
    /// 用於「可指定他人的輔助 buff」(體魄強健術 / 通暢氣脈術 等)— 這類 spell_book_cast
    /// 會自家解析 mouse hover 當 target,我們寫 [0x97C910] 會被覆蓋。
    /// 直接送 packet 繞過該路徑,server 視同玩家手動點自己施放。
    ///
    /// 風險:bypass 了 client cooldown / MP 檢查,server 可能 ERROR(技能不在書上 / MP 不足);
    /// caller 應加 ≥2s cooldown 保險。
    ForceSelfPacket,
    /// **bypass spell_book_cast,直接組 C_SKILL packet,target=任意 id (imm32)**。
    /// 用於 `/MIA` `/MIW` `/MI`(對物品施法)— item.item_param 不是世界 entity id,
    /// 0x73C260 dispatcher 在 `find_entity_by_id` 會 NULL 出來掉到 fallback path
    /// (吃 `[0xABF440]` UI hover target),根本不會用到 [0x97C910] 的 item_param。
    /// 直接送 packet 跳過 client 端 entity lookup,opcode 6 "cccd" 結構讓 server
    /// 收到後在 inventory namespace 找到該 obj 完成施法。
    ForceTargetPacket(u32),
}

/// DrinkHandle — 不需要遊戲 HANDLE 即可建立。
///
/// `use_item_addr` 由 caller 在進場後 reverse 出來填入。3.8 client = `0x004B3EE0`。
///
/// `prologue_snapshot` 為第一次 execute 時讀的 16 bytes,後續每次比對 — 如果 packer
/// 在登出/登入間把該頁重新加密或 relocate,就能立刻看到差異 → 不嘗試 call,避免 crash。
pub struct DrinkHandle {
    use_item_addr: u32,
    last_drink: Mutex<Option<Instant>>,
    prologue_snapshot: Mutex<Option<[u8; 16]>>,
}

impl DrinkHandle {
    /// 建構（不需要遊戲 HANDLE，純結構）
    pub fn new(use_item_addr: u32) -> Self {
        Self {
            use_item_addr,
            last_drink: Mutex::new(None),
            prologue_snapshot: Mutex::new(None),
        }
    }

    /// 取得 USE_ITEM 位址（給 log 用）
    pub fn use_item_addr(&self) -> u32 {
        self.use_item_addr
    }

    /// 一次性執行喝水(HP 補水路徑) — 走內建 500ms cooldown,避免 HP 還沒回升就連噴。
    pub fn execute_drink(&self, h: HANDLE, item_entry: u32) -> Result<()> {
        self.execute_internal(h, item_entry, true)
    }

    /// 直接刪除物品 — 走 C_DELETE_ITEM packet 而非 client UI 路徑。
    ///
    /// 為什麼用 packet 而不 UI 模擬:UI 路徑(拖到垃圾桶 icon)要動滑鼠 + 額外
    /// confirmation dialog,程式自動化不穩。直接 SendPacketData 一行送出去由
    /// server 處理,確定性高。opcode + format 由 address.rs::C_DELETE_ITEM 決定。
    /// `count` 應是當下整疊數量(從 `Item.count` = `[entry+0xA0]` 取),0 也合法
    /// 但 server 會視為「刪 0 個 = no-op」,堆疊物必須帶整疊值。
    ///
    /// **不走** DrinkHandle 內建 cooldown — caller(delete_tick)已經一個 tick 一個 packet。
    pub fn execute_delete(&self, h: HANDLE, item_obj_id: u32, count: u32) -> Result<()> {
        if crate::aux::address::C_DELETE_ITEM.is_none() {
            bail!("C_DELETE_ITEM opcode 未定(address.rs 還沒填),Phase 1 RE 未完成");
        }
        let sc = build_delete_packet_shellcode(item_obj_id, count);
        self.run_remote_call(h, &sc, "delete")
    }

    /// 對「揮舞中武器」使用消耗品(II 路徑 — 磨刀石、修理工具等)。
    ///
    /// 為什麼不呼叫遊戲 `0x00410570` 那層 wrapper:走 RemoteThread 直接 call 那層
    /// wrapper 會被 server 踢線(內部有 ECX 結構檢查,從 remote thread 進場時
    /// 該結構未初始化)。改用 SendPacketData 直接組 II packet:
    ///
    /// ```c
    /// SendPacketData("cdd", 0xA4, weapon.item_param, source.item_param);
    /// ```
    ///
    /// **arg 順序**(2026-05-02 spy capture #14 驗證,真實遊戲手動磨刀的封包):
    /// `cdd` 格式第一個 `d` = 目標(揮舞武器),第二個 `d` = 來源(磨刀石)。
    /// 寫反會被 server reject。
    ///
    /// caller 必須先從 inventory 解出兩個 item_param(`Item::item_param`),不再從
    /// 全域指標讀 — 第一版假設 `[0x00972EDC]` 是揮舞武器 entry pointer 但未驗證,
    /// shellcode 在 `mov edx, [eax+4]` 處 AV 退出,SendPacketData 完全沒被呼叫。
    ///
    /// 不走 DrinkHandle 內建 cooldown — caller 自己 throttle(磨刀石動作頻率不高)。
    pub fn execute_use_on_wielded(
        &self,
        h: HANDLE,
        whetstone_item_param: u32,
        weapon_item_param: u32,
    ) -> Result<()> {
        let sc = build_whetstone_packet_shellcode(whetstone_item_param, weapon_item_param);
        self.run_remote_call(h, &sc, "whetstone")
    }

    /// 變形卷軸 — 把卷軸 obj_id + 變身選項字串組成 II 封包送出。
    ///
    /// 為什麼這條路徑特別:一般物品 use 是 1 個參數,變形卷軸要 server 額外知道「變
    /// 成什麼」(死亡騎士 / 狼 / etc.),所以走 `"cds"` 格式多帶一個字串參數。
    ///
    /// 對齊 2026-05-02 spy capture #139:
    /// `SendPacketData("cds", 0xA4, scroll.item_param, option_string_ptr)` —
    /// `option_string` 是 ASCII null-terminated(像 "death 80"、"wolf"),
    /// shellcode 用 IP-relative 把字串嵌在 codecave 裡面,call 時 push 它的 runtime 位址。
    ///
    /// `option_string` 最多 80 bytes(SHELLCODE_SIZE=128 - 43B prefix - 4B "cds\0" - 1B null)。
    pub fn execute_transform_scroll(
        &self,
        h: HANDLE,
        scroll_item_param: u32,
        option_string: &str,
    ) -> Result<()> {
        let sc = build_transform_packet_shellcode(scroll_item_param, option_string);
        if sc.len() > SHELLCODE_SIZE {
            bail!(
                "transform shellcode {} bytes 超過 SHELLCODE_SIZE {} — option_string 太長",
                sc.len(),
                SHELLCODE_SIZE
            );
        }
        self.run_remote_call(h, &sc, "transform")
    }

    /// 一次性使用物品(buff 補回路徑) — **不走** DrinkHandle 內建 cooldown。
    /// buff_tick 已經有 per-buff 2s cooldown,粒度足夠;DrinkHandle 全域 500ms cooldown
    /// 反而會在「同一個 tick 想連補多個 buff」或「tick 邊界 timing jitter」時擋住合法請求。
    pub fn execute_use_item(&self, h: HANDLE, item_entry: u32) -> Result<()> {
        self.execute_internal(h, item_entry, false)
    }

    /// 送 chat packet — 喊話 / 一般訊息 / dialog reply 通用入口。
    ///
    /// `channel` 取自 [`crate::aux::address::CHAT_CHANNEL_SHOUT`] 等常數;`message`
    /// 是 UTF-8 String,shellcode 內會 Big5 編碼後內嵌。
    ///
    /// **不走** DrinkHandle 內建 cooldown — caller(shout_tick)自己用 interval 控制節奏。
    pub fn execute_chat(&self, h: HANDLE, channel: u8, message: &str) -> Result<()> {
        let sc = build_chat_packet_shellcode(channel, message);
        if sc.len() > SHELLCODE_SIZE {
            bail!(
                "chat shellcode {} bytes 超過 SHELLCODE_SIZE {} — message 太長",
                sc.len(),
                SHELLCODE_SIZE
            );
        }
        self.run_remote_call(h, &sc, "chat")
    }

    /// 一次性施法 — 走**手動施法的等價路徑** `spell_book::cast` (0x73ECE0)。
    ///
    /// `packed_skill_id` 來自 [`crate::aux::spell_db::SpellDb`] 的 `name → packed` 查詢。
    /// `target_mode` 控制 `[0x97C910]`(target_id)在 call 前如何設定:
    /// - [`SkillTargetMode::SelfCast`] (`/ME`) — 設成 `[0xABF4B4]` 自己 char_id(指定自己)
    /// - [`SkillTargetMode::NoSpec`]   (`/M`)  — 完全不動,讓 dispatcher 用當下值(不指定 target)
    /// - [`SkillTargetMode::Explicit(id)`] — 設成指定的 char_id(由 caller 從 entity scan 解析)
    ///
    /// 不論哪個 mode,call 後都會還原 `[0x97C910]` 為原值。
    /// 跟 `execute_use_item` 一樣**不走** DrinkHandle 內建 cooldown。
    pub fn execute_skill(
        &self,
        h: HANDLE,
        packed_skill_id: u32,
        target_mode: SkillTargetMode,
    ) -> Result<()> {
        let sc = build_skill_shellcode(packed_skill_id, target_mode);
        self.run_remote_call(h, &sc, "skill")
    }

    fn execute_internal(&self, h: HANDLE, item_entry: u32, respect_cooldown: bool) -> Result<()> {
        // 1. cooldown check（Rust 端 Instant，不靠遊戲 tick）
        if respect_cooldown {
            let mut last = self.last_drink.lock().unwrap();
            if let Some(t) = *last {
                if t.elapsed() < DRINK_COOLDOWN {
                    bail!(
                        "still cooling down ({}ms left)",
                        (DRINK_COOLDOWN - t.elapsed()).as_millis()
                    );
                }
            }
            *last = Some(Instant::now());
        }

        // 1b. 讀取 USE_ITEM 函數前 16 bytes,跟首次 snapshot 比對
        //     packer 若把該頁重新加密 / relocate,bytes 會變 — 直接 log 警告。
        let cur_bytes = match read_bytes(h, self.use_item_addr, 16) {
            Ok(b) => {
                let mut arr = [0u8; 16];
                arr.copy_from_slice(&b);
                arr
            }
            Err(e) => bail!(
                "讀 USE_ITEM @ 0x{:08X} 失敗(函數頁可能被釋放): {e:#}",
                self.use_item_addr
            ),
        };
        {
            let mut snap = self.prologue_snapshot.lock().unwrap();
            match *snap {
                None => {
                    let hex: String = cur_bytes
                        .iter()
                        .map(|b| format!("{b:02X}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    log_line!(
                        "[drink] USE_ITEM @ 0x{:08X} 首次快照 prologue 16 bytes: {hex}",
                        self.use_item_addr
                    );
                    *snap = Some(cur_bytes);
                }
                Some(orig) if orig != cur_bytes => {
                    let cur_hex: String = cur_bytes
                        .iter()
                        .map(|b| format!("{b:02X}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    let orig_hex: String = orig
                        .iter()
                        .map(|b| format!("{b:02X}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    log_line!(
                        "[drink] ⚠ USE_ITEM prologue 變動! 原: {orig_hex} → 現: {cur_hex} (packer 可能重新加密/relocate)"
                    );
                }
                Some(_) => {} // bytes 相同,正常
            }
        }

        // 2. 組 shellcode
        let sc = build_shellcode(self.use_item_addr, item_entry);

        // 3-7. 共用 alloc/write/thread/wait/free
        self.run_remote_call(h, &sc, "drink")
    }

    /// 跑一段 shellcode(VirtualAllocEx → WriteProcessMemory → CreateRemoteThread →
    /// WaitForSingleObject → CloseHandle → VirtualFreeEx)— 不論成功失敗一律 free。
    ///
    /// `label` 用於 log("drink"/"skill"等),方便除錯時區分來源。
    fn run_remote_call(&self, h: HANDLE, sc: &[u8], label: &str) -> Result<()> {
        let remote_addr = alloc_exec(h, SHELLCODE_SIZE)
            .with_context(|| format!("VirtualAllocEx for {label} shellcode"))?;

        let result = (|| -> Result<()> {
            write_code(h, remote_addr, sc)
                .with_context(|| format!("WriteProcessMemory @ 0x{remote_addr:08X}"))?;

            let mut tid = 0u32;
            let thread_handle = unsafe {
                CreateRemoteThread(
                    h,
                    None,
                    0,
                    Some(std::mem::transmute(remote_addr as usize)),
                    None,
                    0,
                    Some(&mut tid),
                )
            }
            .with_context(|| format!("CreateRemoteThread for {label} shellcode"))?;

            let wait_result = unsafe { WaitForSingleObject(thread_handle, THREAD_WAIT_MS) };
            let mut exit_code: u32 = 0;
            let _ = unsafe { GetExitCodeThread(thread_handle, &mut exit_code) };
            unsafe { CloseHandle(thread_handle).ok() };

            if wait_result == WAIT_TIMEOUT {
                bail!(
                    "CreateRemoteThread {label} timeout {} ms (tid={tid}),目標函數卡住或位址錯誤",
                    THREAD_WAIT_MS
                );
            }
            if wait_result != WAIT_OBJECT_0 {
                bail!(
                    "CreateRemoteThread {label} 等待非預期 (wait={:?}, tid={tid}, exit=0x{exit_code:08X})",
                    wait_result
                );
            }
            log_line!("[{label}] thread tid={tid} exit=0x{exit_code:08X}");
            Ok(())
        })();

        unsafe {
            let _ = VirtualFreeEx(h, remote_addr as *mut _, 0, MEM_RELEASE);
        }

        result
    }
}

// ─── Shellcode 組裝 ────────────────────────────────────────────

/// 組合 18-byte shellcode (cdecl 1-arg 呼叫,通用於 USE_ITEM / 0x00410570 等):
/// ```asm
/// 60                  ; pushad
/// 68 <arg u32>        ; push arg
/// B8 <fn_addr u32>    ; mov eax, fn_addr
/// FF D0               ; call eax
/// 83 C4 04            ; add esp, 4   (cdecl cleanup)
/// 61                  ; popad
/// C3                  ; ret
/// ```
fn build_one_arg_call_shellcode(fn_addr: u32, arg: u32) -> Vec<u8> {
    let mut sc = Vec::with_capacity(14);
    sc.push(0x60); // pushad
    sc.push(0x68); // push imm32
    sc.extend_from_slice(&arg.to_le_bytes());
    sc.push(0xB8); // mov eax, imm32
    sc.extend_from_slice(&fn_addr.to_le_bytes());
    sc.extend_from_slice(&[0xFF, 0xD0]); // call eax
    sc.extend_from_slice(&[0x83, 0xC4, 0x04]); // add esp, 4
    sc.push(0x61); // popad
    sc.push(0xC3); // ret
    sc
}

/// `build_one_arg_call_shellcode` 的舊名,保留給既有 `execute_use_item` 路徑。
fn build_shellcode(use_item_addr: u32, item_entry: u32) -> Vec<u8> {
    build_one_arg_call_shellcode(use_item_addr, item_entry)
}

/// 組合 thiscall spell_book::cast shellcode,依 [`SkillTargetMode`] 變動 target 設定段:
///
/// 共通骨架(end-to-end save/restore `[0x97C910]`):
/// ```asm
/// 60                              ; pushad
/// A1 <TARGET>                     ; mov eax, [TARGET]
/// 50                              ; push eax                       ; 暫存原 target
/// <-- TARGET SETUP (mode 相依) -->
/// 6A 01                           ; push 1                         ; byte_flag
/// 68 <packed>                     ; push packed
/// 8B 0D <SPELL_BOOK_PTR>          ; mov ecx, [SPELL_BOOK_PTR]
/// B8 <SPELL_BOOK_CAST>            ; mov eax, SPELL_BOOK_CAST
/// FF D0                           ; call eax                       ; thiscall, ret 8
/// 58                              ; pop eax
/// A3 <TARGET>                     ; mov [TARGET], eax              ; 還原
/// 61 C3                           ; popad; ret
/// ```
///
/// TARGET SETUP 段:
/// - [`SkillTargetMode::SelfCast`] — 10 bytes:
///   `A1 <SELF_ID> A3 <TARGET>` (mov eax,[SELF]; mov [TARGET],eax)
/// - [`SkillTargetMode::NoSpec`]   — 0 bytes(直接跳過)
/// - [`SkillTargetMode::Explicit(id)`] — 10 bytes:
///   `C7 05 <TARGET> <id>` (mov dword [TARGET], imm32) — 比 SELF 多 1 byte 但與 SELF 相同空間,
///   為了 shellcode 大小一致,直接用 mov imm32 form。
///
/// 結果 size:SelfCast/Explicit 都 45 bytes,NoSpec 35 bytes,ForceSelfPacket 32 bytes
/// (SHELLCODE_SIZE=64 都容得下)。
fn build_skill_shellcode(packed_skill_id: u32, target_mode: SkillTargetMode) -> Vec<u8> {
    if let SkillTargetMode::ForceSelfPacket = target_mode {
        return build_force_self_packet_shellcode(packed_skill_id);
    }
    if let SkillTargetMode::ForceTargetPacket(target_id) = target_mode {
        return build_force_target_packet_shellcode(packed_skill_id, target_id);
    }

    let mut sc = Vec::with_capacity(48);
    // pushad
    sc.push(0x60);
    // mov eax, [TARGET_ID_ADDR]
    sc.push(0xA1);
    sc.extend_from_slice(&TARGET_ID_ADDR.to_le_bytes());
    // push eax (save 原 target_id)
    sc.push(0x50);

    // ── target setup 區段(mode 相依) ─────────────────────────
    match target_mode {
        SkillTargetMode::SelfCast => {
            // mov eax, [SELF_CHAR_ID_ADDR]; mov [TARGET], eax
            sc.push(0xA1);
            sc.extend_from_slice(&SELF_CHAR_ID_ADDR.to_le_bytes());
            sc.push(0xA3);
            sc.extend_from_slice(&TARGET_ID_ADDR.to_le_bytes());
        }
        SkillTargetMode::NoSpec => {
            // 不動 [TARGET] — 由 dispatcher 用當下狀態(玩家鼠標 hover)
        }
        SkillTargetMode::Explicit(id) => {
            // mov dword [TARGET], imm32
            sc.extend_from_slice(&[0xC7, 0x05]);
            sc.extend_from_slice(&TARGET_ID_ADDR.to_le_bytes());
            sc.extend_from_slice(&id.to_le_bytes());
        }
        SkillTargetMode::ForceSelfPacket => unreachable!("已在函數開頭分流"),
        SkillTargetMode::ForceTargetPacket(_) => unreachable!("已在函數開頭分流"),
    }

    // ── 共通呼叫段 ───────────────────────────────────────
    // push 1 (byte_flag — manual cast)
    sc.extend_from_slice(&[0x6A, 0x01]);
    let _ = SPELL_BOOK_BYTE_FLAG;
    // push packed
    sc.push(0x68);
    sc.extend_from_slice(&packed_skill_id.to_le_bytes());
    // mov ecx, [SPELL_BOOK_PTR]
    sc.extend_from_slice(&[0x8B, 0x0D]);
    sc.extend_from_slice(&address::SPELL_BOOK_PTR.to_le_bytes());
    // mov eax, SPELL_BOOK_CAST
    sc.push(0xB8);
    sc.extend_from_slice(&address::SPELL_BOOK_CAST.to_le_bytes());
    // call eax (thiscall, callee ret 8 自清)
    sc.extend_from_slice(&[0xFF, 0xD0]);
    // pop eax; mov [TARGET], eax (還原)
    sc.push(0x58);
    sc.push(0xA3);
    sc.extend_from_slice(&TARGET_ID_ADDR.to_le_bytes());
    // popad; ret
    sc.push(0x61);
    sc.push(0xC3);
    sc
}

/// 「cccd」格式字串位址(executable .rdata,跨 session 不變)— 0x73C31F push 引用點。
/// SendPacketData 用此格式組 7-byte C_SKILL packet:
///   [opcode:c][skill_high:c][skill_low:c][target:d]
const FMT_CCCD_ADDR: u32 = 0x008EF028;

/// C_SKILL opcode(2026-05-01 capture 確認 = 0x06)。
const C_SKILL_OPCODE: u8 = 0x06;

/// `ForceSelfPacket` 專用 shellcode — bypass spell_book_cast,直接組 C_SKILL packet 送
/// SendPacketData,target=self_char_id。
///
/// 對應 0x73C2A0 路徑(玩家手動點自己施放走的真正 packet send),但繞開 spell_book_cast
/// 的 mouse hover target 解析 — 強制 target=self。
///
/// shellcode 對應原始 asm:
/// ```asm
/// 60                       ; pushad
/// FF 35 <SELF_CHAR_ID>     ; push DWORD [SELF_CHAR_ID_ADDR]   ; target = self
/// 68 <skill_low>           ; push imm32
/// 68 <skill_high>          ; push imm32
/// 6A 06                    ; push 6 (opcode)
/// 68 <FMT_CCCD>            ; push "cccd"
/// B8 <SEND_PACKET_DATA>    ; mov eax, SendPacketData
/// FF D0                    ; call eax
/// 83 C4 14                 ; add esp, 0x14 (5 args × 4)
/// 61                       ; popad
/// C3                       ; ret
/// ```
/// 共 32 bytes。
fn build_force_self_packet_shellcode(packed_skill_id: u32) -> Vec<u8> {
    let skill_low: u32 = packed_skill_id & 7;
    let skill_high: u32 = packed_skill_id >> 3;

    let mut sc = Vec::with_capacity(32);
    // pushad
    sc.push(0x60);
    // push DWORD [SELF_CHAR_ID_ADDR]  (FF 35 imm32)
    sc.extend_from_slice(&[0xFF, 0x35]);
    sc.extend_from_slice(&SELF_CHAR_ID_ADDR.to_le_bytes());
    // push skill_low (imm32 — 防呆即使 >127 也能正確 zero-extend)
    sc.push(0x68);
    sc.extend_from_slice(&skill_low.to_le_bytes());
    // push skill_high
    sc.push(0x68);
    sc.extend_from_slice(&skill_high.to_le_bytes());
    // push 6 (opcode, imm8 sign-extend OK 因為 < 0x80)
    sc.extend_from_slice(&[0x6A, C_SKILL_OPCODE]);
    // push "cccd" 字串位址
    sc.push(0x68);
    sc.extend_from_slice(&FMT_CCCD_ADDR.to_le_bytes());
    // mov eax, SEND_PACKET_DATA
    sc.push(0xB8);
    sc.extend_from_slice(&address::SEND_PACKET_DATA.to_le_bytes());
    // call eax
    sc.extend_from_slice(&[0xFF, 0xD0]);
    // add esp, 0x14
    sc.extend_from_slice(&[0x83, 0xC4, 0x14]);
    // popad; ret
    sc.push(0x61);
    sc.push(0xC3);
    sc
}

/// `ForceTargetPacket` 專用 shellcode — 跟 `build_force_self_packet_shellcode` 同結構,
/// 差別只在 target push:用 `68 imm32`(5 bytes)取代 `FF 35 [SELF]`(6 bytes),總長 35 bytes。
///
/// 用於 `/MIA /MIW /MI` 等對物品施法路徑 — caller 從 inventory 解出 `Item.item_param`
/// (= `[item_addr+4]`,inventory namespace 的 obj_id)直接當 target_id 嵌入 packet。
fn build_force_target_packet_shellcode(packed_skill_id: u32, target_id: u32) -> Vec<u8> {
    let skill_low: u32 = packed_skill_id & 7;
    let skill_high: u32 = packed_skill_id >> 3;

    let mut sc = Vec::with_capacity(35);
    // pushad
    sc.push(0x60);
    // push imm32 target_id
    sc.push(0x68);
    sc.extend_from_slice(&target_id.to_le_bytes());
    // push skill_low
    sc.push(0x68);
    sc.extend_from_slice(&skill_low.to_le_bytes());
    // push skill_high
    sc.push(0x68);
    sc.extend_from_slice(&skill_high.to_le_bytes());
    // push 6 (opcode imm8)
    sc.extend_from_slice(&[0x6A, C_SKILL_OPCODE]);
    // push "cccd" 字串位址
    sc.push(0x68);
    sc.extend_from_slice(&FMT_CCCD_ADDR.to_le_bytes());
    // mov eax, SEND_PACKET_DATA
    sc.push(0xB8);
    sc.extend_from_slice(&address::SEND_PACKET_DATA.to_le_bytes());
    // call eax
    sc.extend_from_slice(&[0xFF, 0xD0]);
    // add esp, 0x14
    sc.extend_from_slice(&[0x83, 0xC4, 0x14]);
    // popad; ret
    sc.push(0x61);
    sc.push(0xC3);
    sc
}

/// II packet opcode — 對既有物品施放卷軸/工具的統一 opcode
/// (2026-05-02 spy capture 確認 = 0xA4)。
const II_OPCODE: u8 = 0xA4;

/// `execute_delete` 專用 shellcode —
/// `SendPacketData("cdd", C_DELETE_ITEM, item_obj_id, count)`。
///
/// fmt 3 欄位:c(opcode) + d(item.item_param) + d(quantity = `[entry+0xA0]`)。
/// 「玩家把道具拖進垃圾桶 → ENTER 全刪」的真實封包(2026-05-02 Frida capture
/// #51 cdd 0x8A [obj_id, 500] 確認)。
///
/// quantity 必須是「整疊當下數量」 — 送 0 server 視為刪 0 個 = no-op。
/// caller 應從 `Item.count`(`inventory.rs`,讀 `entry+0xA0`)取值。
/// 非堆疊物 count = 0 或 1 都 OK(non-stack server 處理一致)。
///
/// 跳過遊戲內建的「請輸入數量」對話框路徑(opcode 0x88 fmt='ccs'),
/// 直接送終態 packet — server 對 0x8A 不要求先 open-dialog,任何道具都吃。
///
/// shellcode asm(cdecl 從右到左 push,4 args):
/// ```asm
///    60                       ; pushad
///    68 <count>               ; push count (arg4: quantity)
///    68 <item_obj_id>         ; push item.item_param (arg3)
///    68 <opcode>              ; push opcode (arg2)
///    E8 00 00 00 00           ; call $+5
///    5E                       ; pop esi
///    83 C6 <fmt_disp>         ; add esi, disp8 (esi → "cdd\0")
///    56                       ; push esi  (arg1 = fmt ptr)
///    B8 <SEND_PACKET_DATA>    ; mov eax, SendPacketData
///    FF D0                    ; call eax
///    83 C4 10                 ; add esp, 0x10  (4 args × 4 cdecl cleanup)
///    61                       ; popad
///    C3                       ; ret
/// fmt:  63 64 64 00            ; "cdd\0"
/// ```
/// 共 42 bytes。
fn build_delete_packet_shellcode(item_obj_id: u32, count: u32) -> Vec<u8> {
    let opcode = crate::aux::address::C_DELETE_ITEM
        .expect("C_DELETE_ITEM 未設(看 address.rs,需先跑 Frida RE)");
    let mut sc = Vec::with_capacity(42);

    // [0] pushad
    sc.push(0x60);

    // [1..6] push count (arg4: quantity)
    sc.push(0x68);
    sc.extend_from_slice(&count.to_le_bytes());

    // [6..11] push obj_id (arg3: item.item_param)
    sc.push(0x68);
    sc.extend_from_slice(&item_obj_id.to_le_bytes());

    // [11..16] push opcode (arg2)
    sc.push(0x68);
    sc.extend_from_slice(&(opcode as u32).to_le_bytes());

    // [16..21] call $+5
    sc.extend_from_slice(&[0xE8, 0x00, 0x00, 0x00, 0x00]);

    // [21] pop esi (esi = sc + 21)
    sc.push(0x5E);

    // [22..25] add esi, disp8 (patch 後)
    let add_esi_pos = sc.len();
    sc.extend_from_slice(&[0x83, 0xC6, 0x00]);

    // [25] push esi (arg1 = fmt ptr)
    sc.push(0x56);

    // [26..31] mov eax, SEND_PACKET_DATA
    sc.push(0xB8);
    sc.extend_from_slice(&address::SEND_PACKET_DATA.to_le_bytes());

    // [31..33] call eax
    sc.extend_from_slice(&[0xFF, 0xD0]);

    // [33..36] add esp, 0x10 (4 args × 4 cdecl cleanup)
    sc.extend_from_slice(&[0x83, 0xC4, 0x10]);

    // [36] popad
    sc.push(0x61);

    // [37] ret
    sc.push(0xC3);

    // [38..42] "cdd\0"
    let fmt_pos = sc.len();
    sc.extend_from_slice(b"cdd\0");

    // patch add esi, disp8 — pop esi 後 esi = sc+21,我們要 esi = sc+fmt_pos
    let disp = (fmt_pos as i32) - 21;
    debug_assert!((0..=127).contains(&disp));
    sc[add_esi_pos + 2] = disp as u8;

    sc
}

/// `execute_use_on_wielded` 專用 shellcode — 直接組 II packet 送 SendPacketData。
///
/// **arg 順序**(2026-05-02 spy #14 真實遊戲手動磨刀 capture 驗證):
/// `SendPacketData("cdd", 0xA4, whetstone_param, weapon_param)` —
/// 第一個 `d` = 磨刀石(來源 / source),第二個 `d` = 揮舞武器(目標 / target)。
///
/// 第二版踩過的坑:第一次以為「先寫的 = target」,結果 spy #18 抓到我們送
/// `[0xA4][weapon][whetstone]`,server 把 weapon 當 source 直接「取消裝備武器」。
/// 實際 spy #14 真實封包是 `[0xA4][whetstone][weapon]`,
/// 表示 SendPacketData 的 arg3 = source、arg4 = target。
///
/// shellcode 對應 asm:
/// ```asm
///    60                       ; pushad
///    68 <weapon_param>        ; push imm32                   ; arg4 = target (push 最早 = 最深)
///    68 <whetstone_param>     ; push imm32                   ; arg3 = source
///    68 A4 00 00 00           ; push imm32 0xA4              ; arg2 = opcode
///    E8 00 00 00 00           ; call $+5 (push next IP)
///    5E                       ; pop esi                      ; esi = sc + 21
///    83 C6 11                 ; add esi, 17                  ; esi → "cdd\0" at sc + 38
///    56                       ; push esi                     ; arg1 = fmt
///    B8 <SEND_PACKET_DATA>    ; mov eax, SendPacketData
///    FF D0                    ; call eax
///    83 C4 10                 ; add esp, 0x10                ; cdecl cleanup
///    61                       ; popad
///    C3                       ; ret
///    63 64 64 00              ; "cdd\0"  (fmt string,IP-relative)
/// ```
/// 共 42 bytes(SHELLCODE_SIZE=64 容得下)。
fn build_whetstone_packet_shellcode(whetstone_item_param: u32, weapon_item_param: u32) -> Vec<u8> {
    let mut sc = Vec::with_capacity(42);

    // [0] pushad
    sc.push(0x60);

    // [1..6] push imm32 weapon_item_param  (arg4 = target — push 最早,在 stack 最深)
    sc.push(0x68);
    sc.extend_from_slice(&weapon_item_param.to_le_bytes());

    // [6..11] push imm32 whetstone_item_param  (arg3 = source)
    sc.push(0x68);
    sc.extend_from_slice(&whetstone_item_param.to_le_bytes());

    // [11..16] push imm32 0xA4  (arg2: opcode II)
    sc.push(0x68);
    sc.extend_from_slice(&(II_OPCODE as u32).to_le_bytes());

    // [16..21] call $+5  (push next IP = byte 21)
    sc.extend_from_slice(&[0xE8, 0x00, 0x00, 0x00, 0x00]);

    // [21] pop esi  (esi = sc_addr + 21)
    sc.push(0x5E);

    // [22..25] add esi, disp8  (disp = fmt_pos - 21,patch 在最後)
    let add_esi_pos = sc.len();
    sc.extend_from_slice(&[0x83, 0xC6, 0x00]);

    // [25] push esi  (arg1: fmt string addr)
    sc.push(0x56);

    // [26..31] mov eax, SEND_PACKET_DATA
    sc.push(0xB8);
    sc.extend_from_slice(&address::SEND_PACKET_DATA.to_le_bytes());

    // [31..33] call eax
    sc.extend_from_slice(&[0xFF, 0xD0]);

    // [33..36] add esp, 0x10  (4 args × 4 bytes,cdecl cleanup)
    sc.extend_from_slice(&[0x83, 0xC4, 0x10]);

    // [36] popad
    sc.push(0x61);

    // [37] ret
    sc.push(0xC3);

    // [38..42] "cdd\0"
    let fmt_pos = sc.len();
    sc.extend_from_slice(b"cdd\0");

    // pop esi 後 esi = sc_addr + 21 — 我們要 esi 指向 fmt_pos
    let add_esi_disp = (fmt_pos as i32) - 21;
    debug_assert!((0..=127).contains(&add_esi_disp));
    sc[add_esi_pos + 2] = add_esi_disp as u8;

    sc
}

/// 變形卷軸 IP packet shellcode — `SendPacketData("cds", 0xA4, scroll_param, option_str_ptr)`。
///
/// 對齊 2026-05-02 spy #139 capture(format `"cds"`,opcode 0xA4,arg3 = 卷軸 item_param,
/// arg4 = 指向 ASCII 變身選項字串的指標)。
///
/// shellcode 對應 asm:
/// ```asm
///    60                       ; pushad
///    E8 00 00 00 00           ; call $+5  (push next IP = byte 6)
///    5E                       ; pop esi   (esi = sc + 6)
///    8D 86 <option_offset>    ; lea eax, [esi + option_disp32]
///    50                       ; push eax  ; arg4 = option_string ptr
///    68 <scroll_param>        ; push imm32 ; arg3 = scroll.item_param
///    68 A4 00 00 00           ; push imm32 0xA4 ; arg2 = opcode
///    8D 86 <fmt_offset>       ; lea eax, [esi + fmt_disp32]
///    50                       ; push eax  ; arg1 = fmt
///    B8 <SEND_PACKET_DATA>    ; mov eax, SendPacketData
///    FF D0                    ; call eax
///    83 C4 10                 ; add esp, 0x10  (cdecl cleanup)
///    61                       ; popad
///    C3                       ; ret
/// fmt_pos:    63 64 73 00     ; "cds\0"
/// option_pos: <option_string> 00  ; ASCII + null
/// ```
///
/// 共 43B prefix + 4B fmt + (option_string + null) bytes。
fn build_transform_packet_shellcode(scroll_item_param: u32, option_string: &str) -> Vec<u8> {
    let mut sc = Vec::with_capacity(64);

    // [0] pushad
    sc.push(0x60);

    // [1..6] call $+5
    sc.extend_from_slice(&[0xE8, 0x00, 0x00, 0x00, 0x00]);

    // [6] pop esi (esi = sc_addr + 6)
    sc.push(0x5E);

    // [7..13] lea eax, [esi + option_offset]  (8D 86 disp32)
    sc.extend_from_slice(&[0x8D, 0x86]);
    let option_lea_pos = sc.len();
    sc.extend_from_slice(&[0u8; 4]);

    // [13] push eax  (arg4 = option_string ptr)
    sc.push(0x50);

    // [14..19] push imm32 scroll_item_param  (arg3)
    sc.push(0x68);
    sc.extend_from_slice(&scroll_item_param.to_le_bytes());

    // [19..24] push imm32 0xA4  (arg2: opcode)
    sc.push(0x68);
    sc.extend_from_slice(&(II_OPCODE as u32).to_le_bytes());

    // [24..30] lea eax, [esi + fmt_offset]
    sc.extend_from_slice(&[0x8D, 0x86]);
    let fmt_lea_pos = sc.len();
    sc.extend_from_slice(&[0u8; 4]);

    // [30] push eax  (arg1 = fmt ptr)
    sc.push(0x50);

    // [31..36] mov eax, SEND_PACKET_DATA
    sc.push(0xB8);
    sc.extend_from_slice(&address::SEND_PACKET_DATA.to_le_bytes());

    // [36..38] call eax
    sc.extend_from_slice(&[0xFF, 0xD0]);

    // [38..41] add esp, 0x10
    sc.extend_from_slice(&[0x83, 0xC4, 0x10]);

    // [41] popad
    sc.push(0x61);

    // [42] ret
    sc.push(0xC3);

    // [43..47] "cds\0"
    let fmt_pos = sc.len();
    sc.extend_from_slice(b"cds\0");

    // [47..] option_string + '\0'
    let option_pos = sc.len();
    sc.extend_from_slice(option_string.as_bytes());
    sc.push(0);

    // ── Patch lea displacements ──
    // esi = sc_addr + 6 (right after pop esi); we want eax = sc_addr + target_pos
    let fmt_disp = (fmt_pos as i32) - 6;
    let option_disp = (option_pos as i32) - 6;
    sc[fmt_lea_pos..fmt_lea_pos + 4].copy_from_slice(&fmt_disp.to_le_bytes());
    sc[option_lea_pos..option_lea_pos + 4].copy_from_slice(&option_disp.to_le_bytes());

    sc
}

/// Chat packet shellcode — `SendPacketData("ccs", C_CHAT_OPCODE, channel, msg_ptr)`。
///
/// 對齊 [`build_transform_packet_shellcode`] 的 IP-relative pattern,差別只在 fmt 字串
/// (`"ccs\0"` vs `"cds\0"`)、opcode(`0x88` vs `0xA4`)、第二參(channel `c` vs item_param `d`)。
/// 4 個 args 在 cdecl 下 stack slot 都 4 bytes,所以 push 寫法不變。
///
/// `message` 為 UTF-8 String,函式內會用 `encoding_rs::BIG5` 編碼後內嵌進 shellcode。
/// 字串以 IP-relative `lea` 取得 runtime 位址,無外部記憶體依賴。
///
/// shellcode 對應 asm:
/// ```asm
///    60                       ; pushad
///    E8 00 00 00 00           ; call $+5  (push next IP = byte 6)
///    5E                       ; pop esi   (esi = sc + 6)
///    8D 86 <msg_disp>         ; lea eax, [esi + msg_disp32]
///    50                       ; push eax  ; arg4 = msg ptr (s)
///    68 <channel u32>         ; push imm32 ; arg3 = channel (c, padded)
///    68 <0x88 u32>            ; push imm32 ; arg2 = opcode (c, padded)
///    8D 86 <fmt_disp>         ; lea eax, [esi + fmt_disp32]
///    50                       ; push eax  ; arg1 = fmt
///    B8 <SEND_PACKET_DATA>    ; mov eax, SendPacketData
///    FF D0                    ; call eax
///    83 C4 10                 ; add esp, 0x10  (cdecl cleanup, 4 args × 4)
///    61                       ; popad
///    C3                       ; ret
/// fmt_pos:    63 63 73 00     ; "ccs\0"
/// msg_pos:    <Big5 bytes>... 00
/// ```
///
/// 共 43B prefix + 4B fmt + (Big5 message + null) bytes。
fn build_chat_packet_shellcode(channel: u8, message: &str) -> Vec<u8> {
    use crate::aux::address::C_CHAT_OPCODE;

    let mut sc = Vec::with_capacity(64);

    // [0] pushad
    sc.push(0x60);

    // [1..6] call $+5
    sc.extend_from_slice(&[0xE8, 0x00, 0x00, 0x00, 0x00]);

    // [6] pop esi (esi = sc_addr + 6)
    sc.push(0x5E);

    // [7..13] lea eax, [esi + msg_offset]  (8D 86 disp32)
    sc.extend_from_slice(&[0x8D, 0x86]);
    let msg_lea_pos = sc.len();
    sc.extend_from_slice(&[0u8; 4]);

    // [13] push eax  (arg4 = msg ptr)
    sc.push(0x50);

    // [14..19] push imm32 channel  (arg3, c 但 cdecl 4-byte slot)
    sc.push(0x68);
    sc.extend_from_slice(&(channel as u32).to_le_bytes());

    // [19..24] push imm32 opcode 0x88  (arg2)
    sc.push(0x68);
    sc.extend_from_slice(&(C_CHAT_OPCODE as u32).to_le_bytes());

    // [24..30] lea eax, [esi + fmt_offset]
    sc.extend_from_slice(&[0x8D, 0x86]);
    let fmt_lea_pos = sc.len();
    sc.extend_from_slice(&[0u8; 4]);

    // [30] push eax  (arg1 = fmt ptr)
    sc.push(0x50);

    // [31..36] mov eax, SEND_PACKET_DATA
    sc.push(0xB8);
    sc.extend_from_slice(&address::SEND_PACKET_DATA.to_le_bytes());

    // [36..38] call eax
    sc.extend_from_slice(&[0xFF, 0xD0]);

    // [38..41] add esp, 0x10
    sc.extend_from_slice(&[0x83, 0xC4, 0x10]);

    // [41] popad
    sc.push(0x61);

    // [42] ret
    sc.push(0xC3);

    // [43..47] "ccs\0"
    let fmt_pos = sc.len();
    sc.extend_from_slice(b"ccs\0");

    // [47..] Big5(message) + '\0'
    let msg_pos = sc.len();
    let (big5, _, _) = encoding_rs::BIG5.encode(message);
    sc.extend_from_slice(&big5);
    sc.push(0);

    // ── Patch lea displacements ──
    // esi = sc_addr + 6 (right after pop esi); 我們要 eax = sc_addr + target_pos
    let fmt_disp = (fmt_pos as i32) - 6;
    let msg_disp = (msg_pos as i32) - 6;
    sc[fmt_lea_pos..fmt_lea_pos + 4].copy_from_slice(&fmt_disp.to_le_bytes());
    sc[msg_lea_pos..msg_lea_pos + 4].copy_from_slice(&msg_disp.to_le_bytes());

    sc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shellcode_layout() {
        // pushad(1) + push imm32(5) + mov eax,imm32(5) + call eax(2) + add esp,4(3) + popad(1) + ret(1) = 18
        let sc = build_shellcode(0x004B3EE0, 0x12345678);
        assert_eq!(sc.len(), 18, "shellcode 必須剛好 18 bytes");
        assert_eq!(sc[0], 0x60); // pushad
        assert_eq!(sc[1], 0x68); // push imm32
        assert_eq!(&sc[2..6], &0x12345678u32.to_le_bytes());
        assert_eq!(sc[6], 0xB8); // mov eax, imm32
        assert_eq!(&sc[7..11], &0x004B3EE0u32.to_le_bytes());
        assert_eq!(&sc[11..13], &[0xFF, 0xD0]); // call eax
        assert_eq!(&sc[13..16], &[0x83, 0xC4, 0x04]); // add esp, 4
        assert_eq!(sc[16], 0x61); // popad
        assert_eq!(sc[17], 0xC3); // ret
    }

    #[test]
    fn skill_shellcode_self_cast_layout() {
        // /ME: 1+5+1 + 10(setup) + 2+5+6+5+2+1+5+1+1 = 45
        let sc = build_skill_shellcode(42, SkillTargetMode::SelfCast);
        assert_eq!(sc.len(), 45);
        // 確認 setup 段是 mov eax,[SELF]; mov [TARGET],eax
        assert_eq!(sc[7], 0xA1); // mov eax, [imm32]
        assert_eq!(&sc[8..12], &SELF_CHAR_ID_ADDR.to_le_bytes());
        assert_eq!(sc[12], 0xA3); // mov [imm32], eax
        assert_eq!(&sc[13..17], &TARGET_ID_ADDR.to_le_bytes());
    }

    #[test]
    fn skill_shellcode_no_spec_layout() {
        // /M: 1+5+1 + 0(setup 跳過) + 2+5+6+5+2+1+5+1+1 = 35
        let sc = build_skill_shellcode(42, SkillTargetMode::NoSpec);
        assert_eq!(sc.len(), 35);
        // 第 7 byte 應該直接是 push 1 (跳過 setup)
        assert_eq!(&sc[7..9], &[0x6A, 0x01]);
    }

    #[test]
    fn skill_shellcode_explicit_layout() {
        // /M=name: 1+5+1 + 10(setup C7 05 imm32 imm32) + 2+5+6+5+2+1+5+1+1 = 45
        let sc = build_skill_shellcode(42, SkillTargetMode::Explicit(0xCAFEBABE));
        assert_eq!(sc.len(), 45);
        // setup 段:C7 05 <TARGET> <id>
        assert_eq!(&sc[7..9], &[0xC7, 0x05]);
        assert_eq!(&sc[9..13], &TARGET_ID_ADDR.to_le_bytes());
        assert_eq!(&sc[13..17], &0xCAFEBABE_u32.to_le_bytes());
    }

    #[test]
    fn skill_shellcode_common_tail() {
        // SelfCast / NoSpec / Explicit 的 tail(restore target + popad + ret)應該結構一致
        // — 最後 8 bytes:[58 A3 <TARGET 4B> 61 C3]
        // ForceSelfPacket 不走還原邏輯,排除。
        for mode in [
            SkillTargetMode::SelfCast,
            SkillTargetMode::NoSpec,
            SkillTargetMode::Explicit(0x123),
        ] {
            let sc = build_skill_shellcode(42, mode);
            let n = sc.len();
            assert_eq!(sc[n - 1], 0xC3, "ret");
            assert_eq!(sc[n - 2], 0x61, "popad");
            assert_eq!(
                &sc[n - 6..n - 2],
                &TARGET_ID_ADDR.to_le_bytes(),
                "[TARGET] addr"
            );
            assert_eq!(sc[n - 7], 0xA3, "mov [TARGET], eax");
            assert_eq!(sc[n - 8], 0x58, "pop eax");
        }
    }

    /// Whetstone II packet shellcode layout — 42 bytes 含尾端 "cdd\0" fmt string。
    ///
    /// 結構驗證:weapon/whetstone params + opcode + SEND_PACKET_DATA addr 都正確編碼;
    /// add esi disp 指向 fmt 字串;arg 順序對齊 spy capture(weapon 先、whetstone 後)。
    #[test]
    fn whetstone_packet_shellcode_layout() {
        let whetstone_param = 0xDEADBEEF_u32;
        let weapon_param = 0xCAFEBABE_u32;
        let sc = build_whetstone_packet_shellcode(whetstone_param, weapon_param);
        assert_eq!(sc.len(), 42, "whetstone shellcode 必須 42 bytes");

        // [0] pushad
        assert_eq!(sc[0], 0x60);
        // [1..6] push imm32 weapon_param  (arg4 = target,push 最早)
        assert_eq!(sc[1], 0x68);
        assert_eq!(&sc[2..6], &weapon_param.to_le_bytes());
        // [6..11] push imm32 whetstone_param  (arg3 = source)
        assert_eq!(sc[6], 0x68);
        assert_eq!(&sc[7..11], &whetstone_param.to_le_bytes());
        // [11..16] push imm32 0xA4  (arg2: opcode)
        assert_eq!(sc[11], 0x68);
        assert_eq!(&sc[12..16], &0x000000A4u32.to_le_bytes());
        // [16..21] call $+5
        assert_eq!(&sc[16..21], &[0xE8, 0x00, 0x00, 0x00, 0x00]);
        // [21] pop esi
        assert_eq!(sc[21], 0x5E);
        // [22..25] add esi, disp8 — fmt 在 byte 38,esi 起點 byte 21,disp = 17
        assert_eq!(&sc[22..24], &[0x83, 0xC6]);
        assert_eq!(sc[24], 17);
        // [25] push esi
        assert_eq!(sc[25], 0x56);
        // [26..31] mov eax, SEND_PACKET_DATA
        assert_eq!(sc[26], 0xB8);
        assert_eq!(&sc[27..31], &address::SEND_PACKET_DATA.to_le_bytes());
        // [31..33] call eax
        assert_eq!(&sc[31..33], &[0xFF, 0xD0]);
        // [33..36] add esp, 0x10
        assert_eq!(&sc[33..36], &[0x83, 0xC4, 0x10]);
        // [36] popad
        assert_eq!(sc[36], 0x61);
        // [37] ret
        assert_eq!(sc[37], 0xC3);
        // [38..42] "cdd\0"
        assert_eq!(&sc[38..42], b"cdd\0");
    }

    /// 變形卷軸 IP packet shellcode — 43B prefix + 4B fmt + option_string + null。
    #[test]
    fn transform_packet_shellcode_layout() {
        let scroll_param = 0x1DCD6A79_u32;
        let option = "death 80";
        let sc = build_transform_packet_shellcode(scroll_param, option);

        // total = 43(prefix) + 4(cds\0) + 8(death 80) + 1(null) = 56
        assert_eq!(sc.len(), 43 + 4 + option.len() + 1);

        // [0] pushad
        assert_eq!(sc[0], 0x60);
        // [1..6] call $+5
        assert_eq!(&sc[1..6], &[0xE8, 0x00, 0x00, 0x00, 0x00]);
        // [6] pop esi
        assert_eq!(sc[6], 0x5E);
        // [7..9] lea eax, [esi+disp32]
        assert_eq!(&sc[7..9], &[0x8D, 0x86]);
        // option_pos = 47, esi = sc+6, disp = 47-6 = 41
        let option_disp = i32::from_le_bytes([sc[9], sc[10], sc[11], sc[12]]);
        assert_eq!(option_disp, 47 - 6);
        // [13] push eax
        assert_eq!(sc[13], 0x50);
        // [14..19] push imm32 scroll_param
        assert_eq!(sc[14], 0x68);
        assert_eq!(&sc[15..19], &scroll_param.to_le_bytes());
        // [19..24] push imm32 0xA4
        assert_eq!(sc[19], 0x68);
        assert_eq!(&sc[20..24], &0x000000A4u32.to_le_bytes());
        // [24..26] lea eax, [esi+disp32]
        assert_eq!(&sc[24..26], &[0x8D, 0x86]);
        // fmt_pos = 43, disp = 43-6 = 37
        let fmt_disp = i32::from_le_bytes([sc[26], sc[27], sc[28], sc[29]]);
        assert_eq!(fmt_disp, 43 - 6);
        // [30] push eax
        assert_eq!(sc[30], 0x50);
        // [31..36] mov eax, SEND_PACKET_DATA
        assert_eq!(sc[31], 0xB8);
        assert_eq!(&sc[32..36], &address::SEND_PACKET_DATA.to_le_bytes());
        // [36..38] call eax
        assert_eq!(&sc[36..38], &[0xFF, 0xD0]);
        // [38..41] add esp, 0x10
        assert_eq!(&sc[38..41], &[0x83, 0xC4, 0x10]);
        // [41] popad
        assert_eq!(sc[41], 0x61);
        // [42] ret
        assert_eq!(sc[42], 0xC3);
        // [43..47] "cds\0"
        assert_eq!(&sc[43..47], b"cds\0");
        // [47..55] "death 80"
        assert_eq!(&sc[47..47 + option.len()], option.as_bytes());
        // [55] null terminator
        assert_eq!(sc[47 + option.len()], 0);
    }

    #[test]
    fn delete_packet_shellcode_layout() {
        let sc = build_delete_packet_shellcode(0xDEADBEEF, 365);
        // 預期 42 bytes:
        // pushad(1) + push count(5) + push obj_id(5) + push opcode(5)
        // + call $+5(5) + pop esi(1) + add esi disp8(3) + push esi(1)
        // + mov eax(5) + call eax(2) + add esp 0x10(3) + popad(1) + ret(1)
        // + "cdd\0"(4) = 42
        assert_eq!(sc.len(), 42, "shellcode size mismatch");
        assert_eq!(&sc[sc.len() - 4..], b"cdd\0");
        // [1] = 0x68 (push imm32), [2..6] = count LE
        assert_eq!(sc[1], 0x68);
        assert_eq!(&sc[2..6], &365u32.to_le_bytes());
        // [6] = 0x68, [7..11] = obj_id LE
        assert_eq!(sc[6], 0x68);
        assert_eq!(&sc[7..11], &0xDEADBEEFu32.to_le_bytes());
    }

    #[test]
    fn delete_packet_shellcode_zero_count() {
        // 非堆疊物 count=0(symmetric — Frida #13 證實 server 收 0 不會 reject 非堆疊)
        let sc = build_delete_packet_shellcode(0x12345678, 0);
        assert_eq!(sc.len(), 42);
        assert_eq!(&sc[2..6], &0u32.to_le_bytes());
        assert_eq!(&sc[7..11], &0x12345678u32.to_le_bytes());
    }

    /// ForceSelfPacket layout — 對應 0x73C2A0 路徑的 7-byte C_SKILL packet。
    /// shellcode 36 bytes:1(pushad) + 6(push [SELF]) + 5(push low) + 5(push high)
    /// + 2(push 6) + 5(push fmt) + 5(mov eax,SP) + 2(call) + 3(add esp) + 1(popad) + 1(ret)
    #[test]
    fn skill_shellcode_force_self_packet_layout() {
        // 體魄強健術 capture 過 packed=42 → high=5, low=2
        let sc = build_skill_shellcode(42, SkillTargetMode::ForceSelfPacket);
        assert_eq!(sc.len(), 36, "ForceSelfPacket shellcode 必須 36 bytes");

        // pushad
        assert_eq!(sc[0], 0x60);
        // push DWORD [SELF_CHAR_ID_ADDR]: FF 35 imm32
        assert_eq!(&sc[1..3], &[0xFF, 0x35]);
        assert_eq!(&sc[3..7], &SELF_CHAR_ID_ADDR.to_le_bytes());
        // push skill_low (imm32) — 42 & 7 = 2
        assert_eq!(sc[7], 0x68);
        assert_eq!(&sc[8..12], &2u32.to_le_bytes());
        // push skill_high — 42 >> 3 = 5
        assert_eq!(sc[12], 0x68);
        assert_eq!(&sc[13..17], &5u32.to_le_bytes());
        // push 6 (opcode, 2 bytes imm8)
        assert_eq!(&sc[17..19], &[0x6A, 0x06]);
        // push "cccd" addr
        assert_eq!(sc[19], 0x68);
        assert_eq!(&sc[20..24], &FMT_CCCD_ADDR.to_le_bytes());
        // mov eax, SEND_PACKET_DATA
        assert_eq!(sc[24], 0xB8);
        assert_eq!(&sc[25..29], &address::SEND_PACKET_DATA.to_le_bytes());
        // call eax
        assert_eq!(&sc[29..31], &[0xFF, 0xD0]);
        // add esp, 0x14
        assert_eq!(&sc[31..34], &[0x83, 0xC4, 0x14]);
        // popad; ret
        assert_eq!(sc[34], 0x61);
        assert_eq!(sc[35], 0xC3);
    }

    /// ForceTargetPacket layout — 跟 ForceSelfPacket 同結構,target 改成 `68 imm32`(5 bytes)。
    /// 總長 35 bytes(比 ForceSelfPacket 短 1 byte:push imm32 5B vs push DWORD ptr 6B)。
    #[test]
    fn skill_shellcode_force_target_packet_layout() {
        let target_id = 0xCAFEBABE_u32;
        let sc = build_skill_shellcode(42, SkillTargetMode::ForceTargetPacket(target_id));
        assert_eq!(sc.len(), 35, "ForceTargetPacket shellcode 必須 35 bytes");

        // pushad
        assert_eq!(sc[0], 0x60);
        // push imm32 target_id
        assert_eq!(sc[1], 0x68);
        assert_eq!(&sc[2..6], &target_id.to_le_bytes());
        // push skill_low (42 & 7 = 2)
        assert_eq!(sc[6], 0x68);
        assert_eq!(&sc[7..11], &2u32.to_le_bytes());
        // push skill_high (42 >> 3 = 5)
        assert_eq!(sc[11], 0x68);
        assert_eq!(&sc[12..16], &5u32.to_le_bytes());
        // push 6 opcode
        assert_eq!(&sc[16..18], &[0x6A, 0x06]);
        // push "cccd" addr
        assert_eq!(sc[18], 0x68);
        assert_eq!(&sc[19..23], &FMT_CCCD_ADDR.to_le_bytes());
        // mov eax, SEND_PACKET_DATA
        assert_eq!(sc[23], 0xB8);
        assert_eq!(&sc[24..28], &address::SEND_PACKET_DATA.to_le_bytes());
        // call eax
        assert_eq!(&sc[28..30], &[0xFF, 0xD0]);
        // add esp, 0x14
        assert_eq!(&sc[30..33], &[0x83, 0xC4, 0x14]);
        // popad; ret
        assert_eq!(sc[33], 0x61);
        assert_eq!(sc[34], 0xC3);
    }

    #[test]
    fn chat_packet_shellcode_layout() {
        let sc = build_chat_packet_shellcode(0x02, "test msg");
        // 結尾必為 "ccs\0" + Big5(訊息) + null
        let fmt_pos = sc
            .windows(4)
            .rposition(|w| w == b"ccs\0")
            .expect("fmt 'ccs\\0' 缺");
        let msg_start = fmt_pos + 4;
        let msg_end = msg_start + "test msg".len();
        assert_eq!(&sc[msg_start..msg_end], b"test msg");
        assert_eq!(sc[msg_end], 0, "訊息 null terminator 缺");
        // 起頭應該是 pushad(0x60),結尾在 fmt 之前是 ret(0xC3)
        assert_eq!(sc[0], 0x60);
        // ret 在 fmt 前一個 byte
        assert_eq!(sc[fmt_pos - 1], 0xC3);
    }

    #[test]
    fn chat_packet_shellcode_big5_encodes_chinese() {
        // 中文訊息應以 Big5 編碼進入 shellcode(不是 UTF-8)
        let sc = build_chat_packet_shellcode(0x02, "測試");
        // Big5: 測 = 0xB4 0xFA, 試 = 0xB8 0xD5
        let big5 = [0xB4u8, 0xFA, 0xB8, 0xD5];
        assert!(
            sc.windows(big5.len()).any(|w| w == big5),
            "shellcode 內找不到 '測試' Big5 bytes,檢查 encoding_rs 用法"
        );
    }
}
