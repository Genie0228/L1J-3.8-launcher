//! Monster name color updater for the 3.8 client.
//!
//! Runtime probing showed the normal world-name renderer reads the world entity
//! color word at `entity+0x30`.  The renderer passes two colors to the client
//! text draw routine: inner glyph color and outer glyph color.  This module keeps
//! monster level colors on the entity and patches only the normal-name render
//! call site so colored monsters are drawn as colored outer text with a white
//! inner glyph.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Memory::{
    VirtualQueryEx, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_EXECUTE_READWRITE, PAGE_READONLY,
    PAGE_READWRITE, PAGE_WRITECOPY,
};

use crate::aux::exp_tracker::{level_from_total_exp, G_TOTAL_EXP};
use crate::logger::log_line;
use crate::memory;

const PLAYER_VFPTR: u32 = 0x008D_C08C;
const LOCAL_PLAYER_PTR_ADDR: u32 = 0x00C2_D2B8;
const SELF_CHAR_ID_ADDR: u32 = 0x00AB_F4B4;
const SELECTED_ENTITY_PTR_ADDR: u32 = 0x00AB_F440;
const PLAYER_STATUS_SINGLETON_ADDR: u32 = 0x009A_8CD0;
const PLAYER_LEVEL_OBFUSCATED_OFFSET: u32 = 0x3FC;
const OBFUSCATED_INDEX_XOR: u32 = 0xC001_7921;

const ENTITY_READ_LEN: usize = 0x90;
const ENTITY_SERVER_ID_OFFSET: u32 = 0x0C;
const ENTITY_KIND_OFFSET: u32 = 0x14;
const ENTITY_COLOR_OFFSET: u32 = 0x30;
const ENTITY_LEVEL_CANDIDATE_OFFSET: u32 = 0x5A;
const ENTITY_SPRITE_OFFSET: u32 = 0x18;
const ENTITY_NAME_PTR_OFFSET: u32 = 0x60;
const ENTITY_MAP_OFFSET: u32 = 0x80;
const ENTITY_KIND_WORLD_MONSTER: u8 = 0x00;
const SPRITE_TYPE_MONSTER: u8 = 10;
const MIN_RENDER_MONSTER_OBJECT_ID: u32 = 0x0100_0000;

const DEFAULT_NAME_COLOR: u16 = 0xFFDF;
const MAX_TRUSTED_LEVEL: u32 = 120;
const SCAN_INTERVAL: Duration = Duration::from_millis(900);
const ENABLE_MONSTER_NAME_RENDER_HOOKS: bool = true;
const ENABLE_SELECTED_NAME_COLOR_HOOKS: bool = true;
const ENABLE_OVERHEAD_TEXT_COLOR_HOOK: bool = false;

const HEAP_SCAN_START: u32 = 0x0100_0000;
const HEAP_SCAN_END: u32 = 0x7FFF_0000;
const MAX_REGION_READ: usize = 0x100_0000;

type TouchedColors = Arc<Mutex<HashMap<u32, u16>>>;
type LevelCache = Arc<Mutex<HashMap<u32, StableMonsterLevel>>>;
type SpriteTypes = Arc<Mutex<RuntimeSpriteTypes>>;

#[derive(Debug, Clone, Default, Eq, PartialEq)]
struct LocalPlayerIdentity {
    ptr: u32,
    target_id: u32,
    self_char_id: u32,
    name: String,
    aliases: Vec<String>,
}

struct InstalledScanner {
    cancel: Arc<AtomicBool>,
    touched: TouchedColors,
    worker: Option<JoinHandle<()>>,
    name_render_hook: Option<RenderHookState>,
    hover_name_patch: HoverNamePatchState,
    overhead_text_color_hook: Option<OverheadTextColorHookState>,
    overhead_text_color_reset_hook: Option<OverheadTextColorResetHookState>,
    selected_name_color_hooks: Option<SelectedNameColorHookState>,
}

static STATE: Mutex<Option<InstalledScanner>> = Mutex::new(None);

const NAME_RENDER_HOOK_ADDR: u32 = 0x004F_2BA0;
const NAME_RENDER_HOOK_LEN: usize = 8;
const NAME_RENDER_FALLTHROUGH_ADDR: u32 = 0x004F_2BA8;
const NAME_RENDER_AFTER_CALL_ADDR: u32 = 0x004F_2BE2;
const NAME_RENDER_ORIGINAL_BYTES: [u8; NAME_RENDER_HOOK_LEN] =
    [0x6A, 0x00, 0x8B, 0x15, 0x38, 0xFB, 0x95, 0x00];
const TEXT_DRAW_FN: u32 = 0x0046_F150;
const BLACK_COLOR_GLOBAL_ADDR: u32 = 0x0095_FB38;
const DRAW_SURFACE_GLOBAL_ADDR: u32 = 0x009A_84E0;
const NAME_RENDER_CODECAVE_SIZE: usize = 0x200;

const HOVER_NAME_COLOR_ADDR: u32 = 0x004E_FD71;
const HOVER_NAME_COLOR_LEN: usize = 7;
const HOVER_NAME_COLOR_ORIGINAL_BYTES: [u8; HOVER_NAME_COLOR_LEN] =
    [0x8B, 0x55, 0x08, 0x0F, 0xB7, 0x42, 0x30];
const HOVER_NAME_COLOR_FORCED_WHITE_BYTES: [u8; HOVER_NAME_COLOR_LEN] =
    [0xB8, 0xDF, 0xFF, 0x00, 0x00, 0x90, 0x90];

const TEXT_DRAW_FN_HOOK_LEN: usize = 5;
const TEXT_DRAW_FN_ORIGINAL_BYTES: [u8; TEXT_DRAW_FN_HOOK_LEN] = [0x55, 0x8B, 0xEC, 0x6A, 0xFF];
const TEXT_DRAW_FN_FALLTHROUGH_ADDR: u32 = 0x0046_F155;

const TEXT_DRAW_COMPACT_FN: u32 = 0x0046_F980;
const TEXT_DRAW_COMPACT_HOOK_LEN: usize = 6;
const TEXT_DRAW_COMPACT_ORIGINAL_BYTES: [u8; TEXT_DRAW_COMPACT_HOOK_LEN] =
    [0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x0C];
const TEXT_DRAW_COMPACT_FALLTHROUGH_ADDR: u32 = 0x0046_F986;

const OVERHEAD_TEXT_COLOR_HOOK_ADDR: u32 = 0x0042_BADA;
const OVERHEAD_TEXT_COLOR_HOOK_LEN: usize = 11;
const OVERHEAD_TEXT_COLOR_FALLTHROUGH_ADDR: u32 =
    OVERHEAD_TEXT_COLOR_HOOK_ADDR + OVERHEAD_TEXT_COLOR_HOOK_LEN as u32;
const OVERHEAD_TEXT_COLOR_ORIGINAL_BYTES: [u8; OVERHEAD_TEXT_COLOR_HOOK_LEN] = [
    0x66, 0x8B, 0x4D, 0x10, // mov cx,[ebp+0x10]
    0x66, 0x89, 0x88, 0x9C, 0x03, 0x00, 0x00, // mov [eax+0x39C],cx
];
const OVERHEAD_TEXT_COLOR_CODECAVE_SIZE: usize = 0x200;

const OVERHEAD_TEXT_COLOR_RESET_HOOK_ADDR: u32 = 0x0042_BBA3;
const OVERHEAD_TEXT_COLOR_RESET_HOOK_LEN: usize = 13;
const OVERHEAD_TEXT_COLOR_RESET_FALLTHROUGH_ADDR: u32 =
    OVERHEAD_TEXT_COLOR_RESET_HOOK_ADDR + OVERHEAD_TEXT_COLOR_RESET_HOOK_LEN as u32;
const OVERHEAD_TEXT_COLOR_RESET_ORIGINAL_BYTES: [u8; OVERHEAD_TEXT_COLOR_RESET_HOOK_LEN] = [
    0x66, 0xA1, 0x94, 0xFB, 0x95, 0x00, // mov ax,[0x95FB94]
    0x66, 0x89, 0x82, 0x9C, 0x03, 0x00, 0x00, // mov [edx+0x39C],ax
];
const OVERHEAD_TEXT_COLOR_RESET_CODECAVE_SIZE: usize = 0x200;

const SELECTED_TEXT_CODECAVE_SIZE: usize = 0x500;

const RENDER_MARKER_TABLE_CAPACITY: usize = 128;
const RENDER_MARKER_TABLE_BYTES: usize = 4 + RENDER_MARKER_TABLE_CAPACITY * 4;

const SELECTED_TEXT_DRAW_RETURNS: &[u32] = &[
    0x004F_2E8C,
    0x004F_307A,
    0x004F_310C,
    0x004F_319E,
    0x004F_3235,
    0x004F_3278,
    0x004F_33D5,
    0x004F_3535,
    0x004F_3650,
    0x004F_3693,
    0x004F_38B4,
    0x004F_39AB,
];

const SELECTED_COMPACT_TEXT_DRAW_RETURNS: &[u32] =
    &[0x004F_3039, 0x004F_30CB, 0x004F_315D, 0x004F_31F4];

struct RenderHookState {
    patch_bytes: [u8; NAME_RENDER_HOOK_LEN],
}

struct HoverNamePatchState {
    patch_bytes: [u8; HOVER_NAME_COLOR_LEN],
}

struct OverheadTextColorHookState {
    patch_bytes: [u8; OVERHEAD_TEXT_COLOR_HOOK_LEN],
}

struct OverheadTextColorResetHookState {
    patch_bytes: [u8; OVERHEAD_TEXT_COLOR_RESET_HOOK_LEN],
}

struct SelectedNameColorHookState {
    text_patch_bytes: [u8; TEXT_DRAW_FN_HOOK_LEN],
    compact_patch_bytes: [u8; TEXT_DRAW_COMPACT_HOOK_LEN],
}

#[derive(Debug, Eq, PartialEq)]
enum HookBytes {
    Original,
    Patched,
    Unexpected,
}

pub fn is_installed() -> bool {
    STATE
        .lock()
        .expect("monster_color STATE poisoned")
        .is_some()
}

