//! 3.8 輔助功能總目錄 — LHX 視窗、自動補水、自動施法、撿物通知、各類 toggle patch
//!
//! 模組分群:
//!   - 基礎建設:runtime(全域排程器 / settings)、address(位址常數)、packet(封包格式)
//!   - 偵測層:entity_scan / inventory / player_state / exp_tracker
//!   - 動作層:drink_hook(CreateRemoteThread + USE_ITEM)、buff_dispatch、hotkey
//!   - UI 層:lhx_window(主視窗)、show_clock_patch、monster_color_patch、attack_damage_*
//!   - Toggle 層:toggle/(all_day / underwater_pump / 待補位址的開關)
//!   - 通知層:notification/(撿物 toast + EXP 飄字 overlay)
//!
//! 啟動順序由 runtime::AuxScheduler 控管,功能本身大多 fail-soft(位址抓不到就 warn 跳過)。

pub mod address;
pub mod attack_damage_feet_hook;
pub mod attack_damage_hook;
pub mod buff_dispatch;
pub mod chat;
pub mod chat_width;
pub mod entity_scan;
pub mod exp_tracker;
pub mod notification;
pub mod packet;
pub mod player_state;
pub mod runtime;
pub mod toggle;

pub mod class_remap;
pub mod drink_hook;
pub mod hotkey;
pub mod input_sim;
pub mod inventory;
pub mod lhx_window;
pub mod low_cpu_hook;
pub mod menu_inject;
pub mod monster_color_patch;
pub mod poison_hook;
pub mod profile;
pub mod show_clock_patch;
pub mod spell_book;
pub mod spell_db;
pub mod use_item_spy;

pub use runtime::{AuxScheduler, AuxSettings};