pub fn install(h: HANDLE) -> Result<()> {
    let mut guard = STATE.lock().expect("monster_color STATE poisoned");
    if guard.is_some() {
        return Ok(());
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let touched = Arc::new(Mutex::new(HashMap::new()));
    let levels = Arc::new(Mutex::new(HashMap::new()));
    let sprite_types = Arc::new(Mutex::new(RuntimeSpriteTypes::default()));
    let cancel_thread = cancel.clone();
    let touched_thread = touched.clone();
    let levels_thread = levels.clone();
    let sprite_types_thread = sprite_types.clone();
    let h_raw = h.0 as usize;
    let render_marker_table = memory::alloc_exec(h, RENDER_MARKER_TABLE_BYTES)
        .context("[monster_color] alloc render marker table")?;
    write_render_marker_table(h, render_marker_table, &[])?;
    let name_render_hook = if ENABLE_MONSTER_NAME_RENDER_HOOKS {
        Some(install_name_render_hook(h, render_marker_table)?)
    } else {
        restore_legacy_jmp_hook_if_present(
            h,
            NAME_RENDER_HOOK_ADDR,
            &NAME_RENDER_ORIGINAL_BYTES,
            "legacy name render hook",
        )?;
        None
    };
    let hover_name_patch = install_hover_name_patch(h)?;
    let overhead_text_color_hook = if ENABLE_OVERHEAD_TEXT_COLOR_HOOK {
        Some(install_overhead_text_color_hook(h, render_marker_table)?)
    } else {
        restore_legacy_jmp_hook_if_present(
            h,
            OVERHEAD_TEXT_COLOR_HOOK_ADDR,
            &OVERHEAD_TEXT_COLOR_ORIGINAL_BYTES,
            "legacy overhead text color hook",
        )?;
        None
    };
    let overhead_text_color_reset_hook = if ENABLE_OVERHEAD_TEXT_COLOR_HOOK {
        Some(install_overhead_text_color_reset_hook(
            h,
            render_marker_table,
        )?)
    } else {
        restore_legacy_jmp_hook_if_present(
            h,
            OVERHEAD_TEXT_COLOR_RESET_HOOK_ADDR,
            &OVERHEAD_TEXT_COLOR_RESET_ORIGINAL_BYTES,
            "legacy overhead text color reset hook",
        )?;
        None
    };
    restore_legacy_selected_name_color_hooks(h)?;
    let selected_name_color_hooks = if ENABLE_SELECTED_NAME_COLOR_HOOKS {
        Some(install_selected_name_color_hooks(h, render_marker_table)?)
    } else {
        None
    };

    let worker = thread::Builder::new()
        .name("monster-color-scan".to_string())
        .spawn(move || {
            let h = HANDLE(h_raw as *mut _);
            scan_loop(
                h,
                cancel_thread,
                touched_thread,
                levels_thread,
                sprite_types_thread,
                render_marker_table,
            );
        })
        .context("[monster_color] spawn scanner thread")?;

    *guard = Some(InstalledScanner {
        cancel,
        touched,
        worker: Some(worker),
        name_render_hook,
        hover_name_patch,
        overhead_text_color_hook,
        overhead_text_color_reset_hook,
        selected_name_color_hooks,
    });

    log_line!(
        "[monster_color] installed entity scanner + normal name render hook, vfptr=0x{PLAYER_VFPTR:08X}, color=entity+0x{ENTITY_COLOR_OFFSET:02X}"
    );
    Ok(())
}

pub fn uninstall(h: HANDLE) -> Result<()> {
    let state = {
        let mut guard = STATE.lock().expect("monster_color STATE poisoned");
        guard.take()
    };

    let Some(mut state) = state else {
        return Ok(());
    };

    state.cancel.store(true, Ordering::Relaxed);
    if let Some(worker) = state.worker.take() {
        let _ = worker.join();
    }

    let restored = restore_touched_colors(h, &state.touched)?;
    if let Some(selected_name_color_hooks) = &state.selected_name_color_hooks {
        restore_selected_name_color_hooks(h, selected_name_color_hooks)?;
    }
    if let Some(overhead_text_color_hook) = &state.overhead_text_color_hook {
        restore_overhead_text_color_hook(h, overhead_text_color_hook)?;
    }
    if let Some(overhead_text_color_reset_hook) = &state.overhead_text_color_reset_hook {
        restore_overhead_text_color_reset_hook(h, overhead_text_color_reset_hook)?;
    }
    restore_hover_name_patch(h, &state.hover_name_patch)?;
    if let Some(name_render_hook) = &state.name_render_hook {
        restore_name_render_hook(h, name_render_hook)?;
    }
    log_line!("[monster_color] uninstalled entity color scanner, restored={restored}");
    Ok(())
}

fn install_name_render_hook(h: HANDLE, render_marker_table: u32) -> Result<RenderHookState> {
    let current = read_hook_bytes(h, NAME_RENDER_HOOK_ADDR, NAME_RENDER_HOOK_LEN)?;
    match classify_hook_bytes(&current, &NAME_RENDER_ORIGINAL_BYTES, None) {
        HookBytes::Original => {}
        HookBytes::Patched => anyhow::bail!("[monster_color] name render hook already patched"),
        HookBytes::Unexpected => anyhow::bail!(
            "[monster_color] name render hook bytes mismatch @ 0x{NAME_RENDER_HOOK_ADDR:08X}: {:02X?}",
            current
        ),
    }

    let cave = memory::alloc_exec(h, NAME_RENDER_CODECAVE_SIZE)
        .context("[monster_color] alloc name render codecave")?;
    let shellcode = build_name_render_shellcode(cave, render_marker_table);
    if shellcode.len() > NAME_RENDER_CODECAVE_SIZE {
        anyhow::bail!(
            "[monster_color] name render shellcode too large: {} > {}",
            shellcode.len(),
            NAME_RENDER_CODECAVE_SIZE
        );
    }
    memory::write_code(h, cave, &shellcode)
        .context("[monster_color] write name render codecave")?;

    let patch_bytes = build_hook_patch::<NAME_RENDER_HOOK_LEN>(NAME_RENDER_HOOK_ADDR, cave);
    memory::write_code(h, NAME_RENDER_HOOK_ADDR, &patch_bytes)
        .context("[monster_color] patch name render hook site")?;

    log_line!("[monster_color] name render hook @ 0x{NAME_RENDER_HOOK_ADDR:08X} -> 0x{cave:08X}");
    Ok(RenderHookState { patch_bytes })
}

fn install_hover_name_patch(h: HANDLE) -> Result<HoverNamePatchState> {
    let current = read_hook_bytes(h, HOVER_NAME_COLOR_ADDR, HOVER_NAME_COLOR_LEN)?;
    if current == HOVER_NAME_COLOR_FORCED_WHITE_BYTES {
        memory::write_code(h, HOVER_NAME_COLOR_ADDR, &HOVER_NAME_COLOR_ORIGINAL_BYTES)
            .context("[monster_color] restore hover name color reader")?;
        log_line!(
            "[monster_color] hover name color restored to entity+0x{ENTITY_COLOR_OFFSET:02X} @ 0x{HOVER_NAME_COLOR_ADDR:08X}"
        );
    } else if current != HOVER_NAME_COLOR_ORIGINAL_BYTES {
        anyhow::bail!(
            "[monster_color] hover name color bytes mismatch @ 0x{HOVER_NAME_COLOR_ADDR:08X}: {:02X?}",
            current
        );
    }

    Ok(HoverNamePatchState {
        patch_bytes: HOVER_NAME_COLOR_ORIGINAL_BYTES,
    })
}

fn install_overhead_text_color_hook(
    h: HANDLE,
    render_marker_table: u32,
) -> Result<OverheadTextColorHookState> {
    let current = read_hook_bytes(
        h,
        OVERHEAD_TEXT_COLOR_HOOK_ADDR,
        OVERHEAD_TEXT_COLOR_HOOK_LEN,
    )?;
    match classify_hook_bytes(&current, &OVERHEAD_TEXT_COLOR_ORIGINAL_BYTES, None) {
        HookBytes::Original => {}
        HookBytes::Patched => {
            anyhow::bail!("[monster_color] overhead text color hook already patched")
        }
        HookBytes::Unexpected => anyhow::bail!(
            "[monster_color] overhead text color bytes mismatch @ 0x{OVERHEAD_TEXT_COLOR_HOOK_ADDR:08X}: {:02X?}",
            current
        ),
    }

    let cave = memory::alloc_exec(h, OVERHEAD_TEXT_COLOR_CODECAVE_SIZE)
        .context("[monster_color] alloc overhead text color codecave")?;
    let shellcode = build_overhead_text_color_shellcode(cave, render_marker_table);
    if shellcode.len() > OVERHEAD_TEXT_COLOR_CODECAVE_SIZE {
        anyhow::bail!(
            "[monster_color] overhead text color shellcode too large: {} > {}",
            shellcode.len(),
            OVERHEAD_TEXT_COLOR_CODECAVE_SIZE
        );
    }
    memory::write_code(h, cave, &shellcode)
        .context("[monster_color] write overhead text color codecave")?;

    let patch_bytes =
        build_hook_patch::<OVERHEAD_TEXT_COLOR_HOOK_LEN>(OVERHEAD_TEXT_COLOR_HOOK_ADDR, cave);
    memory::write_code(h, OVERHEAD_TEXT_COLOR_HOOK_ADDR, &patch_bytes)
        .context("[monster_color] patch overhead text color hook")?;

    log_line!(
        "[monster_color] overhead text color hook @ 0x{OVERHEAD_TEXT_COLOR_HOOK_ADDR:08X} -> 0x{cave:08X}"
    );
    Ok(OverheadTextColorHookState { patch_bytes })
}

fn install_overhead_text_color_reset_hook(
    h: HANDLE,
    render_marker_table: u32,
) -> Result<OverheadTextColorResetHookState> {
    let current = read_hook_bytes(
        h,
        OVERHEAD_TEXT_COLOR_RESET_HOOK_ADDR,
        OVERHEAD_TEXT_COLOR_RESET_HOOK_LEN,
    )?;
    match classify_hook_bytes(&current, &OVERHEAD_TEXT_COLOR_RESET_ORIGINAL_BYTES, None) {
        HookBytes::Original => {}
        HookBytes::Patched => {
            anyhow::bail!("[monster_color] overhead text color reset hook already patched")
        }
        HookBytes::Unexpected => anyhow::bail!(
            "[monster_color] overhead text color reset bytes mismatch @ 0x{OVERHEAD_TEXT_COLOR_RESET_HOOK_ADDR:08X}: {:02X?}",
            current
        ),
    }

    let cave = memory::alloc_exec(h, OVERHEAD_TEXT_COLOR_RESET_CODECAVE_SIZE)
        .context("[monster_color] alloc overhead text color reset codecave")?;
    let shellcode = build_overhead_text_color_reset_shellcode(cave, render_marker_table);
    if shellcode.len() > OVERHEAD_TEXT_COLOR_RESET_CODECAVE_SIZE {
        anyhow::bail!(
            "[monster_color] overhead text color reset shellcode too large: {} > {}",
            shellcode.len(),
            OVERHEAD_TEXT_COLOR_RESET_CODECAVE_SIZE
        );
    }
    memory::write_code(h, cave, &shellcode)
        .context("[monster_color] write overhead text color reset codecave")?;

    let patch_bytes = build_hook_patch::<OVERHEAD_TEXT_COLOR_RESET_HOOK_LEN>(
        OVERHEAD_TEXT_COLOR_RESET_HOOK_ADDR,
        cave,
    );
    memory::write_code(h, OVERHEAD_TEXT_COLOR_RESET_HOOK_ADDR, &patch_bytes)
        .context("[monster_color] patch overhead text color reset hook")?;

    log_line!(
        "[monster_color] overhead text color reset hook @ 0x{OVERHEAD_TEXT_COLOR_RESET_HOOK_ADDR:08X} -> 0x{cave:08X}"
    );
    Ok(OverheadTextColorResetHookState { patch_bytes })
}

fn restore_legacy_selected_name_color_hooks(h: HANDLE) -> Result<()> {
    restore_legacy_jmp_hook_if_present(
        h,
        TEXT_DRAW_FN,
        &TEXT_DRAW_FN_ORIGINAL_BYTES,
        "legacy text draw color hook",
    )?;
    restore_legacy_jmp_hook_if_present(
        h,
        TEXT_DRAW_COMPACT_FN,
        &TEXT_DRAW_COMPACT_ORIGINAL_BYTES,
        "legacy compact text draw color hook",
    )
}

fn install_selected_name_color_hooks(
    h: HANDLE,
    render_marker_table: u32,
) -> Result<SelectedNameColorHookState> {
    let current = read_hook_bytes(h, TEXT_DRAW_FN, TEXT_DRAW_FN_HOOK_LEN)?;
    match classify_hook_bytes(&current, &TEXT_DRAW_FN_ORIGINAL_BYTES, None) {
        HookBytes::Original => {}
        HookBytes::Patched => anyhow::bail!("[monster_color] text draw color hook already patched"),
        HookBytes::Unexpected => anyhow::bail!(
            "[monster_color] text draw bytes mismatch @ 0x{TEXT_DRAW_FN:08X}: {:02X?}",
            current
        ),
    }

    let current = read_hook_bytes(h, TEXT_DRAW_COMPACT_FN, TEXT_DRAW_COMPACT_HOOK_LEN)?;
    match classify_hook_bytes(&current, &TEXT_DRAW_COMPACT_ORIGINAL_BYTES, None) {
        HookBytes::Original => {}
        HookBytes::Patched => {
            anyhow::bail!("[monster_color] compact text draw color hook already patched")
        }
        HookBytes::Unexpected => anyhow::bail!(
            "[monster_color] compact text draw bytes mismatch @ 0x{TEXT_DRAW_COMPACT_FN:08X}: {:02X?}",
            current
        ),
    }

    let text_cave = memory::alloc_exec(h, SELECTED_TEXT_CODECAVE_SIZE)
        .context("[monster_color] alloc text draw color codecave")?;
    let text_shellcode = build_text_draw_color_fix_shellcode(
        text_cave,
        render_marker_table,
        &TEXT_DRAW_FN_ORIGINAL_BYTES,
        TEXT_DRAW_FN_FALLTHROUGH_ADDR,
        SELECTED_TEXT_DRAW_RETURNS,
    );
    if text_shellcode.len() > SELECTED_TEXT_CODECAVE_SIZE {
        anyhow::bail!(
            "[monster_color] text draw color shellcode too large: {} > {}",
            text_shellcode.len(),
            SELECTED_TEXT_CODECAVE_SIZE
        );
    }
    memory::write_code(h, text_cave, &text_shellcode)
        .context("[monster_color] write text draw color codecave")?;

    let compact_cave = memory::alloc_exec(h, SELECTED_TEXT_CODECAVE_SIZE)
        .context("[monster_color] alloc compact text draw color codecave")?;
    let compact_shellcode = build_text_draw_color_fix_shellcode(
        compact_cave,
        render_marker_table,
        &TEXT_DRAW_COMPACT_ORIGINAL_BYTES,
        TEXT_DRAW_COMPACT_FALLTHROUGH_ADDR,
        SELECTED_COMPACT_TEXT_DRAW_RETURNS,
    );
    if compact_shellcode.len() > SELECTED_TEXT_CODECAVE_SIZE {
        anyhow::bail!(
            "[monster_color] compact text draw color shellcode too large: {} > {}",
            compact_shellcode.len(),
            SELECTED_TEXT_CODECAVE_SIZE
        );
    }
    memory::write_code(h, compact_cave, &compact_shellcode)
        .context("[monster_color] write compact text draw color codecave")?;

    let text_patch_bytes = build_hook_patch::<TEXT_DRAW_FN_HOOK_LEN>(TEXT_DRAW_FN, text_cave);
    memory::write_code(h, TEXT_DRAW_FN, &text_patch_bytes)
        .context("[monster_color] patch text draw color hook")?;

    let compact_patch_bytes =
        build_hook_patch::<TEXT_DRAW_COMPACT_HOOK_LEN>(TEXT_DRAW_COMPACT_FN, compact_cave);
    memory::write_code(h, TEXT_DRAW_COMPACT_FN, &compact_patch_bytes)
        .context("[monster_color] patch compact text draw color hook")?;

    log_line!(
        "[monster_color] selected name color hooks @ 0x{TEXT_DRAW_FN:08X}/0x{TEXT_DRAW_COMPACT_FN:08X}"
    );
    Ok(SelectedNameColorHookState {
        text_patch_bytes,
        compact_patch_bytes,
    })
}

fn restore_name_render_hook(h: HANDLE, state: &RenderHookState) -> Result<()> {
    let current = read_hook_bytes(h, NAME_RENDER_HOOK_ADDR, NAME_RENDER_HOOK_LEN)?;
    match classify_hook_bytes(&current, &NAME_RENDER_ORIGINAL_BYTES, Some(&state.patch_bytes)) {
        HookBytes::Original => Ok(()),
        HookBytes::Patched => memory::write_code(
            h,
            NAME_RENDER_HOOK_ADDR,
            &NAME_RENDER_ORIGINAL_BYTES,
        )
        .context("[monster_color] restore name render hook site"),
        HookBytes::Unexpected => anyhow::bail!(
            "[monster_color] refuse to restore unexpected name render hook bytes @ 0x{NAME_RENDER_HOOK_ADDR:08X}: {:02X?}",
            current
        ),
    }
}

fn restore_overhead_text_color_hook(h: HANDLE, state: &OverheadTextColorHookState) -> Result<()> {
    restore_fixed_hook(
        h,
        OVERHEAD_TEXT_COLOR_HOOK_ADDR,
        &OVERHEAD_TEXT_COLOR_ORIGINAL_BYTES,
        &state.patch_bytes,
        "overhead text color hook",
    )
}

fn restore_overhead_text_color_reset_hook(
    h: HANDLE,
    state: &OverheadTextColorResetHookState,
) -> Result<()> {
    restore_fixed_hook(
        h,
        OVERHEAD_TEXT_COLOR_RESET_HOOK_ADDR,
        &OVERHEAD_TEXT_COLOR_RESET_ORIGINAL_BYTES,
        &state.patch_bytes,
        "overhead text color reset hook",
    )
}

fn restore_selected_name_color_hooks(h: HANDLE, state: &SelectedNameColorHookState) -> Result<()> {
    restore_fixed_hook(
        h,
        TEXT_DRAW_FN,
        &TEXT_DRAW_FN_ORIGINAL_BYTES,
        &state.text_patch_bytes,
        "text draw color hook",
    )?;
    restore_fixed_hook(
        h,
        TEXT_DRAW_COMPACT_FN,
        &TEXT_DRAW_COMPACT_ORIGINAL_BYTES,
        &state.compact_patch_bytes,
        "compact text draw color hook",
    )
}

fn restore_fixed_hook<const N: usize>(
    h: HANDLE,
    addr: u32,
    original: &[u8; N],
    patch: &[u8; N],
    label: &str,
) -> Result<()> {
    let current = read_hook_bytes(h, addr, N)?;
    match classify_hook_bytes(&current, original, Some(patch)) {
        HookBytes::Original => Ok(()),
        HookBytes::Patched => memory::write_code(h, addr, original)
            .with_context(|| format!("[monster_color] restore {label}")),
        HookBytes::Unexpected => anyhow::bail!(
            "[monster_color] refuse to restore unexpected {label} bytes @ 0x{addr:08X}: {:02X?}",
            current
        ),
    }
}

fn restore_legacy_jmp_hook_if_present<const N: usize>(
    h: HANDLE,
    addr: u32,
    original: &[u8; N],
    label: &str,
) -> Result<()> {
    let current = read_hook_bytes(h, addr, N)?;
    if current == original {
        return Ok(());
    }
    if is_legacy_jmp_hook(&current, original) {
        memory::write_code(h, addr, original)
            .with_context(|| format!("[monster_color] restore {label}"))?;
        log_line!("[monster_color] restored {label} @ 0x{addr:08X}");
        return Ok(());
    }
    anyhow::bail!(
        "[monster_color] {label} bytes mismatch @ 0x{addr:08X}: {:02X?}",
        current
    )
}

fn is_legacy_jmp_hook<const N: usize>(current: &[u8], original: &[u8; N]) -> bool {
    current != original && current.first() == Some(&0xE9)
}

fn restore_hover_name_patch(h: HANDLE, state: &HoverNamePatchState) -> Result<()> {
    let current = read_hook_bytes(h, HOVER_NAME_COLOR_ADDR, HOVER_NAME_COLOR_LEN)?;
    if current == HOVER_NAME_COLOR_FORCED_WHITE_BYTES {
        return memory::write_code(h, HOVER_NAME_COLOR_ADDR, &HOVER_NAME_COLOR_ORIGINAL_BYTES)
            .context("[monster_color] restore legacy forced-white hover name color");
    }
    match classify_hook_bytes(
        &current,
        &HOVER_NAME_COLOR_ORIGINAL_BYTES,
        Some(&state.patch_bytes),
    ) {
        HookBytes::Original => Ok(()),
        HookBytes::Patched => memory::write_code(
            h,
            HOVER_NAME_COLOR_ADDR,
            &HOVER_NAME_COLOR_ORIGINAL_BYTES,
        )
        .context("[monster_color] restore hover name color"),
        HookBytes::Unexpected => anyhow::bail!(
            "[monster_color] refuse to restore unexpected hover name color bytes @ 0x{HOVER_NAME_COLOR_ADDR:08X}: {:02X?}",
            current
        ),
    }
}

fn read_hook_bytes(h: HANDLE, addr: u32, len: usize) -> Result<Vec<u8>> {
    memory::read_bytes(h, addr, len)
        .with_context(|| format!("[monster_color] read hook bytes @ 0x{addr:08X}"))
}

fn classify_hook_bytes<const N: usize>(
    bytes: &[u8],
    original_bytes: &[u8; N],
    patch_bytes: Option<&[u8; N]>,
) -> HookBytes {
    if bytes == original_bytes {
        HookBytes::Original
    } else if patch_bytes.is_some_and(|p| bytes == p) {
        HookBytes::Patched
    } else {
        HookBytes::Unexpected
    }
}

fn build_hook_patch<const N: usize>(hook_addr: u32, target_addr: u32) -> [u8; N] {
    debug_assert!(N >= 5);
    let rel = (target_addr as i64 - (hook_addr as i64 + 5)) as i32;
    let mut patch = [0x90u8; N];
    patch[0] = 0xE9;
    patch[1..5].copy_from_slice(&rel.to_le_bytes());
    patch
}

fn scan_loop(
    h: HANDLE,
    cancel: Arc<AtomicBool>,
    touched: TouchedColors,
    levels: LevelCache,
    sprite_types: SpriteTypes,
    render_marker_table: u32,
) {
    let mut tick = 0u32;
    while !cancel.load(Ordering::Relaxed) {
        tick = tick.wrapping_add(1);
        let emit_scan_log = tick <= 3 || tick % 20 == 0;
        refresh_runtime_sprite_types_if_needed(h, &sprite_types, emit_scan_log);
        match refresh_entity_colors(
            h,
            &touched,
            &levels,
            &sprite_types,
            render_marker_table,
            emit_scan_log,
        ) {
            Ok(stats) => {
                if emit_scan_log {
                    log_line!(
                        "[monster_color] scan candidates={} visible={} colored={} missing_level={} player_level={} source={}",
                        stats.candidates,
                        stats.visible,
                        stats.colored,
                        stats.missing_level,
                        stats.player_level,
                        stats.player_level_source.as_str()
                    );
                    for sample in &stats.missing_samples {
                        log_line!(
                        "[monster_color]   missing level sample @0x{:08X} id={} kind=0x{:02X} sprite={} map={} color=0x{:04X} sampled_level_byte={}",
                            sample.addr,
                            sample.server_id,
                            sample.entity_kind,
                            sample.sprite,
                            sample.map,
                            sample.current_color,
                            sample.sampled_level_byte
                        );
                    }
                    for sample in &stats.level_samples {
                        log_line!(
                            "[monster_color][level] @0x{:08X} id={} kind=0x{:02X} sprite={} sprite_type={} map={} name_ptr=0x{:08X} name={:?} raw5A={} runtime_level={} final_level={} player={} chosen_color={} current=0x{:04X}",
                            sample.addr,
                            sample.server_id,
                            sample.entity_kind,
                            sample.sprite,
                            format_optional_u8(sample.sprite_type),
                            sample.map,
                            sample.name_ptr,
                            sample.name,
                            sample.sampled_level_byte,
                            format_optional_u32(sample.runtime_level),
                            format_optional_u32(sample.resolved_level),
                            stats.player_level,
                            format_optional_color(sample.chosen_color),
                            sample.current_color
                        );
                    }
                }
            }
            Err(e) if emit_scan_log => {
                log_line!("[monster_color] scan failed: {e:#}");
            }
            Err(_) => {}
        }

        sleep_cancelable(&cancel, SCAN_INTERVAL);
    }
}

fn sleep_cancelable(cancel: &AtomicBool, duration: Duration) {
    let step = Duration::from_millis(50);
    let mut slept = Duration::ZERO;
    while slept < duration && !cancel.load(Ordering::Relaxed) {
        thread::sleep(step);
        slept += step;
    }
}

#[derive(Default)]
struct ScanStats {
    candidates: usize,
    visible: usize,
    colored: usize,
    missing_level: usize,
    player_level: u32,
    player_level_source: PlayerLevelSource,
    missing_samples: Vec<MissingLevelSample>,
    level_samples: Vec<MonsterLevelSample>,
}

#[derive(Clone, Copy, Default)]
enum PlayerLevelSource {
    Direct,
    #[default]
    TotalExp,
}

impl PlayerLevelSource {
    fn as_str(self) -> &'static str {
        match self {
            PlayerLevelSource::Direct => "direct",
            PlayerLevelSource::TotalExp => "total_exp",
        }
    }
}

struct MissingLevelSample {
    addr: u32,
    server_id: u32,
    entity_kind: u8,
    sprite: u16,
    map: u32,
    current_color: u16,
    sampled_level_byte: u8,
}

struct MonsterLevelSample {
    addr: u32,
    server_id: u32,
    entity_kind: u8,
    sprite: u16,
    sprite_type: Option<u8>,
    map: u32,
    name_ptr: u32,
    name: String,
    current_color: u16,
    sampled_level_byte: u8,
    runtime_level: Option<u32>,
    resolved_level: Option<u32>,
    chosen_color: Option<u16>,
}

#[derive(Default)]
struct RuntimeSpriteTypes {
    by_sprite: HashMap<u16, u8>,
    source_addr: Option<u32>,
}

#[derive(Default)]
struct ParsedSpriteTypeRecords {
    direct_types: HashMap<u16, u8>,
    aliases: HashMap<u16, u16>,
}

fn refresh_runtime_sprite_types_if_needed(h: HANDLE, sprite_types: &SpriteTypes, emit_log: bool) {
    if !sprite_types
        .lock()
        .expect("monster_color sprite types poisoned")
        .by_sprite
        .is_empty()
    {
        return;
    }

    match load_runtime_sprite_types(h) {
        Ok(Some(loaded)) => {
            let count = loaded.by_sprite.len();
            let source = loaded.source_addr.unwrap_or(0);
            *sprite_types
                .lock()
                .expect("monster_color sprite types poisoned") = loaded;
            log_line!(
                "[monster_color] runtime sprite types loaded: {} records from heap 0x{source:08X}",
                count
            );
        }
        Ok(None) if emit_log => {
            log_line!("[monster_color] runtime sprite types not found yet");
        }
        Ok(None) => {}
        Err(e) if emit_log => {
            log_line!("[monster_color] runtime sprite type scan failed: {e:#}");
        }
        Err(_) => {}
    }
}

fn sprite_type_for(sprite_types: &SpriteTypes, sprite: u16) -> Option<u8> {
    sprite_types
        .lock()
        .expect("monster_color sprite types poisoned")
        .by_sprite
        .get(&sprite)
        .copied()
}

fn load_runtime_sprite_types(h: HANDLE) -> Result<Option<RuntimeSpriteTypes>> {
    let marker = b"102.type(";
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
            addr = addr.checked_add(0x1000).unwrap_or(HEAP_SCAN_END);
            continue;
        }

        let region_base = mbi.BaseAddress as u32;
        let region_size = mbi.RegionSize.min(usize::MAX / 2);
        if mbi.State == MEM_COMMIT
            && matches!(
                mbi.Protect,
                PAGE_READWRITE | PAGE_EXECUTE_READWRITE | PAGE_READONLY | PAGE_WRITECOPY
            )
            && region_size > marker.len()
        {
            let read_size = region_size.min(MAX_REGION_READ);
            if let Ok(buf) = memory::read_bytes(h, region_base, read_size) {
                if contains_bytes(&buf, marker) {
                    let records = parse_runtime_sprite_type_records(&buf);
                    let by_sprite = resolve_runtime_sprite_types(records);
                    if by_sprite.len() >= 100 {
                        return Ok(Some(RuntimeSpriteTypes {
                            by_sprite,
                            source_addr: Some(region_base),
                        }));
                    }
                }
            }
        }

        let next = region_base.wrapping_add(region_size as u32);
        if next <= addr {
            addr = addr.checked_add(0x1000).unwrap_or(HEAP_SCAN_END);
        } else {
            addr = next;
        }
    }

    Ok(None)
}

fn parse_runtime_sprite_type_records(raw: &[u8]) -> ParsedSpriteTypeRecords {
    let text = String::from_utf8_lossy(raw);
    let mut records = ParsedSpriteTypeRecords::default();
    let mut current_sprite: Option<u16> = None;

    for line in text.split(|ch| ch == '\n' || ch == '\0') {
        let line = line.trim_end_matches('\r');
        if let Some((sprite, alias)) = parse_sprite_record_header(line) {
            current_sprite = Some(sprite);
            if let Some(alias) = alias {
                records.aliases.insert(sprite, alias);
            }
        }

        if let (Some(sprite), Some(sprite_type)) = (current_sprite, parse_sprite_type(line)) {
            records.direct_types.insert(sprite, sprite_type);
        }
    }

    records
}

fn parse_sprite_record_header(line: &str) -> Option<(u16, Option<u16>)> {
    let line = line.strip_prefix('#').unwrap_or(line);
    if !line.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
        return None;
    }

    let sprite_end = line
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(line.len());
    let sprite = line[..sprite_end].parse().ok()?;
    let rest = line[sprite_end..].trim_start();
    if rest.is_empty() {
        return Some((sprite, None));
    }

    let token_end = rest
        .bytes()
        .position(|b| b.is_ascii_whitespace())
        .unwrap_or(rest.len());
    let first_token = &rest[..token_end];
    let alias = first_token
        .split_once('=')
        .and_then(|(_, value)| value.parse::<u16>().ok());

    Some((sprite, alias))
}

fn parse_sprite_type(line: &str) -> Option<u8> {
    let tail = line.split_once("102.type(")?.1;
    let end = tail.find(')')?;
    tail[..end].trim().parse::<u8>().ok()
}

fn resolve_runtime_sprite_types(records: ParsedSpriteTypeRecords) -> HashMap<u16, u8> {
    let mut resolved = HashMap::new();
    for &sprite in records.direct_types.keys().chain(records.aliases.keys()) {
        if let Some(sprite_type) =
            resolve_runtime_sprite_type(sprite, &records.direct_types, &records.aliases)
        {
            resolved.insert(sprite, sprite_type);
        }
    }
    resolved
}

fn resolve_runtime_sprite_type(
    sprite: u16,
    direct_types: &HashMap<u16, u8>,
    aliases: &HashMap<u16, u16>,
) -> Option<u8> {
    let mut current = sprite;
    for _ in 0..16 {
        if let Some(sprite_type) = direct_types.get(&current) {
            return Some(*sprite_type);
        }
        current = *aliases.get(&current)?;
    }
    None
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct MonsterLevelResolution {
    runtime_level: Option<u32>,
    resolved_level: Option<u32>,
}

fn resolve_entity_monster_level(
    snapshot: EntitySnapshot,
    sprite_type: Option<u8>,
    known_colored_by_patch: bool,
) -> MonsterLevelResolution {
    let runtime_level = snapshot
        .monster_level
        .filter(|_| can_use_runtime_monster_level(snapshot, sprite_type, known_colored_by_patch));

    if runtime_level.is_none() {
        return MonsterLevelResolution {
            runtime_level: None,
            resolved_level: None,
        };
    }

    MonsterLevelResolution {
        runtime_level,
        resolved_level: runtime_level,
    }
}

fn resolve_entity_monster_level_for_identity(
    local_identity: &LocalPlayerIdentity,
    snapshot: EntitySnapshot,
    entity_names: &[String],
    sprite_type: Option<u8>,
    known_colored_by_patch: bool,
) -> MonsterLevelResolution {
    if is_local_player_entity(local_identity, snapshot, entity_names) {
        return MonsterLevelResolution {
            runtime_level: None,
            resolved_level: None,
        };
    }

    resolve_entity_monster_level(snapshot, sprite_type, known_colored_by_patch)
}

fn is_local_player_entity(
    local_identity: &LocalPlayerIdentity,
    snapshot: EntitySnapshot,
    entity_names: &[String],
) -> bool {
    if local_identity.ptr != 0 && snapshot.addr == local_identity.ptr {
        return true;
    }
    if local_identity.target_id != 0 && snapshot.server_id == local_identity.target_id {
        return true;
    }
    if local_identity.self_char_id != 0 && snapshot.server_id == local_identity.self_char_id {
        return true;
    }
    let local_names = std::iter::once(local_identity.name.as_str())
        .chain(local_identity.aliases.iter().map(String::as_str))
        .filter(|name| !name.trim().is_empty());
    local_names.into_iter().any(|local_name| {
        entity_names
            .iter()
            .filter(|name| !name.trim().is_empty())
            .any(|entity_name| local_name.eq_ignore_ascii_case(entity_name.trim()))
    })
}

fn can_use_runtime_monster_level(
    snapshot: EntitySnapshot,
    sprite_type: Option<u8>,
    known_colored_by_patch: bool,
) -> bool {
    match sprite_type {
        Some(SPRITE_TYPE_MONSTER) => {
            snapshot.entity_kind == ENTITY_KIND_WORLD_MONSTER || known_colored_by_patch
        }
        Some(_) => false,
        None => {
            snapshot.server_id >= MIN_RENDER_MONSTER_OBJECT_ID
                && (snapshot.entity_kind == ENTITY_KIND_WORLD_MONSTER || known_colored_by_patch)
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct StableMonsterLevel {
    level: u32,
    seen: u8,
}

fn refresh_entity_colors(
    h: HANDLE,
    touched: &TouchedColors,
    levels: &LevelCache,
    sprite_types: &SpriteTypes,
    render_marker_table: u32,
    collect_level_samples: bool,
) -> Result<ScanStats> {
    let local_identity = read_local_player_identity(h);
    let (player_level, player_level_source) = read_player_level(h).unwrap_or_else(|| {
        let total_exp = memory::read_u32(h, G_TOTAL_EXP).unwrap_or(0);
        (
            level_from_total_exp(total_exp as u64),
            PlayerLevelSource::TotalExp,
        )
    });
    let entities = enumerate_player_entities(h)?;
    let mut stats = ScanStats {
        candidates: entities.len(),
        player_level,
        player_level_source,
        ..Default::default()
    };
    let mut render_entities = Vec::new();

    for entity_addr in entities {
        let Ok(raw) = memory::read_bytes(h, entity_addr, ENTITY_READ_LEN) else {
            continue;
        };
        let Some(snapshot) = EntitySnapshot::parse(entity_addr, &raw) else {
            continue;
        };
        if !snapshot.is_visible_world_entity() {
            continue;
        }

        stats.visible += 1;

        let entity_names = read_entity_alias_names(h, snapshot.addr);
        let entity_name = entity_names
            .first()
            .cloned()
            .unwrap_or_else(|| read_entity_name(h, snapshot.name_ptr));
        let sprite_type = sprite_type_for(sprite_types, snapshot.sprite);
        let known_colored_by_patch = has_original_color(touched, snapshot.addr);
        if is_local_player_entity(&local_identity, snapshot, &entity_names) {
            continue;
        }
        let level_resolution = resolve_entity_monster_level_for_identity(
            &local_identity,
            snapshot,
            &entity_names,
            sprite_type,
            known_colored_by_patch,
        );
        let _ = resolve_monster_level(levels, snapshot.addr, snapshot.monster_level);
        let chosen_color = level_resolution
            .resolved_level
            .map(|level| select_level_color(level, player_level).rgb565());
        if collect_level_samples && stats.level_samples.len() < 16 {
            stats.level_samples.push(MonsterLevelSample {
                addr: snapshot.addr,
                server_id: snapshot.server_id,
                entity_kind: snapshot.entity_kind,
                sprite: snapshot.sprite,
                sprite_type,
                map: snapshot.map,
                name_ptr: snapshot.name_ptr,
                name: entity_name.clone(),
                current_color: snapshot.current_color,
                sampled_level_byte: snapshot.sampled_level_byte,
                runtime_level: level_resolution.runtime_level,
                resolved_level: level_resolution.resolved_level,
                chosen_color,
            });
        }

        let monster_level = if let Some(monster_level) = level_resolution.resolved_level {
            monster_level
        } else {
            stats.missing_level += 1;
            if stats.missing_samples.len() < 4 {
                stats.missing_samples.push(MissingLevelSample {
                    addr: snapshot.addr,
                    server_id: snapshot.server_id,
                    entity_kind: snapshot.entity_kind,
                    sprite: snapshot.sprite,
                    map: snapshot.map,
                    current_color: snapshot.current_color,
                    sampled_level_byte: snapshot.sampled_level_byte,
                });
            }
            restore_original_color_if_needed(h, touched, snapshot)?;
            continue;
        };

        let Some(color) = patch_color_for_level(monster_level, player_level) else {
            restore_original_color_if_needed(h, touched, snapshot)?;
            continue;
        };

        render_entities.push(snapshot.addr);
        if snapshot.current_color == color {
            continue;
        }

        remember_original_color(touched, snapshot.addr, snapshot.current_color);
        write_entity_color(h, snapshot.addr, color)?;
        stats.colored += 1;
    }

    write_render_marker_table(h, render_marker_table, &render_entities)?;
    Ok(stats)
}

fn read_local_player_identity(h: HANDLE) -> LocalPlayerIdentity {
    let ptr = memory::read_u32(h, LOCAL_PLAYER_PTR_ADDR).unwrap_or(0);
    let target_id = if ptr != 0 {
        memory::read_u32(h, ptr + ENTITY_SERVER_ID_OFFSET).unwrap_or(0)
    } else {
        0
    };
    let self_char_id = memory::read_u32(h, SELF_CHAR_ID_ADDR).unwrap_or(0);
    let name_ptr = if ptr != 0 {
        memory::read_u32(h, ptr + ENTITY_NAME_PTR_OFFSET).unwrap_or(0)
    } else {
        0
    };
    let name = read_entity_name(h, name_ptr);
    let aliases = if ptr != 0 {
        read_entity_alias_names(h, ptr)
    } else {
        Vec::new()
    };

    LocalPlayerIdentity {
        ptr,
        target_id,
        self_char_id,
        name,
        aliases,
    }
}

fn read_entity_alias_names(h: HANDLE, entity_addr: u32) -> Vec<String> {
    let mut names = Vec::new();
    for offset in [0x60u32, 0x64, 0x6C] {
        let name_ptr = memory::read_u32(h, entity_addr + offset).unwrap_or(0);
        let name = read_entity_name(h, name_ptr);
        if !name.trim().is_empty()
            && !names
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(name.trim()))
        {
            names.push(name.trim().to_string());
        }
    }
    names
}

fn read_entity_name(h: HANDLE, name_ptr: u32) -> String {
    if !(0x0010_0000..HEAP_SCAN_END).contains(&name_ptr) {
        return String::new();
    }

    let Ok(raw) = memory::read_bytes(h, name_ptr, 96) else {
        return String::new();
    };
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    crate::legacy_text::decode_zstr(&raw[..end])
}

fn format_optional_u32(value: Option<u32>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "None".to_string())
}

fn format_optional_u8(value: Option<u8>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "None".to_string())
}

fn format_optional_color(value: Option<u16>) -> String {
    value
        .map(|v| format!("0x{v:04X}"))
        .unwrap_or_else(|| "None".to_string())
}

fn read_player_level(h: HANDLE) -> Option<(u32, PlayerLevelSource)> {
    let owner = memory::read_u32(h, PLAYER_STATUS_SINGLETON_ADDR).ok()?;
    if owner == 0 {
        return None;
    }

    let encoded_addr = memory::read_u32(h, owner + PLAYER_LEVEL_OBFUSCATED_OFFSET).ok()?;
    let level = read_obfuscated_u32(h, encoded_addr).and_then(normalize_level)?;
    Some((level, PlayerLevelSource::Direct))
}

fn resolve_monster_level(
    levels: &LevelCache,
    entity_addr: u32,
    sampled_level: Option<u32>,
) -> Option<u32> {
    let mut guard = levels.lock().expect("monster_color levels poisoned");

    let Some(level) = sampled_level else {
        guard.remove(&entity_addr);
        return None;
    };

    let entry = guard
        .entry(entity_addr)
        .or_insert(StableMonsterLevel { level, seen: 0 });
    if entry.level != level {
        *entry = StableMonsterLevel { level, seen: 1 };
        return None;
    }

    entry.seen = entry.seen.saturating_add(1).min(2);
    (entry.seen >= 2).then_some(level)
}

fn read_obfuscated_u32(h: HANDLE, encoded_addr: u32) -> Option<u32> {
    if encoded_addr == 0 {
        return None;
    }

    let encoded_index = memory::read_u32(h, encoded_addr).ok()?;
    let table_addr = memory::read_u32(h, encoded_addr + 4).ok()?;
    let value_xor = memory::read_u32(h, encoded_addr + 8).ok()?;
    let index = decode_obfuscated_index(encoded_index)?;
    let table_value = memory::read_u32(h, table_addr + index * 4).ok()?;
    Some(decode_obfuscated_value(table_value, value_xor))
}

fn decode_obfuscated_index(encoded_index: u32) -> Option<u32> {
    let index = encoded_index ^ OBFUSCATED_INDEX_XOR;
    (index < 0x1000).then_some(index)
}

fn decode_obfuscated_value(table_value: u32, value_xor: u32) -> u32 {
    table_value ^ value_xor
}

fn restore_original_color_if_needed(
    h: HANDLE,
    touched: &TouchedColors,
    snapshot: EntitySnapshot,
) -> Result<()> {
    if let Some(original) = take_original_color(touched, snapshot.addr) {
        if snapshot.current_color != original {
            write_entity_color(h, snapshot.addr, original)?;
        }
        return Ok(());
    }

    Ok(())
}

fn restore_touched_colors(h: HANDLE, touched: &TouchedColors) -> Result<usize> {
    let originals = {
        let mut guard = touched.lock().expect("monster_color touched poisoned");
        std::mem::take(&mut *guard)
    };

    let mut restored = 0;
    for (entity_addr, color) in originals {
        write_entity_color(h, entity_addr, color)
            .with_context(|| format!("[monster_color] restore 0x{entity_addr:08X}"))?;
        restored += 1;
    }
    Ok(restored)
}

fn remember_original_color(touched: &TouchedColors, entity_addr: u32, color: u16) {
    let mut guard = touched.lock().expect("monster_color touched poisoned");
    guard.entry(entity_addr).or_insert(color);
}

fn has_original_color(touched: &TouchedColors, entity_addr: u32) -> bool {
    let guard = touched.lock().expect("monster_color touched poisoned");
    guard.contains_key(&entity_addr)
}

fn take_original_color(touched: &TouchedColors, entity_addr: u32) -> Option<u16> {
    let mut guard = touched.lock().expect("monster_color touched poisoned");
    guard.remove(&entity_addr)
}

fn write_entity_color(h: HANDLE, entity_addr: u32, color: u16) -> Result<()> {
    memory::write_code(h, entity_addr + ENTITY_COLOR_OFFSET, &color.to_le_bytes())
}

fn write_render_marker_table(h: HANDLE, table_addr: u32, entity_addrs: &[u32]) -> Result<()> {
    let count = entity_addrs.len().min(RENDER_MARKER_TABLE_CAPACITY);
    let mut raw = vec![0u8; RENDER_MARKER_TABLE_BYTES];
    raw[..4].copy_from_slice(&(count as u32).to_le_bytes());
    for (idx, &entity_addr) in entity_addrs.iter().take(count).enumerate() {
        let start = 4 + idx * 4;
        raw[start..start + 4].copy_from_slice(&entity_addr.to_le_bytes());
    }
    memory::write_code(h, table_addr, &raw)
        .with_context(|| format!("[monster_color] write render marker table 0x{table_addr:08X}"))
}

fn is_feature_color(color: u16) -> bool {
    [
        MonsterNameColor::DarkRed,
        MonsterNameColor::LightRed,
        MonsterNameColor::Blue,
        MonsterNameColor::Green,
    ]
    .into_iter()
    .any(|c| c.rgb565() == color)
}

fn build_name_render_shellcode(cave_addr: u32, render_marker_table: u32) -> Vec<u8> {
    let mut sc = Vec::new();

    // eax = entity currently being rendered ([ebp-0x238]).
    sc.extend_from_slice(&[0x8B, 0x85]);
    sc.extend_from_slice(&(-0x238i32).to_le_bytes());
    sc.extend_from_slice(&[0x85, 0xC0]); // test eax,eax
    let je_original_null = emit_jcc32(&mut sc, 0x84);

    sc.extend_from_slice(&[0x0F, 0xB7, 0x48, ENTITY_COLOR_OFFSET as u8]); // movzx ecx,word ptr [eax+0x30]
    sc.extend_from_slice(&[0x81, 0xF9]);
    sc.extend_from_slice(&(DEFAULT_NAME_COLOR as u32).to_le_bytes()); // cmp ecx,default white
    let je_original_default_white = emit_jcc32(&mut sc, 0x84);
    sc.extend_from_slice(&[0x81, 0xF9]);
    sc.extend_from_slice(&(MonsterNameColor::White.rgb565() as u32).to_le_bytes());
    let je_original_feature_white = emit_jcc32(&mut sc, 0x84);

    let mut feature_color_matches = Vec::new();
    for color in [
        MonsterNameColor::DarkRed,
        MonsterNameColor::LightRed,
        MonsterNameColor::Blue,
        MonsterNameColor::Green,
    ] {
        sc.extend_from_slice(&[0x81, 0xF9]); // cmp ecx,color
        sc.extend_from_slice(&(color.rgb565() as u32).to_le_bytes());
        feature_color_matches.push(emit_jcc32(&mut sc, 0x84)); // je check object id
    }
    let jmp_original_not_feature = emit_jmp32(&mut sc);

    let check_object_id_offset = sc.len();
    for rel in feature_color_matches {
        patch_rel32(&mut sc, rel, check_object_id_offset);
    }
    sc.extend_from_slice(&[0x8B, 0x50, ENTITY_SERVER_ID_OFFSET as u8]); // mov edx,[eax+0x0C]
    sc.extend_from_slice(&[0x81, 0xFA]);
    sc.extend_from_slice(&MIN_RENDER_MONSTER_OBJECT_ID.to_le_bytes()); // cmp edx,min monster object id
    let jb_original_low_object_id = emit_jcc32(&mut sc, 0x82); // jb original
    let jmp_original_not_marked = emit_marker_table_check_eax(&mut sc, render_marker_table);

    // Monster with a non-white feature color:
    // call TextDraw(surface, name, len, x, y, inner_white, outer_feature_color, 0).
    sc.extend_from_slice(&[0x6A, 0x00]); // push 0
    sc.push(0x51); // push ecx; outer color = entity+0x30
    sc.push(0x68); // push inner white
    sc.extend_from_slice(&(DEFAULT_NAME_COLOR as u32).to_le_bytes());
    sc.extend_from_slice(&[0x8B, 0x95]);
    sc.extend_from_slice(&(-0x25Ci32).to_le_bytes());
    sc.push(0x52); // push y
    sc.extend_from_slice(&[0x8B, 0x85]);
    sc.extend_from_slice(&(-0x260i32).to_le_bytes());
    sc.push(0x50); // push x
    sc.extend_from_slice(&[0x8B, 0x8D]);
    sc.extend_from_slice(&(-0x258i32).to_le_bytes());
    sc.push(0x51); // push len
    sc.extend_from_slice(&[0x8B, 0x95]);
    sc.extend_from_slice(&(-0x238i32).to_le_bytes());
    sc.extend_from_slice(&[0x8B, 0x42, ENTITY_NAME_PTR_OFFSET as u8]);
    sc.push(0x50); // push name ptr
    sc.extend_from_slice(&[0x8B, 0x0D]);
    sc.extend_from_slice(&DRAW_SURFACE_GLOBAL_ADDR.to_le_bytes());
    sc.push(0x51); // push draw surface
    emit_call_abs(&mut sc, cave_addr, TEXT_DRAW_FN);
    sc.extend_from_slice(&[0x83, 0xC4, 0x20]); // add esp,0x20
    emit_jmp_abs(&mut sc, cave_addr, NAME_RENDER_AFTER_CALL_ADDR);

    let original_offset = sc.len();
    patch_rel32(&mut sc, je_original_null, original_offset);
    patch_rel32(&mut sc, je_original_default_white, original_offset);
    patch_rel32(&mut sc, je_original_feature_white, original_offset);
    patch_rel32(&mut sc, jmp_original_not_feature, original_offset);
    patch_rel32(&mut sc, jb_original_low_object_id, original_offset);
    patch_rel32(&mut sc, jmp_original_not_marked, original_offset);

    // Stolen original bytes from 0x004F2BA0, then jump back to 0x004F2BA8.
    sc.extend_from_slice(&[0x6A, 0x00]); // push 0
    sc.extend_from_slice(&[0x8B, 0x15]);
    sc.extend_from_slice(&BLACK_COLOR_GLOBAL_ADDR.to_le_bytes()); // mov edx,[0x95FB38]
    emit_jmp_abs(&mut sc, cave_addr, NAME_RENDER_FALLTHROUGH_ADDR);

    sc
}

fn build_text_draw_color_fix_shellcode(
    cave_addr: u32,
    render_marker_table: u32,
    stolen_bytes: &[u8],
    fallthrough_addr: u32,
    caller_returns: &[u32],
) -> Vec<u8> {
    let mut sc = Vec::new();
    sc.extend_from_slice(stolen_bytes);

    sc.extend_from_slice(&[0x8B, 0x45, 0x04]); // mov eax,[ebp+4] ; caller return address
    let mut caller_matches = Vec::new();
    for &ret in caller_returns {
        sc.extend_from_slice(&[0x3D]); // cmp eax,ret
        sc.extend_from_slice(&ret.to_le_bytes());
        caller_matches.push(emit_jcc32(&mut sc, 0x84)); // je check color
    }
    let jmp_done_no_caller = emit_jmp32(&mut sc);

    let check_color_offset = sc.len();
    for rel in caller_matches {
        patch_rel32(&mut sc, rel, check_color_offset);
    }

    sc.extend_from_slice(&[0x8B, 0x45, 0x1C]); // mov eax,[ebp+0x1C] ; inner color
    let mut feature_matches = Vec::new();
    for color in [
        MonsterNameColor::DarkRed,
        MonsterNameColor::LightRed,
        MonsterNameColor::Blue,
        MonsterNameColor::Green,
    ] {
        sc.extend_from_slice(&[0x3D]); // cmp eax,color
        sc.extend_from_slice(&(color.rgb565() as u32).to_le_bytes());
        feature_matches.push(emit_jcc32(&mut sc, 0x84)); // je check entity marker
    }
    let jmp_done_not_feature = emit_jmp32(&mut sc);

    let check_entity_offset = sc.len();
    for rel in feature_matches {
        patch_rel32(&mut sc, rel, check_entity_offset);
    }

    sc.extend_from_slice(&[0x8B, 0x15]);
    sc.extend_from_slice(&SELECTED_ENTITY_PTR_ADDR.to_le_bytes()); // mov edx,[0xABF440] ; selected entity
    sc.extend_from_slice(&[0x85, 0xD2]); // test edx,edx
    let je_done_no_entity = emit_jcc32(&mut sc, 0x84);
    let jmp_done_not_marked = emit_marker_table_check_edx(&mut sc, render_marker_table);

    sc.extend_from_slice(&[0x89, 0x45, 0x20]); // mov [ebp+0x20],eax ; outer color
    sc.extend_from_slice(&[0xC7, 0x45, 0x1C]);
    sc.extend_from_slice(&(DEFAULT_NAME_COLOR as u32).to_le_bytes()); // inner white

    let done_offset = sc.len();
    patch_rel32(&mut sc, jmp_done_no_caller, done_offset);
    patch_rel32(&mut sc, jmp_done_not_feature, done_offset);
    patch_rel32(&mut sc, je_done_no_entity, done_offset);
    patch_rel32(&mut sc, jmp_done_not_marked, done_offset);
    emit_jmp_abs(&mut sc, cave_addr, fallthrough_addr);

    sc
}

fn build_overhead_text_color_shellcode(cave_addr: u32, render_marker_table: u32) -> Vec<u8> {
    let mut sc = Vec::new();

    sc.push(0x52); // push edx
    sc.extend_from_slice(&[0x8B, 0x55, 0xCC]); // mov edx,[ebp-0x34] ; target entity
    sc.extend_from_slice(&[0x85, 0xD2]); // test edx,edx
    let je_original_color = emit_jcc32(&mut sc, 0x84);
    let jmp_original_not_marked = emit_marker_table_check_edx(&mut sc, render_marker_table);

    sc.extend_from_slice(&[0x66, 0x8B, 0x4A, ENTITY_COLOR_OFFSET as u8]); // mov cx,[edx+0x30]
    let jmp_store = emit_jmp32(&mut sc);

    let original_color_offset = sc.len();
    patch_rel32(&mut sc, je_original_color, original_color_offset);
    patch_rel32(&mut sc, jmp_original_not_marked, original_color_offset);
    sc.extend_from_slice(&[0x66, 0x8B, 0x4D, 0x10]); // mov cx,[ebp+0x10]

    let store_offset = sc.len();
    patch_rel32(&mut sc, jmp_store, store_offset);
    sc.push(0x5A); // pop edx
    sc.extend_from_slice(&[0x66, 0x89, 0x88, 0x9C, 0x03, 0x00, 0x00]); // mov [eax+0x39C],cx
    emit_jmp_abs(&mut sc, cave_addr, OVERHEAD_TEXT_COLOR_FALLTHROUGH_ADDR);

    sc
}

fn build_overhead_text_color_reset_shellcode(cave_addr: u32, render_marker_table: u32) -> Vec<u8> {
    let mut sc = Vec::new();

    sc.extend_from_slice(&[0x66, 0xA1]);
    sc.extend_from_slice(&0x0095_FB94u32.to_le_bytes()); // mov ax,[0x95FB94]
    sc.extend_from_slice(&[0x85, 0xD2]); // test edx,edx
    let je_done_no_text = emit_jcc32(&mut sc, 0x84);
    sc.extend_from_slice(&[0x8B, 0x8A, 0x98, 0x03, 0x00, 0x00]); // mov ecx,[edx+0x398] ; text entity
    sc.extend_from_slice(&[0x85, 0xC9]); // test ecx,ecx
    let je_store_default = emit_jcc32(&mut sc, 0x84);
    let jmp_store_not_marked = emit_marker_table_check_ecx(&mut sc, render_marker_table);
    sc.extend_from_slice(&[0x0F, 0xB7, 0x49, ENTITY_COLOR_OFFSET as u8]); // movzx ecx,[ecx+0x30]

    let mut feature_matches = Vec::new();
    for color in [
        MonsterNameColor::DarkRed,
        MonsterNameColor::LightRed,
        MonsterNameColor::Blue,
        MonsterNameColor::Green,
    ] {
        sc.extend_from_slice(&[0x81, 0xF9]); // cmp ecx,color
        sc.extend_from_slice(&(color.rgb565() as u32).to_le_bytes());
        feature_matches.push(emit_jcc32(&mut sc, 0x84)); // je use entity color
    }
    let jmp_store_default = emit_jmp32(&mut sc);

    let use_entity_color_offset = sc.len();
    for rel in feature_matches {
        patch_rel32(&mut sc, rel, use_entity_color_offset);
    }
    sc.extend_from_slice(&[0x66, 0x89, 0xC8]); // mov ax,cx

    let store_offset = sc.len();
    patch_rel32(&mut sc, je_store_default, store_offset);
    patch_rel32(&mut sc, jmp_store_not_marked, store_offset);
    patch_rel32(&mut sc, jmp_store_default, store_offset);
    sc.extend_from_slice(&[0x66, 0x89, 0x82, 0x9C, 0x03, 0x00, 0x00]); // mov [edx+0x39C],ax

    let done_offset = sc.len();
    patch_rel32(&mut sc, je_done_no_text, done_offset);
    emit_jmp_abs(
        &mut sc,
        cave_addr,
        OVERHEAD_TEXT_COLOR_RESET_FALLTHROUGH_ADDR,
    );

    sc
}

fn emit_marker_table_check_eax(sc: &mut Vec<u8>, marker_table_addr: u32) -> usize {
    emit_marker_table_check(sc, marker_table_addr, 0x06)
}

fn emit_marker_table_check_edx(sc: &mut Vec<u8>, marker_table_addr: u32) -> usize {
    emit_marker_table_check(sc, marker_table_addr, 0x16)
}

fn emit_marker_table_check_ecx(sc: &mut Vec<u8>, marker_table_addr: u32) -> usize {
    emit_marker_table_check(sc, marker_table_addr, 0x0E)
}

fn emit_marker_table_check(
    sc: &mut Vec<u8>,
    marker_table_addr: u32,
    cmp_ptr_esi_reg_modrm: u8,
) -> usize {
    sc.push(0x56); // push esi
    sc.push(0x57); // push edi
    sc.push(0xBE); // mov esi,marker_table_addr
    sc.extend_from_slice(&marker_table_addr.to_le_bytes());
    sc.extend_from_slice(&[0x8B, 0x3E]); // mov edi,[esi] ; count
    sc.extend_from_slice(&[0x83, 0xC6, 0x04]); // add esi,4 ; first entry
    sc.extend_from_slice(&[0x85, 0xFF]); // test edi,edi
    let jz_fail = emit_jcc32(sc, 0x84);

    let loop_offset = sc.len();
    sc.extend_from_slice(&[0x39, cmp_ptr_esi_reg_modrm]); // cmp [esi],reg
    let je_match = emit_jcc32(sc, 0x84);
    sc.extend_from_slice(&[0x83, 0xC6, 0x04]); // add esi,4
    sc.push(0x4F); // dec edi
    let jnz_loop = emit_jcc32(sc, 0x85);

    let fail_offset = sc.len();
    patch_rel32(sc, jz_fail, fail_offset);
    patch_rel32(sc, jnz_loop, loop_offset);
    sc.push(0x5F); // pop edi
    sc.push(0x5E); // pop esi
    let jmp_fail = emit_jmp32(sc);

    let match_offset = sc.len();
    patch_rel32(sc, je_match, match_offset);
    sc.push(0x5F); // pop edi
    sc.push(0x5E); // pop esi
    jmp_fail
}

fn emit_call_abs(sc: &mut Vec<u8>, cave_addr: u32, target_addr: u32) {
    let call_offset = sc.len();
    sc.push(0xE8);
    let rel = (target_addr as i64 - (cave_addr as i64 + call_offset as i64 + 5)) as i32;
    sc.extend_from_slice(&rel.to_le_bytes());
}

fn emit_jmp_abs(sc: &mut Vec<u8>, cave_addr: u32, target_addr: u32) {
    let jmp_offset = sc.len();
    sc.push(0xE9);
    let rel = (target_addr as i64 - (cave_addr as i64 + jmp_offset as i64 + 5)) as i32;
    sc.extend_from_slice(&rel.to_le_bytes());
}

fn emit_jcc32(sc: &mut Vec<u8>, condition: u8) -> usize {
    sc.extend_from_slice(&[0x0F, condition]);
    let rel_offset = sc.len();
    sc.extend_from_slice(&0i32.to_le_bytes());
    rel_offset
}

fn emit_jmp32(sc: &mut Vec<u8>) -> usize {
    sc.push(0xE9);
    let rel_offset = sc.len();
    sc.extend_from_slice(&0i32.to_le_bytes());
    rel_offset
}

fn patch_rel32(sc: &mut [u8], rel_offset: usize, target_offset: usize) {
    let rel = target_offset as i32 - (rel_offset as i32 + 4);
    sc[rel_offset..rel_offset + 4].copy_from_slice(&rel.to_le_bytes());
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct EntitySnapshot {
    addr: u32,
    server_id: u32,
    entity_kind: u8,
    sprite: u16,
    map: u32,
    name_ptr: u32,
    current_color: u16,
    sampled_level_byte: u8,
    monster_level: Option<u32>,
}

impl EntitySnapshot {
    fn parse(addr: u32, raw: &[u8]) -> Option<Self> {
        if raw.len() < ENTITY_READ_LEN {
            return None;
        }
        let vfptr = read_u32_le(raw, 0)?;
        if vfptr != PLAYER_VFPTR {
            return None;
        }

        let server_id = read_u32_le(raw, ENTITY_SERVER_ID_OFFSET as usize)?;
        let entity_kind = *raw.get(ENTITY_KIND_OFFSET as usize)?;
        let sprite = read_u16_le(raw, ENTITY_SPRITE_OFFSET as usize)?;
        let map = read_u32_le(raw, ENTITY_MAP_OFFSET as usize)?;
        let name_ptr = read_u32_le(raw, ENTITY_NAME_PTR_OFFSET as usize)?;
        let current_color = read_u16_le(raw, ENTITY_COLOR_OFFSET as usize)?;
        let sampled_level_byte = *raw.get(ENTITY_LEVEL_CANDIDATE_OFFSET as usize)?;
        let monster_level = normalize_level(sampled_level_byte as u32);

        Some(Self {
            addr,
            server_id,
            entity_kind,
            sprite,
            map,
            name_ptr,
            current_color,
            sampled_level_byte,
            monster_level,
        })
    }

    fn is_visible_world_entity(self) -> bool {
        self.sprite != 0 && self.map != 0 && self.name_ptr != 0
    }

    #[cfg(test)]
    fn is_probable_monster(self) -> bool {
        self.monster_level
            .is_some_and(|_| can_use_runtime_monster_level(self, Some(SPRITE_TYPE_MONSTER), false))
    }
}

fn normalize_level(level: u32) -> Option<u32> {
    (1..=MAX_TRUSTED_LEVEL).contains(&level).then_some(level)
}

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
            addr = addr.checked_add(0x1000).unwrap_or(HEAP_SCAN_END);
            continue;
        }

        let region_base = mbi.BaseAddress as u32;
        let region_size = mbi.RegionSize.min(usize::MAX / 2);
        if mbi.State == MEM_COMMIT
            && matches!(
                mbi.Protect,
                PAGE_READWRITE | PAGE_EXECUTE_READWRITE | PAGE_READONLY | PAGE_WRITECOPY
            )
            && region_size > 0
        {
            let read_size = region_size.min(MAX_REGION_READ);
            if let Ok(buf) = memory::read_bytes(h, region_base, read_size) {
                let mut i = 0;
                while i + 4 <= buf.len() {
                    if buf[i..i + 4] == vfptr_le {
                        let entity_addr = region_base.wrapping_add(i as u32);
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
            addr = addr.checked_add(0x1000).unwrap_or(HEAP_SCAN_END);
        } else {
            addr = next;
        }
    }

    Ok(hits)
}

pub fn select_level_color(monster_level: u32, player_level: u32) -> MonsterNameColor {
    let delta = monster_level.saturating_sub(player_level);
    if delta >= 30 {
        MonsterNameColor::DarkRed
    } else if delta >= 20 {
        MonsterNameColor::LightRed
    } else if delta >= 11 {
        MonsterNameColor::Blue
    } else if delta >= 1 {
        MonsterNameColor::Green
    } else {
        MonsterNameColor::White
    }
}

fn patch_color_for_level(monster_level: u32, player_level: u32) -> Option<u16> {
    let color = select_level_color(monster_level, player_level).rgb565();
    is_feature_color(color).then_some(color)
}

fn read_u16_le(raw: &[u8], offset: usize) -> Option<u16> {
    raw.get(offset..offset + 2)
        .and_then(|b| b.try_into().ok())
        .map(u16::from_le_bytes)
}

fn read_u32_le(raw: &[u8], offset: usize) -> Option<u32> {
    raw.get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MonsterNameColor {
    DarkRed,
    LightRed,
    Blue,
    Green,
    White,
}

impl MonsterNameColor {
    fn rgb565(self) -> u16 {
        match self {
            MonsterNameColor::DarkRed => 0x8800,
            MonsterNameColor::LightRed => 0xF800,
            MonsterNameColor::Blue => 0x001F,
            MonsterNameColor::Green => 0x07E0,
            MonsterNameColor::White => DEFAULT_NAME_COLOR,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entity(level: u8) -> Vec<u8> {
        let mut raw = vec![0u8; ENTITY_READ_LEN];
        raw[0..4].copy_from_slice(&PLAYER_VFPTR.to_le_bytes());
        raw[ENTITY_SERVER_ID_OFFSET as usize..ENTITY_SERVER_ID_OFFSET as usize + 4]
            .copy_from_slice(&200_000_123u32.to_le_bytes());
        raw[ENTITY_KIND_OFFSET as usize] = 0x00;
        raw[ENTITY_SPRITE_OFFSET as usize..ENTITY_SPRITE_OFFSET as usize + 2]
            .copy_from_slice(&1022u16.to_le_bytes());
        raw[ENTITY_COLOR_OFFSET as usize..ENTITY_COLOR_OFFSET as usize + 2]
            .copy_from_slice(&DEFAULT_NAME_COLOR.to_le_bytes());
        raw[ENTITY_LEVEL_CANDIDATE_OFFSET as usize] = level;
        raw[ENTITY_NAME_PTR_OFFSET as usize..ENTITY_NAME_PTR_OFFSET as usize + 4]
            .copy_from_slice(&0x1234_5678u32.to_le_bytes());
        raw[ENTITY_MAP_OFFSET as usize..ENTITY_MAP_OFFSET as usize + 4]
            .copy_from_slice(&4u32.to_le_bytes());
        raw
    }

    #[test]
    fn selects_lhx_monster_color_from_level_delta() {
        assert_eq!(select_level_color(129, 99), MonsterNameColor::DarkRed);
        assert_eq!(select_level_color(128, 99), MonsterNameColor::LightRed);
        assert_eq!(select_level_color(119, 99), MonsterNameColor::LightRed);
        assert_eq!(select_level_color(118, 99), MonsterNameColor::Blue);
        assert_eq!(select_level_color(110, 99), MonsterNameColor::Blue);
        assert_eq!(select_level_color(109, 99), MonsterNameColor::Green);
        assert_eq!(select_level_color(104, 99), MonsterNameColor::Green);
        assert_eq!(select_level_color(100, 99), MonsterNameColor::Green);
        assert_eq!(select_level_color(99, 99), MonsterNameColor::White);
        assert_eq!(select_level_color(8, 99), MonsterNameColor::White);
        assert_eq!(select_level_color(8, 5), MonsterNameColor::Green);
    }

    #[test]
    fn low_level_white_uses_client_default_white() {
        assert_eq!(MonsterNameColor::White.rgb565(), DEFAULT_NAME_COLOR);
        assert!(!is_feature_color(DEFAULT_NAME_COLOR));
    }

    #[test]
    fn low_level_white_band_is_not_written_over_client_color() {
        assert_eq!(select_level_color(8, 99), MonsterNameColor::White);
        assert_eq!(patch_color_for_level(8, 99), None);
    }

    #[test]
    fn non_white_level_bands_are_patch_colors() {
        assert_eq!(
            patch_color_for_level(8, 5),
            Some(MonsterNameColor::Green.rgb565())
        );
        assert_eq!(
            patch_color_for_level(104, 99),
            Some(MonsterNameColor::Green.rgb565())
        );
        assert_eq!(
            patch_color_for_level(110, 99),
            Some(MonsterNameColor::Blue.rgb565())
        );
        assert_eq!(
            patch_color_for_level(119, 99),
            Some(MonsterNameColor::LightRed.rgb565())
        );
        assert_eq!(
            patch_color_for_level(129, 99),
            Some(MonsterNameColor::DarkRed.rgb565())
        );
    }

    #[test]
    fn blue_level_band_uses_unambiguous_blue() {
        assert_eq!(MonsterNameColor::Blue.rgb565(), 0x001F);
    }

    #[test]
    fn accepts_monster_level_after_two_stable_samples() {
        let levels = Arc::new(Mutex::new(HashMap::new()));
        assert_eq!(resolve_monster_level(&levels, 0x1234_0000, Some(65)), None);
        assert_eq!(
            resolve_monster_level(&levels, 0x1234_0000, Some(65)),
            Some(65)
        );
    }

    #[test]
    fn resets_monster_level_when_candidate_changes() {
        let levels = Arc::new(Mutex::new(HashMap::new()));
        assert_eq!(resolve_monster_level(&levels, 0x1234_0000, Some(65)), None);
        assert_eq!(resolve_monster_level(&levels, 0x1234_0000, Some(41)), None);
        assert_eq!(
            resolve_monster_level(&levels, 0x1234_0000, Some(41)),
            Some(41)
        );
        assert_eq!(resolve_monster_level(&levels, 0x1234_0000, None), None);
    }

    #[test]
    fn parses_visible_entity_candidate_level_and_color_field() {
        let mut raw = sample_entity(12);
        raw[0x50] = 0;
        raw[0x54] = 0;
        let entity = EntitySnapshot::parse(0x1234_0000, &raw).unwrap();
        assert!(entity.is_visible_world_entity());
        assert_eq!(entity.sprite, 1022);
        assert_eq!(entity.server_id, 200_000_123);
        assert_eq!(entity.entity_kind, 0x00);
        assert!(entity.is_probable_monster());
        assert_eq!(entity.map, 4);
        assert_eq!(entity.name_ptr, 0x1234_5678);
        assert_eq!(entity.current_color, DEFAULT_NAME_COLOR);
        assert_eq!(entity.sampled_level_byte, 12);
        assert_eq!(entity.monster_level, Some(12));
    }

    #[test]
    fn treats_implausible_candidate_level_as_unknown_for_current_world_entities() {
        let raw = sample_entity(197);
        let entity = EntitySnapshot::parse(0x1234_0000, &raw).unwrap();
        assert_eq!(entity.sampled_level_byte, 197);
        assert_eq!(entity.monster_level, None);
    }

    #[test]
    fn rejects_non_visible_or_non_entity_buffers() {
        let mut raw = sample_entity(10);
        raw[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        assert!(EntitySnapshot::parse(0x1234_0000, &raw).is_none());

        let mut raw = sample_entity(10);
        raw[ENTITY_MAP_OFFSET as usize..ENTITY_MAP_OFFSET as usize + 4]
            .copy_from_slice(&0u32.to_le_bytes());
        let entity = EntitySnapshot::parse(0x1234_0000, &raw).unwrap();
        assert!(!entity.is_visible_world_entity());
    }

    #[test]
    fn normalizes_trusted_level_range() {
        assert_eq!(normalize_level(1), Some(1));
        assert_eq!(normalize_level(120), Some(120));
        assert_eq!(normalize_level(0), None);
        assert_eq!(normalize_level(121), None);
    }

    #[test]
    fn decodes_direct_player_level_obfuscated_slot() {
        assert_eq!(decode_obfuscated_index(OBFUSCATED_INDEX_XOR ^ 2), Some(2));
        assert_eq!(decode_obfuscated_value(0x6F57_5B01, 0x6F57_5B07), 6);
        assert_eq!(normalize_level(6), Some(6));
    }

    #[test]
    fn rejects_implausible_obfuscated_index() {
        assert_eq!(decode_obfuscated_index(OBFUSCATED_INDEX_XOR ^ 0x1000), None);
    }

    #[test]
    fn rejects_visible_town_npcs_and_static_world_objects() {
        let mut raw = sample_entity(8);
        raw[ENTITY_SERVER_ID_OFFSET as usize..ENTITY_SERVER_ID_OFFSET as usize + 4]
            .copy_from_slice(&81_356u32.to_le_bytes());
        raw[ENTITY_KIND_OFFSET as usize] = 0x03;
        raw[ENTITY_SPRITE_OFFSET as usize..ENTITY_SPRITE_OFFSET as usize + 2]
            .copy_from_slice(&2143u16.to_le_bytes());
        let entity = EntitySnapshot::parse(0x1234_0000, &raw).unwrap();
        assert_eq!(
            resolve_entity_monster_level(entity, Some(12), false).resolved_level,
            None
        );

        let mut raw = sample_entity(0);
        raw[ENTITY_SERVER_ID_OFFSET as usize..ENTITY_SERVER_ID_OFFSET as usize + 4]
            .copy_from_slice(&200_030_391u32.to_le_bytes());
        raw[ENTITY_KIND_OFFSET as usize] = ENTITY_KIND_WORLD_MONSTER;
        let entity = EntitySnapshot::parse(0x1234_0000, &raw).unwrap();
        assert_eq!(
            resolve_entity_monster_level(entity, Some(SPRITE_TYPE_MONSTER), false).resolved_level,
            None
        );
    }

    #[test]
    fn does_not_special_case_sprite_ids_when_runtime_type_is_not_monster() {
        let mut raw = sample_entity(1);
        raw[ENTITY_SERVER_ID_OFFSET as usize..ENTITY_SERVER_ID_OFFSET as usize + 4]
            .copy_from_slice(&81_356u32.to_le_bytes());
        raw[ENTITY_KIND_OFFSET as usize] = ENTITY_KIND_WORLD_MONSTER;
        raw[ENTITY_SPRITE_OFFSET as usize..ENTITY_SPRITE_OFFSET as usize + 2]
            .copy_from_slice(&2143u16.to_le_bytes());
        let entity = EntitySnapshot::parse(0x1234_0000, &raw).unwrap();
        assert_eq!(
            resolve_entity_monster_level(entity, Some(12), false).resolved_level,
            None
        );

        assert_eq!(
            resolve_entity_monster_level(entity, None, false).resolved_level,
            None
        );
    }

    #[test]
    fn rejects_monster_sprite_when_entity_kind_is_not_world_monster() {
        let mut raw = sample_entity(10);
        raw[ENTITY_SERVER_ID_OFFSET as usize..ENTITY_SERVER_ID_OFFSET as usize + 4]
            .copy_from_slice(&200_004_962u32.to_le_bytes());
        raw[ENTITY_KIND_OFFSET as usize] = 0x03;
        raw[ENTITY_SPRITE_OFFSET as usize..ENTITY_SPRITE_OFFSET as usize + 2]
            .copy_from_slice(&1022u16.to_le_bytes());
        let entity = EntitySnapshot::parse(0x1234_0000, &raw).unwrap();

        assert_eq!(
            resolve_entity_monster_level(entity, Some(SPRITE_TYPE_MONSTER), false).resolved_level,
            None
        );
    }

    #[test]
    fn keeps_previously_colored_monster_when_runtime_kind_changes_temporarily() {
        let mut raw = sample_entity(10);
        raw[ENTITY_SERVER_ID_OFFSET as usize..ENTITY_SERVER_ID_OFFSET as usize + 4]
            .copy_from_slice(&200_031_075u32.to_le_bytes());
        raw[ENTITY_KIND_OFFSET as usize] = 0x01;
        let entity = EntitySnapshot::parse(0x1234_0000, &raw).unwrap();

        assert_eq!(
            resolve_entity_monster_level(entity, Some(SPRITE_TYPE_MONSTER), true).resolved_level,
            Some(10)
        );
    }

    #[test]
    fn does_not_keep_town_npc_colored_when_kind_is_npc_state() {
        let mut raw = sample_entity(65);
        raw[ENTITY_SERVER_ID_OFFSET as usize..ENTITY_SERVER_ID_OFFSET as usize + 4]
            .copy_from_slice(&200_029_192u32.to_le_bytes());
        raw[ENTITY_KIND_OFFSET as usize] = 0x03;
        raw[ENTITY_SPRITE_OFFSET as usize..ENTITY_SPRITE_OFFSET as usize + 2]
            .copy_from_slice(&335u16.to_le_bytes());
        let entity = EntitySnapshot::parse(0x1234_0000, &raw).unwrap();

        assert_eq!(
            resolve_entity_monster_level(entity, Some(5), true).resolved_level,
            None
        );
    }

    #[test]
    fn rejects_local_player_identity_even_when_sprite_type_is_monster() {
        let mut raw = sample_entity(80);
        raw[ENTITY_SERVER_ID_OFFSET as usize..ENTITY_SERVER_ID_OFFSET as usize + 4]
            .copy_from_slice(&0xBFu32.to_le_bytes());
        let entity = EntitySnapshot::parse(0x2233_0000, &raw).unwrap();
        let identity = LocalPlayerIdentity {
            ptr: 0x1111_0000,
            target_id: 0xBF,
            self_char_id: 0xBF,
            name: "Me".to_string(),
            aliases: vec!["Me".to_string()],
        };

        assert!(is_local_player_entity(
            &identity,
            entity,
            &["Me".to_string()]
        ));
        assert_eq!(
            resolve_entity_monster_level_for_identity(
                &identity,
                entity,
                &["Me".to_string()],
                Some(SPRITE_TYPE_MONSTER),
                false
            )
            .resolved_level,
            None
        );
    }

    #[test]
    fn rejects_local_player_world_avatar_by_name_when_object_id_differs() {
        let mut raw = sample_entity(80);
        raw[ENTITY_SERVER_ID_OFFSET as usize..ENTITY_SERVER_ID_OFFSET as usize + 4]
            .copy_from_slice(&200_123_456u32.to_le_bytes());
        let entity = EntitySnapshot::parse(0x2233_0000, &raw).unwrap();
        let identity = LocalPlayerIdentity {
            ptr: 0x1111_0000,
            target_id: 0xBF,
            self_char_id: 0xBF,
            name: "MyChar".to_string(),
            aliases: vec!["MyChar".to_string()],
        };

        assert!(is_local_player_entity(
            &identity,
            entity,
            &["MyChar".to_string()]
        ));
        assert_eq!(
            resolve_entity_monster_level_for_identity(
                &identity,
                entity,
                &["MyChar".to_string()],
                Some(SPRITE_TYPE_MONSTER),
                false
            )
            .resolved_level,
            None
        );
    }

    #[test]
    fn rejects_local_player_world_avatar_by_alias_name_when_primary_name_differs() {
        let mut raw = sample_entity(80);
        raw[ENTITY_SERVER_ID_OFFSET as usize..ENTITY_SERVER_ID_OFFSET as usize + 4]
            .copy_from_slice(&200_123_456u32.to_le_bytes());
        let entity = EntitySnapshot::parse(0x2233_0000, &raw).unwrap();
        let identity = LocalPlayerIdentity {
            ptr: 0x1111_0000,
            target_id: 0xBF,
            self_char_id: 0xBF,
            name: "LoginName".to_string(),
            aliases: vec!["衣衫衣衫".to_string()],
        };
        let entity_names = vec!["SomeHeapName".to_string(), "衣衫衣衫".to_string()];

        assert!(is_local_player_entity(&identity, entity, &entity_names));
        assert_eq!(
            resolve_entity_monster_level_for_identity(
                &identity,
                entity,
                &entity_names,
                Some(SPRITE_TYPE_MONSTER),
                false
            )
            .resolved_level,
            None
        );
    }

    #[test]
    fn accepts_live_high_object_id_monsters_with_valid_level() {
        let mut raw = sample_entity(8);
        raw[ENTITY_SERVER_ID_OFFSET as usize..ENTITY_SERVER_ID_OFFSET as usize + 4]
            .copy_from_slice(&200_031_075u32.to_le_bytes());
        raw[ENTITY_SPRITE_OFFSET as usize..ENTITY_SPRITE_OFFSET as usize + 2]
            .copy_from_slice(&3864u16.to_le_bytes());
        let entity = EntitySnapshot::parse(0x1234_0000, &raw).unwrap();

        assert_eq!(
            resolve_entity_monster_level(entity, Some(SPRITE_TYPE_MONSTER), false).resolved_level,
            Some(8)
        );
    }

    #[test]
    fn accepts_unknown_sprite_type_world_monster_with_high_object_id_and_valid_level() {
        let mut raw = sample_entity(8);
        raw[ENTITY_SERVER_ID_OFFSET as usize..ENTITY_SERVER_ID_OFFSET as usize + 4]
            .copy_from_slice(&200_031_075u32.to_le_bytes());
        raw[ENTITY_KIND_OFFSET as usize] = ENTITY_KIND_WORLD_MONSTER;
        raw[ENTITY_SPRITE_OFFSET as usize..ENTITY_SPRITE_OFFSET as usize + 2]
            .copy_from_slice(&3865u16.to_le_bytes());
        let entity = EntitySnapshot::parse(0x1234_0000, &raw).unwrap();

        assert_eq!(
            resolve_entity_monster_level(entity, None, false).resolved_level,
            Some(8)
        );
    }

    #[test]
    fn keeps_previously_colored_high_object_id_monster_when_sprite_type_is_temporarily_unknown() {
        let mut raw = sample_entity(10);
        raw[ENTITY_SERVER_ID_OFFSET as usize..ENTITY_SERVER_ID_OFFSET as usize + 4]
            .copy_from_slice(&200_031_075u32.to_le_bytes());
        raw[ENTITY_KIND_OFFSET as usize] = 0x01;
        let entity = EntitySnapshot::parse(0x1234_0000, &raw).unwrap();

        assert_eq!(
            resolve_entity_monster_level(entity, None, true).resolved_level,
            Some(10)
        );
    }

    #[test]
    fn accepts_runtime_level_monsters_without_id_range() {
        let mut raw = sample_entity(8);
        raw[ENTITY_SERVER_ID_OFFSET as usize..ENTITY_SERVER_ID_OFFSET as usize + 4]
            .copy_from_slice(&123_456_789u32.to_le_bytes());
        let entity = EntitySnapshot::parse(0x1234_0000, &raw).unwrap();

        assert_eq!(
            resolve_entity_monster_level(entity, Some(SPRITE_TYPE_MONSTER), false).resolved_level,
            Some(8)
        );
    }
    #[test]
    fn name_render_shellcode_gates_feature_color_by_world_object_id() {
        let sc = build_name_render_shellcode(0x1000_0000, 0x2000_0000);

        assert!(sc
            .windows(3)
            .any(|w| w == [0x8B, 0x50, ENTITY_SERVER_ID_OFFSET as u8]));
        assert!(sc.windows(2).any(|w| w == [0x81, 0xFA]));
    }

    #[test]
    fn name_render_shellcode_gates_feature_color_by_render_marker_table() {
        let sc = build_name_render_shellcode(0x1000_0000, 0x2000_0000);

        assert!(sc.windows(4).any(|w| w == 0x2000_0000u32.to_le_bytes()));
        assert!(sc.windows(2).any(|w| w == [0x39, 0x06])); // cmp [esi],eax
    }

    #[test]
    fn runtime_render_hooks_are_limited_to_normal_world_name_call_site() {
        assert!(ENABLE_MONSTER_NAME_RENDER_HOOKS);
        assert!(ENABLE_SELECTED_NAME_COLOR_HOOKS);
        assert!(!ENABLE_OVERHEAD_TEXT_COLOR_HOOK);
    }

    #[test]
    fn overhead_text_color_shellcode_gates_by_render_marker_table() {
        let sc = build_overhead_text_color_shellcode(0x1000_0000, 0x2000_0000);

        assert!(sc.windows(3).any(|w| w == [0x8B, 0x55, 0xCC])); // mov edx,[ebp-0x34]
        assert!(sc.windows(4).any(|w| w == 0x2000_0000u32.to_le_bytes()));
        assert!(sc.windows(2).any(|w| w == [0x39, 0x16])); // cmp [esi],edx
        assert!(sc
            .windows(4)
            .any(|w| w == [0x66, 0x8B, 0x4A, ENTITY_COLOR_OFFSET as u8]));
        assert!(sc
            .windows(7)
            .any(|w| w == [0x66, 0x89, 0x88, 0x9C, 0x03, 0x00, 0x00]));
    }

    #[test]
    fn overhead_text_color_reset_shellcode_restores_feature_color_from_text_entity() {
        let sc = build_overhead_text_color_reset_shellcode(0x1000_0000, 0x2000_0000);

        assert!(sc.windows(4).any(|w| w == 0x0095_FB94u32.to_le_bytes()));
        assert!(sc
            .windows(6)
            .any(|w| w == [0x8B, 0x8A, 0x98, 0x03, 0x00, 0x00])); // mov ecx,[edx+0x398]
        assert!(sc.windows(4).any(|w| w == 0x2000_0000u32.to_le_bytes()));
        assert!(sc.windows(2).any(|w| w == [0x39, 0x0E])); // cmp [esi],ecx
        assert!(sc
            .windows(4)
            .any(|w| w == [0x0F, 0xB7, 0x49, ENTITY_COLOR_OFFSET as u8])); // movzx ecx,[ecx+0x30]
        assert!(sc
            .windows(7)
            .any(|w| w == [0x66, 0x89, 0x82, 0x9C, 0x03, 0x00, 0x00])); // mov [edx+0x39C],ax
        assert!(sc
            .windows(4)
            .any(|w| w == (MonsterNameColor::Green.rgb565() as u32).to_le_bytes()));
    }

    #[test]
    fn recognizes_legacy_text_draw_jmp_hook_for_restore() {
        let original = [0x55, 0x8B, 0xEC, 0x6A, 0xFF];
        let legacy_jmp = [0xE9, 0x11, 0x22, 0x33, 0x44];

        assert!(is_legacy_jmp_hook(&legacy_jmp, &original));
        assert!(!is_legacy_jmp_hook(&original, &original));
    }

    #[test]
    fn text_draw_shellcode_gates_feature_color_by_render_marker_table() {
        let sc = build_text_draw_color_fix_shellcode(
            0x1000_0000,
            0x2000_0000,
            &TEXT_DRAW_FN_ORIGINAL_BYTES,
            TEXT_DRAW_FN_FALLTHROUGH_ADDR,
            SELECTED_TEXT_DRAW_RETURNS,
        );

        assert!(sc
            .windows(6)
            .any(|w| w == [0x8B, 0x15, 0x40, 0xF4, 0xAB, 0x00])); // mov edx,[0xABF440]
        assert!(sc.windows(4).any(|w| w == 0x2000_0000u32.to_le_bytes()));
        assert!(sc.windows(2).any(|w| w == [0x39, 0x16])); // cmp [esi],edx
    }

    #[test]
    fn identifies_feature_palette_colors() {
        assert!(is_feature_color(MonsterNameColor::Green.rgb565()));
        assert!(!is_feature_color(DEFAULT_NAME_COLOR));
    }

    #[test]
    fn parses_runtime_sprite_types_from_decrypted_client_memory_text() {
        let raw = b"S12338 0 41211\r\n#94 48 sword orc 102.type(10)\n\0"
            .iter()
            .chain(b"148 48 kent castle guard 102.type(12)\n\0")
            .chain(b"335 32 guard archer 102.type(5)\n\0")
            .chain(b"3864 48=94 orc fighter morph\n\0")
            .copied()
            .collect::<Vec<_>>();

        let records = parse_runtime_sprite_type_records(&raw);
        let types = resolve_runtime_sprite_types(records);

        assert_eq!(types.get(&94), Some(&SPRITE_TYPE_MONSTER));
        assert_eq!(types.get(&3864), Some(&SPRITE_TYPE_MONSTER));
        assert_eq!(types.get(&148), Some(&12));
        assert_eq!(types.get(&335), Some(&5));
    }

    #[test]
    fn direct_sprite_type_zero_overrides_alias_resolution() {
        let raw = b"#94 48 sword orc 102.type(10)\n\0336 32=94 guard archer shadow 102.type(0)\n";

        let records = parse_runtime_sprite_type_records(raw);
        let types = resolve_runtime_sprite_types(records);

        assert_eq!(types.get(&336), Some(&0));
    }
}
