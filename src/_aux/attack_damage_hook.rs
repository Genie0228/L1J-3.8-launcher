//! 3.8 attack damage overhead display hook.
//!
//! Packet paths patched:
//! - `0x005295D9`: single target attack packet path (普攻 / 單體技能,顯示真實傷害)
//!
//! Range packet 內 damage word 是 hit flag (0x0020 = 命中 / 0x0000 = miss),
//! 不是真實傷害 — packet 結構就沒送 damage value,所以範圍技顯示「( 32 ) 累計」
//! 的 32 是命中次數的 hit flag 加總。要顯示真實傷害必須改 server 廣播 packet。
//!
//! Display is done through the client's built-in overhead speech function:
//! `0x42B7B0(target_id, text, 0xf800, 1, 0, 0)`(BGR565 紅,跟白色聊天泡泡區隔)。

use anyhow::{anyhow, bail, Context, Result};
use std::ops::Deref;
use std::sync::Mutex;
use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

use crate::logger::log_line;
use crate::memory;

const ATTACK_HOOK_ADDR: u32 = 0x0052_95D9;
const ATTACK_HOOK_LEN: usize = 10;
const ATTACK_HOOK_FALLTHROUGH_ADDR: u32 = 0x0052_95E3;
const ATTACK_SKIP_ADDR: u32 = 0x0052_9BCD;
const ATTACK_ORIGINAL_BYTES: [u8; ATTACK_HOOK_LEN] = [
    0x83, 0x7D, 0xE0, 0x00, // cmp dword ptr [ebp-0x20], 0
    0x0F, 0x8E, 0xEA, 0x05, 0x00, 0x00, // jle 0x00529BCD
];

const AOE_HOOK_ADDR: u32 = 0x0052_A4F2;
const AOE_HOOK_LEN: usize = 7;
const AOE_HOOK_FALLTHROUGH_ADDR: u32 = AOE_HOOK_ADDR + AOE_HOOK_LEN as u32;
const AOE_ORIGINAL_BYTES: [u8; AOE_HOOK_LEN] = [
    0x83, 0x3D, 0xB8, 0xD2, 0xC2, 0x00, 0x00, // cmp dword ptr [0x00C2D2B8], 0
];

const MAGIC_AOE_HOOK_ADDR: u32 = 0x0052_A8F1;
const MAGIC_AOE_HOOK_LEN: usize = 7;
const MAGIC_AOE_HOOK_FALLTHROUGH_ADDR: u32 = MAGIC_AOE_HOOK_ADDR + MAGIC_AOE_HOOK_LEN as u32;
const MAGIC_AOE_ORIGINAL_BYTES: [u8; MAGIC_AOE_HOOK_LEN] = [
    0x83, 0x3D, 0xB8, 0xD2, 0xC2, 0x00, 0x00, // cmp dword ptr [0x00C2D2B8], 0
];

const MAGIC_AOE_EXT_DAMAGE_HOOK_ADDR: u32 = 0x0052_A821;
const MAGIC_AOE_EXT_DAMAGE_HOOK_LEN: usize = 6;
const MAGIC_AOE_EXT_DAMAGE_FALLTHROUGH_ADDR: u32 =
    MAGIC_AOE_EXT_DAMAGE_HOOK_ADDR + MAGIC_AOE_EXT_DAMAGE_HOOK_LEN as u32;
const MAGIC_AOE_EXT_DAMAGE_ORIGINAL_BYTES: [u8; MAGIC_AOE_EXT_DAMAGE_HOOK_LEN] = [
    0x83, 0xC4, 0x10, // add esp, 0x10
    0x89, 0x45, 0xD0, // mov [ebp-0x30], eax
];

const SELF_CHAR_ID_ADDR: u32 = 0x00AB_F4B4;
const LOCAL_PLAYER_PTR_ADDR: u32 = 0x00C2_D2B8;
const LOCAL_PLAYER_ID_OFFSET: u8 = 0x0C;
const LOCAL_PLAYER_ALT_ID_OFFSET: u8 = 0x14;
const OVERHEAD_TEXT_FN: u32 = 0x0042_B7B0;
const DAMAGE_TEXT_COLOR: u32 = 0x0000_F800;

const CODECAVE_SIZE: usize = 0x8000;
const TEXT_BUFFER_SIZE: usize = 96;
const ACCUMULATOR_TIMEOUT_MS: u32 = 8_000;

#[derive(Debug, Default, Clone, Copy)]
struct LastHitState {
    target_id: u32,
    total: u32,
    tick: u32,
}

fn update_total_state(state: &mut LastHitState, target_id: u32, damage: u32, now: u32) -> u32 {
    let elapsed = now.wrapping_sub(state.tick);
    if state.target_id == target_id && elapsed <= ACCUMULATOR_TIMEOUT_MS {
        state.total = state.total.wrapping_add(damage);
    } else {
        state.target_id = target_id;
        state.total = damage;
    }
    state.tick = now;
    state.total
}

#[derive(Debug)]
struct HookState {
    codecave_addr: u32,
    attack_patch_bytes: [u8; ATTACK_HOOK_LEN],
    magic_aoe_ext_damage_patch_bytes: [u8; MAGIC_AOE_EXT_DAMAGE_HOOK_LEN],
}

#[derive(Debug, Eq, PartialEq)]
enum HookBytes {
    Original,
    Patched,
    Unexpected,
}

static STATE: Mutex<Option<HookState>> = Mutex::new(None);

#[cfg(test)]
fn active_hook_sites() -> &'static [u32] {
    &[ATTACK_HOOK_ADDR, MAGIC_AOE_EXT_DAMAGE_HOOK_ADDR]
}

pub fn is_installed() -> bool {
    STATE
        .lock()
        .expect("attack_damage STATE poisoned")
        .is_some()
}

/// kernel32!GetTickCount 在同 session 32-bit process 共用 base,launcher 解析後直接給遊戲 process 用。
fn resolve_get_tick_count() -> Result<u32> {
    unsafe {
        let kernel32 = GetModuleHandleW(PCWSTR(
            "kernel32.dll\0"
                .encode_utf16()
                .collect::<Vec<u16>>()
                .as_ptr(),
        ))
        .context("GetModuleHandleW(kernel32.dll)")?;
        let proc = GetProcAddress(kernel32, PCSTR(b"GetTickCount\0".as_ptr()))
            .ok_or_else(|| anyhow!("GetProcAddress(GetTickCount) returned NULL"))?;
        Ok(proc as usize as u32)
    }
}

pub fn install(h: HANDLE) -> Result<()> {
    let mut guard = STATE.lock().expect("attack_damage STATE poisoned");
    if guard.is_some() {
        return Ok(());
    }

    ensure_hook_original(h, ATTACK_HOOK_ADDR, &ATTACK_ORIGINAL_BYTES, "attack")?;
    ensure_hook_original(
        h,
        MAGIC_AOE_EXT_DAMAGE_HOOK_ADDR,
        &MAGIC_AOE_EXT_DAMAGE_ORIGINAL_BYTES,
        "magic_aoe_ext_damage",
    )?;

    let cave = memory::alloc_exec(h, CODECAVE_SIZE).context("[attack_damage] alloc codecave")?;
    let gettickcount_addr =
        resolve_get_tick_count().context("[attack_damage] resolve kernel32!GetTickCount")?;
    let shellcode = build_shellcode(cave, gettickcount_addr);
    if shellcode.bytes.len() > CODECAVE_SIZE {
        bail!(
            "[attack_damage] shellcode too large: {} > {}",
            shellcode.bytes.len(),
            CODECAVE_SIZE
        );
    }

    memory::write_code(h, cave, &shellcode.bytes).context("[attack_damage] write codecave")?;

    let attack_patch_bytes = build_hook_patch::<ATTACK_HOOK_LEN>(
        ATTACK_HOOK_ADDR,
        cave + shellcode.attack_entry_offset as u32,
    );
    memory::write_code(h, ATTACK_HOOK_ADDR, &attack_patch_bytes)
        .context("[attack_damage] patch attack hook site")?;

    let magic_aoe_ext_damage_patch_bytes = build_hook_patch::<MAGIC_AOE_EXT_DAMAGE_HOOK_LEN>(
        MAGIC_AOE_EXT_DAMAGE_HOOK_ADDR,
        cave + shellcode.magic_aoe_ext_damage_entry_offset as u32,
    );
    memory::write_code(
        h,
        MAGIC_AOE_EXT_DAMAGE_HOOK_ADDR,
        &magic_aoe_ext_damage_patch_bytes,
    )
    .context("[attack_damage] patch magic aoe extended damage hook site")?;

    *guard = Some(HookState {
        codecave_addr: cave,
        attack_patch_bytes,
        magic_aoe_ext_damage_patch_bytes,
    });

    log_line!(
        "[attack_damage] installed hook @ 0x{ATTACK_HOOK_ADDR:08X}, range_ext @ 0x{MAGIC_AOE_EXT_DAMAGE_HOOK_ADDR:08X}, overhead_text=0x{OVERHEAD_TEXT_FN:08X}, codecave=0x{cave:08X}"
    );
    Ok(())
}

pub fn uninstall(h: HANDLE) -> Result<()> {
    let mut guard = STATE.lock().expect("attack_damage STATE poisoned");
    let Some(state) = guard.take() else {
        return Ok(());
    };

    restore_hook(
        h,
        MAGIC_AOE_EXT_DAMAGE_HOOK_ADDR,
        &MAGIC_AOE_EXT_DAMAGE_ORIGINAL_BYTES,
        &state.magic_aoe_ext_damage_patch_bytes,
        "magic_aoe_ext_damage",
    )?;
    restore_hook(
        h,
        ATTACK_HOOK_ADDR,
        &ATTACK_ORIGINAL_BYTES,
        &state.attack_patch_bytes,
        "attack",
    )?;
    log_line!(
        "[attack_damage] uninstalled hook @ 0x{ATTACK_HOOK_ADDR:08X}; codecave 0x{:08X} left allocated",
        state.codecave_addr
    );
    Ok(())
}

fn ensure_hook_original<const N: usize>(
    h: HANDLE,
    addr: u32,
    original: &[u8; N],
    label: &str,
) -> Result<()> {
    let current = read_hook_bytes(h, addr, N)?;
    match classify_hook_bytes(&current, original, None) {
        HookBytes::Original => Ok(()),
        HookBytes::Patched => bail!("[attack_damage] {label} hook already patched @ 0x{addr:08X}"),
        HookBytes::Unexpected => bail!(
            "[attack_damage] {label} hook bytes mismatch @ 0x{addr:08X}: {:02X?}",
            current
        ),
    }
}

fn restore_hook<const N: usize>(
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
            .with_context(|| format!("[attack_damage] restore {label} hook site")),
        HookBytes::Unexpected => bail!(
            "[attack_damage] refuse to restore unexpected {label} hook bytes @ 0x{addr:08X}: {:02X?}",
            current
        ),
    }
}

fn read_hook_bytes(h: HANDLE, addr: u32, len: usize) -> Result<Vec<u8>> {
    memory::read_bytes(h, addr, len)
        .with_context(|| format!("[attack_damage] read hook bytes @ 0x{addr:08X}"))
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

#[derive(Clone, Copy)]
enum DataRef {
    StackSave,
    TotalDamage,
    CurrentDamage,
    CurrentTarget,
    RandomState,
    LoopIndex,
    LoopCount,
    ColorTable,
    LastTarget,
    LastTotal,
    LastTick,
    GetTickCountPtr,
    TextBuffer,
}

struct AbsFixup {
    offset: usize,
    what: DataRef,
}

struct Shellcode {
    bytes: Vec<u8>,
    attack_entry_offset: usize,
    #[allow(dead_code)]
    aoe_entry_offset: usize,
    #[allow(dead_code)]
    magic_aoe_entry_offset: usize,
    magic_aoe_ext_damage_entry_offset: usize,
}

impl Deref for Shellcode {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

struct AttackBranchFixups {
    skip_rel_offset: usize,
    fallthrough_rel_offset: usize,
}

struct AoeBranchFixups {
    fallthrough_rel_offset: usize,
}

fn push_abs(sc: &mut Vec<u8>, fixups: &mut Vec<AbsFixup>, what: DataRef) {
    fixups.push(AbsFixup {
        offset: sc.len(),
        what,
    });
    sc.extend_from_slice(&0u32.to_le_bytes());
}

fn emit_jcc32(sc: &mut Vec<u8>, second_opcode: u8) -> usize {
    sc.extend_from_slice(&[0x0F, second_opcode]);
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

fn emit_call32(sc: &mut Vec<u8>) -> usize {
    sc.push(0xE8);
    let rel_offset = sc.len();
    sc.extend_from_slice(&0i32.to_le_bytes());
    rel_offset
}

fn patch_rel32(sc: &mut [u8], rel_offset: usize, target_offset: usize) {
    let next = rel_offset + 4;
    let rel = target_offset as isize - next as isize;
    sc[rel_offset..rel_offset + 4].copy_from_slice(&(rel as i32).to_le_bytes());
}

fn patch_abs32(sc: &mut [u8], offset: usize, value: u32) {
    sc[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn build_shellcode(cave: u32, gettickcount_addr: u32) -> Shellcode {
    let mut sc = Vec::<u8>::with_capacity(CODECAVE_SIZE);
    let mut abs_fixups = Vec::<AbsFixup>::new();
    let mut append_calls = Vec::<usize>::new();

    let attack_entry_offset = sc.len();
    let attack_branch = emit_attack_entry(&mut sc, &mut abs_fixups, &mut append_calls);

    let aoe_entry_offset = sc.len();
    let aoe_branch = emit_aoe_entry(&mut sc, &mut abs_fixups, &mut append_calls);

    let magic_aoe_entry_offset = sc.len();
    let magic_aoe_branch = emit_magic_aoe_entry(&mut sc, &mut abs_fixups, &mut append_calls);

    let magic_aoe_ext_damage_entry_offset = sc.len();
    let magic_aoe_ext_damage_branch =
        emit_magic_aoe_ext_damage_entry(&mut sc, &mut abs_fixups, &mut append_calls);

    let append_u32_offset = sc.len();
    for call in append_calls {
        patch_rel32(&mut sc, call, append_u32_offset);
    }
    emit_append_u32(&mut sc);

    patch_abs_data(&mut sc, cave, gettickcount_addr, abs_fixups);
    patch_attack_branch(&mut sc, cave, attack_branch);
    patch_aoe_branch(&mut sc, cave, aoe_branch);
    patch_magic_aoe_branch(&mut sc, cave, magic_aoe_branch);
    patch_magic_aoe_ext_damage_branch(&mut sc, cave, magic_aoe_ext_damage_branch);

    Shellcode {
        bytes: sc,
        attack_entry_offset,
        aoe_entry_offset,
        magic_aoe_entry_offset,
        magic_aoe_ext_damage_entry_offset,
    }
}

fn emit_entry_prologue(sc: &mut Vec<u8>, abs_fixups: &mut Vec<AbsFixup>) {
    sc.extend_from_slice(&[0x9C, 0x60]); // pushfd; pushad
    sc.extend_from_slice(&[0x89, 0x25]); // mov [stack_save], esp
    push_abs(sc, abs_fixups, DataRef::StackSave);
}

fn emit_attack_entry(
    sc: &mut Vec<u8>,
    abs_fixups: &mut Vec<AbsFixup>,
    append_calls: &mut Vec<usize>,
) -> AttackBranchFixups {
    let mut skip_jumps = Vec::<usize>::new();
    emit_entry_prologue(sc, abs_fixups);

    sc.extend_from_slice(&[0x8B, 0x55, 0xEC]); // mov edx, [ebp-0x14] attacker
    emit_attacker_filter(sc, &mut skip_jumps);

    sc.extend_from_slice(&[0x8B, 0x4D, 0xF0]); // mov ecx, [ebp-0x10] target
    sc.extend_from_slice(&[0x0F, 0xB7, 0x55, 0xCC]); // movzx edx, word [ebp-0x34] damage

    emit_update_total_and_display(sc, abs_fixups, &mut skip_jumps, append_calls);
    emit_restore_and_skip(sc, abs_fixups, skip_jumps);

    sc.extend_from_slice(&[0x83, 0x7D, 0xE0, 0x00]); // cmp dword ptr [ebp-0x20], 0
    sc.extend_from_slice(&[0x0F, 0x8E]);
    let skip_rel_offset = sc.len();
    sc.extend_from_slice(&0i32.to_le_bytes());
    sc.push(0xE9);
    let fallthrough_rel_offset = sc.len();
    sc.extend_from_slice(&0i32.to_le_bytes());

    AttackBranchFixups {
        skip_rel_offset,
        fallthrough_rel_offset,
    }
}

fn emit_aoe_entry(
    sc: &mut Vec<u8>,
    abs_fixups: &mut Vec<AbsFixup>,
    append_calls: &mut Vec<usize>,
) -> AoeBranchFixups {
    let mut skip_jumps = Vec::<usize>::new();
    emit_entry_prologue(sc, abs_fixups);

    sc.extend_from_slice(&[0x8B, 0x55, 0xF0]); // mov edx, [ebp-0x10] attacker
    emit_attacker_filter(sc, &mut skip_jumps);

    emit_display_aggregated_aoe_hits(
        sc,
        abs_fixups,
        &mut skip_jumps,
        append_calls,
        &[0x8B, 0x75, 0xE4],       // mov esi, [ebp-0x1C] hit container
        &[0x0F, 0xB7, 0x45, 0xE0], // movzx eax, word [ebp-0x20] count
        &[
            0x8B, 0x46, 0x08, // mov eax, [esi+8]
            0x0F, 0xB7, 0x04, 0x58, // movzx eax, word [eax+ebx*2]
        ],
    );
    emit_restore_and_skip(sc, abs_fixups, skip_jumps);

    sc.extend_from_slice(&AOE_ORIGINAL_BYTES);
    sc.push(0xE9);
    let fallthrough_rel_offset = sc.len();
    sc.extend_from_slice(&0i32.to_le_bytes());

    AoeBranchFixups {
        fallthrough_rel_offset,
    }
}

fn emit_magic_aoe_entry(
    sc: &mut Vec<u8>,
    abs_fixups: &mut Vec<AbsFixup>,
    append_calls: &mut Vec<usize>,
) -> AoeBranchFixups {
    let mut skip_jumps = Vec::<usize>::new();
    emit_entry_prologue(sc, abs_fixups);

    sc.extend_from_slice(&[0x8B, 0x55, 0xE4]); // mov edx, [ebp-0x1C] magic caster
    emit_attacker_filter(sc, &mut skip_jumps);

    emit_display_aggregated_aoe_hits(
        sc,
        abs_fixups,
        &mut skip_jumps,
        append_calls,
        &[0x8B, 0x75, 0xBC],       // mov esi, [ebp-0x44] hit container
        &[0x0F, 0xB7, 0x45, 0xB4], // movzx eax, word [ebp-0x4C] count
        &[
            0x8B, 0x46, 0x08, // mov eax, [esi+8]
            0x0F, 0xB7, 0x04, 0x58, // movzx eax, word [eax+ebx*2]
        ],
    );
    emit_restore_and_skip(sc, abs_fixups, skip_jumps);

    sc.extend_from_slice(&MAGIC_AOE_ORIGINAL_BYTES);
    sc.push(0xE9);
    let fallthrough_rel_offset = sc.len();
    sc.extend_from_slice(&0i32.to_le_bytes());

    AoeBranchFixups {
        fallthrough_rel_offset,
    }
}

fn emit_magic_aoe_ext_damage_entry(
    sc: &mut Vec<u8>,
    abs_fixups: &mut Vec<AbsFixup>,
    append_calls: &mut Vec<usize>,
) -> AoeBranchFixups {
    let mut skip_jumps = Vec::<usize>::new();

    // Opcode 42 forked server layout:
    // repeat targetCount: D targetId, H hitFlag, D damage.
    // The original parser has just consumed D/H and returned the packet cursor
    // in EAX. Consume the extra damage dword before replaying the original
    // parse epilogue so the next target starts aligned.
    sc.extend_from_slice(&[0x8B, 0x10]); // mov edx, [eax] damage
    sc.extend_from_slice(&[0x83, 0xC0, 0x04]); // add eax, 4

    emit_entry_prologue(sc, abs_fixups);

    sc.extend_from_slice(&[0x89, 0x15]);
    push_abs(sc, abs_fixups, DataRef::CurrentDamage);

    sc.extend_from_slice(&[0x8B, 0x55, 0xE4]); // mov edx, [ebp-0x1C] caster
    emit_attacker_filter(sc, &mut skip_jumps);

    sc.extend_from_slice(&[0x8B, 0x75, 0xBC]); // mov esi, [ebp-0x44] hit container
    sc.extend_from_slice(&[0x85, 0xF6]); // test esi, esi
    skip_jumps.push(emit_jcc32(sc, 0x84));

    sc.extend_from_slice(&[0x8B, 0x9D, 0x10, 0xFE, 0xFF, 0xFF]); // mov ebx, [ebp-0x1F0] index

    sc.extend_from_slice(&[0x8B, 0x7E, 0x08]); // mov edi, [esi+8] hit flag array
    sc.extend_from_slice(&[0x85, 0xFF]); // test edi, edi
    skip_jumps.push(emit_jcc32(sc, 0x84));
    sc.extend_from_slice(&[0x0F, 0xB7, 0x04, 0x5F]); // movzx eax, word [edi+ebx*2]
    sc.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
    skip_jumps.push(emit_jcc32(sc, 0x84));

    sc.extend_from_slice(&[0x8B, 0x7E, 0x04]); // mov edi, [esi+4] target array
    sc.extend_from_slice(&[0x85, 0xFF]); // test edi, edi
    skip_jumps.push(emit_jcc32(sc, 0x84));
    sc.extend_from_slice(&[0x8B, 0x0C, 0x9F]); // mov ecx, [edi+ebx*4] target
    sc.extend_from_slice(&[0x85, 0xC9]); // test ecx, ecx
    skip_jumps.push(emit_jcc32(sc, 0x84));

    sc.extend_from_slice(&[0x8B, 0x15]);
    push_abs(sc, abs_fixups, DataRef::CurrentDamage);
    sc.extend_from_slice(&[0x85, 0xD2]); // test edx, edx
    skip_jumps.push(emit_jcc32(sc, 0x8E)); // skip damage <= 0

    emit_update_total_and_display_no_checks(sc, abs_fixups, append_calls);
    emit_restore_and_skip(sc, abs_fixups, skip_jumps);

    sc.extend_from_slice(&MAGIC_AOE_EXT_DAMAGE_ORIGINAL_BYTES);
    sc.push(0xE9);
    let fallthrough_rel_offset = sc.len();
    sc.extend_from_slice(&0i32.to_le_bytes());

    AoeBranchFixups {
        fallthrough_rel_offset,
    }
}

fn emit_attacker_filter(sc: &mut Vec<u8>, skip_jumps: &mut Vec<usize>) {
    sc.extend_from_slice(&[0x8B, 0x0D]);
    sc.extend_from_slice(&SELF_CHAR_ID_ADDR.to_le_bytes());
    sc.extend_from_slice(&[0x39, 0xCA]); // cmp edx, ecx
    let attacker_ok_self = emit_jcc32(sc, 0x84);

    sc.extend_from_slice(&[0xA1]);
    sc.extend_from_slice(&LOCAL_PLAYER_PTR_ADDR.to_le_bytes());
    sc.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
    skip_jumps.push(emit_jcc32(sc, 0x84));

    sc.extend_from_slice(&[0x8B, 0x48, LOCAL_PLAYER_ID_OFFSET]);
    sc.extend_from_slice(&[0x39, 0xCA]);
    let attacker_ok_player_id = emit_jcc32(sc, 0x84);

    sc.extend_from_slice(&[0x8B, 0x48, LOCAL_PLAYER_ALT_ID_OFFSET]);
    sc.extend_from_slice(&[0x39, 0xCA]);
    skip_jumps.push(emit_jcc32(sc, 0x85));

    let attacker_ok_offset = sc.len();
    patch_rel32(sc, attacker_ok_self, attacker_ok_offset);
    patch_rel32(sc, attacker_ok_player_id, attacker_ok_offset);
}

fn emit_display_aggregated_aoe_hits(
    sc: &mut Vec<u8>,
    abs_fixups: &mut Vec<AbsFixup>,
    skip_jumps: &mut Vec<usize>,
    append_calls: &mut Vec<usize>,
    load_container: &[u8],
    load_count: &[u8],
    load_hit_damage: &[u8],
) {
    // The hook runs after the client has parsed the full range-hit array.
    // Aggregate duplicate target entries once, otherwise early chunks such as
    // 32 + 56 would display only the first parsed value.
    sc.extend_from_slice(load_container);
    sc.extend_from_slice(&[0x85, 0xF6]); // test esi, esi
    skip_jumps.push(emit_jcc32(sc, 0x84));

    sc.extend_from_slice(&[0x8B, 0x56, 0x08]); // mov edx, [esi+8] damage array
    sc.extend_from_slice(&[0x85, 0xD2]);
    skip_jumps.push(emit_jcc32(sc, 0x84));

    sc.extend_from_slice(&[0x8B, 0x7E, 0x04]); // mov edi, [esi+4] target array
    sc.extend_from_slice(&[0x85, 0xFF]); // test edi, edi
    skip_jumps.push(emit_jcc32(sc, 0x84));

    sc.extend_from_slice(load_count);
    sc.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
    skip_jumps.push(emit_jcc32(sc, 0x84));
    sc.push(0xA3); // mov [loop_count], eax
    push_abs(sc, abs_fixups, DataRef::LoopCount);
    sc.extend_from_slice(&[0xC7, 0x05]); // mov dword ptr [loop_index], 0
    push_abs(sc, abs_fixups, DataRef::LoopIndex);
    sc.extend_from_slice(&0u32.to_le_bytes());

    let outer_loop = sc.len();
    sc.extend_from_slice(load_container);
    sc.extend_from_slice(&[0x8B, 0x7E, 0x04]); // mov edi, [esi+4]
    sc.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, [loop_index]
    push_abs(sc, abs_fixups, DataRef::LoopIndex);
    sc.push(0xA1); // mov eax, [loop_count]
    push_abs(sc, abs_fixups, DataRef::LoopCount);
    sc.extend_from_slice(&[0x39, 0xC3]); // cmp ebx, eax
    let done_jump = emit_jcc32(sc, 0x8D);

    sc.extend_from_slice(&[0x8B, 0x0C, 0x9F]); // mov ecx, [edi+ebx*4] target
    sc.extend_from_slice(&[0x85, 0xC9]); // test ecx, ecx
    let outer_next_jumps_first = emit_jcc32(sc, 0x84);

    sc.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx; prior index
    let prior_loop = sc.len();
    sc.extend_from_slice(&[0x39, 0xDA]); // cmp edx, ebx
    let no_prior_jump = emit_jcc32(sc, 0x8D);
    sc.extend_from_slice(&[0x39, 0x0C, 0x97]); // cmp [edi+edx*4], ecx
    let outer_next_jumps_prior = emit_jcc32(sc, 0x84);
    sc.push(0x42); // inc edx
    let prior_next_jump = emit_jmp32(sc);

    let sum_start = sc.len();
    patch_rel32(sc, no_prior_jump, sum_start);
    patch_rel32(sc, prior_next_jump, prior_loop);

    sc.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx; total damage
    sc.extend_from_slice(&[0x31, 0xDB]); // xor ebx, ebx; sum index

    let sum_loop = sc.len();
    sc.push(0xA1); // mov eax, [loop_count]
    push_abs(sc, abs_fixups, DataRef::LoopCount);
    sc.extend_from_slice(&[0x39, 0xC3]); // cmp ebx, eax
    let sum_done_jump = emit_jcc32(sc, 0x8D);
    sc.extend_from_slice(&[0x39, 0x0C, 0x9F]); // cmp [edi+ebx*4], ecx
    let next_jump = emit_jcc32(sc, 0x85);
    sc.push(0x50); // push eax
    sc.extend_from_slice(load_hit_damage);
    sc.extend_from_slice(&[0x01, 0xC2]); // add edx, eax
    sc.push(0x58); // pop eax

    let next_offset = sc.len();
    patch_rel32(sc, next_jump, next_offset);
    sc.push(0x43); // inc ebx
    let loop_jump = emit_jmp32(sc);

    let done_offset = sc.len();
    patch_rel32(sc, sum_done_jump, done_offset);
    patch_rel32(sc, loop_jump, sum_loop);

    sc.extend_from_slice(&[0x85, 0xD2]); // test edx, edx
    let outer_next_jumps_zero_total = emit_jcc32(sc, 0x84);
    emit_update_total_and_display_no_checks(sc, abs_fixups, append_calls);

    let outer_next = sc.len();
    patch_rel32(sc, outer_next_jumps_first, outer_next);
    patch_rel32(sc, outer_next_jumps_prior, outer_next);
    patch_rel32(sc, outer_next_jumps_zero_total, outer_next);
    sc.extend_from_slice(&[0xFF, 0x05]); // inc dword ptr [loop_index]
    push_abs(sc, abs_fixups, DataRef::LoopIndex);
    let outer_jump = emit_jmp32(sc);

    let done_offset = sc.len();
    patch_rel32(sc, done_jump, done_offset);
    patch_rel32(sc, outer_jump, outer_loop);
}

fn emit_update_total_and_display(
    sc: &mut Vec<u8>,
    abs_fixups: &mut Vec<AbsFixup>,
    skip_jumps: &mut Vec<usize>,
    append_calls: &mut Vec<usize>,
) {
    sc.extend_from_slice(&[0x85, 0xC9]); // test ecx, ecx
    skip_jumps.push(emit_jcc32(sc, 0x84));
    sc.extend_from_slice(&[0x85, 0xD2]); // test edx, edx
    skip_jumps.push(emit_jcc32(sc, 0x84));

    sc.extend_from_slice(&[0x89, 0x15]);
    push_abs(sc, abs_fixups, DataRef::CurrentDamage);
    sc.extend_from_slice(&[0x89, 0x0D]);
    push_abs(sc, abs_fixups, DataRef::CurrentTarget);

    emit_update_target_total(sc, abs_fixups);
    emit_damage_text(sc, abs_fixups, append_calls);
    emit_overhead_text_call(sc, abs_fixups);
}

fn emit_update_total_and_display_no_checks(
    sc: &mut Vec<u8>,
    abs_fixups: &mut Vec<AbsFixup>,
    append_calls: &mut Vec<usize>,
) {
    sc.extend_from_slice(&[0x89, 0x15]);
    push_abs(sc, abs_fixups, DataRef::CurrentDamage);
    sc.extend_from_slice(&[0x89, 0x0D]);
    push_abs(sc, abs_fixups, DataRef::CurrentTarget);

    emit_update_target_total(sc, abs_fixups);
    emit_damage_text(sc, abs_fixups, append_calls);
    emit_overhead_text_call(sc, abs_fixups);
}

fn emit_update_target_total(sc: &mut Vec<u8>, abs_fixups: &mut Vec<AbsFixup>) {
    // Single-slot accumulator with GetTickCount-based timeout. Mirrors
    // `update_total_state` in this module.
    //   ecx = current target_id
    //   edx = current damage
    //   data: last_target / last_total / last_tick / gettickcount_ptr

    // ecx/edx are stdcall caller-save; kernel32!GetTickCount may clobber them.
    // Save and restore so the accumulator updates with the real target/damage.
    sc.push(0x51); // push ecx (target_id)
    sc.push(0x52); // push edx (current damage)
    sc.extend_from_slice(&[0xFF, 0x15]); // call dword [gettickcount_ptr] -> eax = now ms
    push_abs(sc, abs_fixups, DataRef::GetTickCountPtr);
    sc.push(0x5A); // pop edx
    sc.push(0x59); // pop ecx

    sc.extend_from_slice(&[0x89, 0xC3]); // mov ebx, eax  (save now)
    sc.extend_from_slice(&[0x2B, 0x1D]); // sub ebx, [last_tick]  -> elapsed
    push_abs(sc, abs_fixups, DataRef::LastTick);

    sc.extend_from_slice(&[0x3B, 0x0D]); // cmp ecx, [last_target]
    push_abs(sc, abs_fixups, DataRef::LastTarget);
    let reset_on_target_change = emit_jcc32(sc, 0x85); // jne .reset

    sc.extend_from_slice(&[0x81, 0xFB]); // cmp ebx, ACCUMULATOR_TIMEOUT_MS
    sc.extend_from_slice(&ACCUMULATOR_TIMEOUT_MS.to_le_bytes());
    let reset_on_timeout = emit_jcc32(sc, 0x87); // ja .reset (unsigned greater)

    sc.extend_from_slice(&[0x01, 0x15]); // add [last_total], edx
    push_abs(sc, abs_fixups, DataRef::LastTotal);
    let commit_jmp = emit_jmp32(sc);

    let reset_offset = sc.len();
    patch_rel32(sc, reset_on_target_change, reset_offset);
    patch_rel32(sc, reset_on_timeout, reset_offset);
    sc.extend_from_slice(&[0x89, 0x0D]); // mov [last_target], ecx
    push_abs(sc, abs_fixups, DataRef::LastTarget);
    sc.extend_from_slice(&[0x89, 0x15]); // mov [last_total], edx
    push_abs(sc, abs_fixups, DataRef::LastTotal);

    let commit_offset = sc.len();
    patch_rel32(sc, commit_jmp, commit_offset);

    sc.push(0xA3); // mov [last_tick], eax
    push_abs(sc, abs_fixups, DataRef::LastTick);

    sc.push(0xA1); // mov eax, [last_total]
    push_abs(sc, abs_fixups, DataRef::LastTotal);
    sc.push(0xA3); // mov [total_damage], eax
    push_abs(sc, abs_fixups, DataRef::TotalDamage);
}

fn emit_damage_text(
    sc: &mut Vec<u8>,
    abs_fixups: &mut Vec<AbsFixup>,
    append_calls: &mut Vec<usize>,
) {
    sc.push(0xBF); // mov edi, text_buffer
    push_abs(sc, abs_fixups, DataRef::TextBuffer);
    // 14-byte preamble: "\\fRf>( \\fRf0"
    //   - "\\fRf>" 把後續字染成 Lineage 色 14(白),涵蓋 '(' 跟空格
    //   - "( "    可見的左括號 + 空格
    //   - "\\fRf0" 把後續(當下傷害數字)染成色 0,'0' 之後會被 random color 改寫
    sc.extend_from_slice(&[0xC7, 0x07, 0x5C, 0x5C, 0x66, 0x52]); // [edi+0]  = "\\fR"
    sc.extend_from_slice(&[0xC7, 0x47, 0x04, 0x66, 0x3E, 0x28, 0x20]); // [edi+4]  = "f>( "
    sc.extend_from_slice(&[0xC7, 0x47, 0x08, 0x5C, 0x5C, 0x66, 0x52]); // [edi+8]  = "\\fR"
    sc.extend_from_slice(&[0x66, 0xC7, 0x47, 0x0C, 0x66, 0x30]); // [edi+12] = "f0" (word)
    sc.extend_from_slice(&[0x83, 0xC7, 0x0E]); // add edi, 14

    sc.push(0xA1); // mov eax, [random_state]
    push_abs(sc, abs_fixups, DataRef::RandomState);
    sc.extend_from_slice(&[0x69, 0xC0, 0xFD, 0x43, 0x03, 0x00]);
    sc.extend_from_slice(&[0x03, 0x05]);
    push_abs(sc, abs_fixups, DataRef::CurrentDamage);
    sc.extend_from_slice(&[0x03, 0x05]);
    push_abs(sc, abs_fixups, DataRef::CurrentTarget);
    sc.extend_from_slice(&[0x05, 0xC3, 0x9E, 0x26, 0x00]);
    sc.push(0xA3);
    push_abs(sc, abs_fixups, DataRef::RandomState);
    sc.extend_from_slice(&[0xC1, 0xE8, 0x08]);
    sc.extend_from_slice(&[0x83, 0xE0, 0x03]);
    sc.extend_from_slice(&[0x8A, 0x98]);
    push_abs(sc, abs_fixups, DataRef::ColorTable);
    sc.extend_from_slice(&[0x88, 0x5F, 0xFF]); // update inline color digit

    sc.push(0xA1);
    push_abs(sc, abs_fixups, DataRef::CurrentDamage);
    append_calls.push(emit_call32(sc));
    sc.extend_from_slice(&[0xC7, 0x07, 0x5C, 0x5C, 0x66, 0x52]);
    sc.extend_from_slice(&[0xC7, 0x47, 0x04, 0x66, 0x3E, 0x20, 0x29]);
    sc.extend_from_slice(&[0xC6, 0x47, 0x08, 0x20]);
    sc.extend_from_slice(&[0x83, 0xC7, 0x09]);

    sc.push(0xA1);
    push_abs(sc, abs_fixups, DataRef::TotalDamage);
    append_calls.push(emit_call32(sc));
    sc.extend_from_slice(&[0xC6, 0x07, 0x00]);
}

fn emit_restore_and_skip(sc: &mut Vec<u8>, abs_fixups: &mut Vec<AbsFixup>, skip_jumps: Vec<usize>) {
    sc.extend_from_slice(&[0x8B, 0x25]); // mov esp, [stack_save]
    push_abs(sc, abs_fixups, DataRef::StackSave);

    let skip_offset = sc.len();
    for jump in skip_jumps {
        patch_rel32(sc, jump, skip_offset);
    }

    sc.extend_from_slice(&[0x61, 0x9D]); // popad; popfd
}

fn patch_attack_branch(sc: &mut [u8], cave: u32, fixups: AttackBranchFixups) {
    let skip_next_ip = cave as i64 + fixups.skip_rel_offset as i64 + 4;
    let skip_rel = ATTACK_SKIP_ADDR as i64 - skip_next_ip;
    patch_abs32(sc, fixups.skip_rel_offset, skip_rel as u32);

    let fallthrough_next_ip = cave as i64 + fixups.fallthrough_rel_offset as i64 + 4;
    let fallthrough_rel = ATTACK_HOOK_FALLTHROUGH_ADDR as i64 - fallthrough_next_ip;
    patch_abs32(sc, fixups.fallthrough_rel_offset, fallthrough_rel as u32);
}

fn patch_aoe_branch(sc: &mut [u8], cave: u32, fixups: AoeBranchFixups) {
    let fallthrough_next_ip = cave as i64 + fixups.fallthrough_rel_offset as i64 + 4;
    let fallthrough_rel = AOE_HOOK_FALLTHROUGH_ADDR as i64 - fallthrough_next_ip;
    patch_abs32(sc, fixups.fallthrough_rel_offset, fallthrough_rel as u32);
}

fn patch_magic_aoe_branch(sc: &mut [u8], cave: u32, fixups: AoeBranchFixups) {
    let fallthrough_next_ip = cave as i64 + fixups.fallthrough_rel_offset as i64 + 4;
    let fallthrough_rel = MAGIC_AOE_HOOK_FALLTHROUGH_ADDR as i64 - fallthrough_next_ip;
    patch_abs32(sc, fixups.fallthrough_rel_offset, fallthrough_rel as u32);
}

fn patch_magic_aoe_ext_damage_branch(sc: &mut [u8], cave: u32, fixups: AoeBranchFixups) {
    let fallthrough_next_ip = cave as i64 + fixups.fallthrough_rel_offset as i64 + 4;
    let fallthrough_rel = MAGIC_AOE_EXT_DAMAGE_FALLTHROUGH_ADDR as i64 - fallthrough_next_ip;
    patch_abs32(sc, fixups.fallthrough_rel_offset, fallthrough_rel as u32);
}

fn emit_overhead_text_call(sc: &mut Vec<u8>, fixups: &mut Vec<AbsFixup>) {
    sc.extend_from_slice(&[0x6A, 0x00]); // push 0
    sc.extend_from_slice(&[0x6A, 0x00]); // push 0
    sc.extend_from_slice(&[0x6A, 0x01]); // push 1
    sc.push(0x68); // push DAMAGE_TEXT_COLOR
    sc.extend_from_slice(&DAMAGE_TEXT_COLOR.to_le_bytes());
    sc.push(0x68); // push text_buffer
    push_abs(sc, fixups, DataRef::TextBuffer);
    sc.extend_from_slice(&[0xFF, 0x35]); // push [current_target]
    push_abs(sc, fixups, DataRef::CurrentTarget);
    sc.push(0xB8); // mov eax, OVERHEAD_TEXT_FN
    sc.extend_from_slice(&OVERHEAD_TEXT_FN.to_le_bytes());
    sc.extend_from_slice(&[0xFF, 0xD0]); // call eax
    sc.extend_from_slice(&[0x83, 0xC4, 0x18]); // add esp, 24
}

fn emit_append_u32(sc: &mut Vec<u8>) {
    let start = sc.len();
    sc.extend_from_slice(&[0x53, 0x51, 0x52, 0x56]); // push ebx; push ecx; push edx; push esi
    sc.extend_from_slice(&[0x31, 0xC9, 0xBB, 0x0A, 0x00, 0x00, 0x00, 0x85, 0xC0]);
    let jne_loop_offset = sc.len();
    sc.extend_from_slice(&[0x75, 0x00]);
    sc.extend_from_slice(&[0xC6, 0x07, 0x30, 0x47]); // zero: *edi++ = '0'
    let jmp_done_offset = sc.len();
    sc.extend_from_slice(&[0xEB, 0x00]);

    let loop_offset = sc.len();
    sc.extend_from_slice(&[0x31, 0xD2, 0xF7, 0xF3, 0x80, 0xC2, 0x30, 0x52, 0x41]);
    sc.extend_from_slice(&[0x85, 0xC0]);
    let rel_loop = loop_offset as isize - (sc.len() as isize + 2);
    sc.extend_from_slice(&[0x75, rel_loop as u8]);

    let write_offset = sc.len();
    sc.extend_from_slice(&[0x5A, 0x88, 0x17, 0x47]);
    let rel_write = write_offset as isize - (sc.len() as isize + 2);
    sc.extend_from_slice(&[0xE2, rel_write as u8]);

    let done_offset = sc.len();
    sc.extend_from_slice(&[0x5E, 0x5A, 0x59, 0x5B, 0xC3]); // pop esi; pop edx; pop ecx; pop ebx; ret

    let rel_to_loop = loop_offset as isize - (jne_loop_offset as isize + 2);
    sc[jne_loop_offset + 1] = rel_to_loop as u8;
    let rel_to_done = done_offset as isize - (jmp_done_offset as isize + 2);
    sc[jmp_done_offset + 1] = rel_to_done as u8;

    debug_assert!(sc.len() - start < 0x40);
}

fn patch_abs_data(sc: &mut Vec<u8>, cave: u32, gettickcount_addr: u32, fixups: Vec<AbsFixup>) {
    let stack_save_offset = sc.len();
    sc.extend_from_slice(&0u32.to_le_bytes());
    let total_damage_offset = sc.len();
    sc.extend_from_slice(&0u32.to_le_bytes());
    let current_damage_offset = sc.len();
    sc.extend_from_slice(&0u32.to_le_bytes());
    let current_target_offset = sc.len();
    sc.extend_from_slice(&0u32.to_le_bytes());
    let random_state_offset = sc.len();
    sc.extend_from_slice(&(cave ^ 0xA5A5_5A5A).to_le_bytes());
    let loop_index_offset = sc.len();
    sc.extend_from_slice(&0u32.to_le_bytes());
    let loop_count_offset = sc.len();
    sc.extend_from_slice(&0u32.to_le_bytes());
    let color_table_offset = sc.len();
    sc.extend_from_slice(b"2222");
    let last_target_offset = sc.len();
    sc.extend_from_slice(&0u32.to_le_bytes());
    let last_total_offset = sc.len();
    sc.extend_from_slice(&0u32.to_le_bytes());
    let last_tick_offset = sc.len();
    sc.extend_from_slice(&0u32.to_le_bytes());
    let gettickcount_ptr_offset = sc.len();
    sc.extend_from_slice(&gettickcount_addr.to_le_bytes());
    let text_buffer_offset = sc.len();
    sc.resize(sc.len() + TEXT_BUFFER_SIZE, 0);

    for fixup in fixups {
        let offset = match fixup.what {
            DataRef::StackSave => stack_save_offset,
            DataRef::TotalDamage => total_damage_offset,
            DataRef::CurrentDamage => current_damage_offset,
            DataRef::CurrentTarget => current_target_offset,
            DataRef::RandomState => random_state_offset,
            DataRef::LoopIndex => loop_index_offset,
            DataRef::LoopCount => loop_count_offset,
            DataRef::ColorTable => color_table_offset,
            DataRef::LastTarget => last_target_offset,
            DataRef::LastTotal => last_total_offset,
            DataRef::LastTick => last_tick_offset,
            DataRef::GetTickCountPtr => gettickcount_ptr_offset,
            DataRef::TextBuffer => text_buffer_offset,
        };
        patch_abs32(sc, fixup.offset, cave + offset as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_slot_first_hit_reports_current_damage_only() {
        let mut state = LastHitState::default();
        assert_eq!(update_total_state(&mut state, 0x12345, 7, 1_000), 7);
        assert_eq!(state.target_id, 0x12345);
        assert_eq!(state.total, 7);
        assert_eq!(state.tick, 1_000);
    }

    #[test]
    fn single_slot_accumulates_for_same_target_within_timeout() {
        let mut state = LastHitState::default();
        update_total_state(&mut state, 0x100, 7, 1_000);
        assert_eq!(update_total_state(&mut state, 0x100, 10, 2_500), 17);
        assert_eq!(
            update_total_state(&mut state, 0x100, 5, 9_400),
            22,
            "still within {ACCUMULATOR_TIMEOUT_MS}ms of last hit"
        );
        assert_eq!(state.tick, 9_400);
    }

    #[test]
    fn single_slot_resets_when_target_changes() {
        let mut state = LastHitState::default();
        update_total_state(&mut state, 0x100, 50, 1_000);
        assert_eq!(
            update_total_state(&mut state, 0x200, 7, 1_500),
            7,
            "switching target must drop previous monster's accumulator"
        );
        assert_eq!(state.target_id, 0x200);
        assert_eq!(state.total, 7);
    }

    #[test]
    fn single_slot_resets_when_idle_exceeds_timeout() {
        let mut state = LastHitState::default();
        update_total_state(&mut state, 0x100, 200, 1_000);
        assert_eq!(
            update_total_state(&mut state, 0x100, 7, 1_000 + ACCUMULATOR_TIMEOUT_MS + 1),
            7,
            "same target id after timeout = monster respawn (server reused id), reset"
        );
        assert_eq!(state.total, 7);
    }

    #[test]
    fn single_slot_handles_get_tick_count_wraparound() {
        let mut state = LastHitState {
            target_id: 0x100,
            total: 50,
            tick: 0xFFFF_FFF0,
        };
        assert_eq!(
            update_total_state(&mut state, 0x100, 7, 100),
            57,
            "GetTickCount wrap (delta = ~16ms) must keep accumulating"
        );
    }

    #[test]
    fn classifies_hook_bytes() {
        assert_eq!(
            classify_hook_bytes(&ATTACK_ORIGINAL_BYTES, &ATTACK_ORIGINAL_BYTES, None),
            HookBytes::Original
        );
        let patch = build_hook_patch::<ATTACK_HOOK_LEN>(ATTACK_HOOK_ADDR, 0x1000_0000);
        assert_eq!(
            classify_hook_bytes(&patch, &ATTACK_ORIGINAL_BYTES, Some(&patch)),
            HookBytes::Patched
        );
        assert_eq!(
            classify_hook_bytes(&[0x90; ATTACK_HOOK_LEN], &ATTACK_ORIGINAL_BYTES, None),
            HookBytes::Unexpected
        );
    }

    #[test]
    fn hook_patches_jump_to_requested_targets() {
        let target = 0x1234_5000u32;
        let patch = build_hook_patch::<ATTACK_HOOK_LEN>(ATTACK_HOOK_ADDR, target);
        assert_eq!(patch[0], 0xE9);
        let rel = i32::from_le_bytes(patch[1..5].try_into().unwrap());
        assert_eq!((ATTACK_HOOK_ADDR as i64 + 5 + rel as i64) as u32, target);

        let aoe_patch = build_hook_patch::<AOE_HOOK_LEN>(AOE_HOOK_ADDR, target);
        assert_eq!(aoe_patch[0], 0xE9);
        assert_eq!(aoe_patch[5], 0x90);
        assert_eq!(aoe_patch[6], 0x90);

        let magic_aoe_patch = build_hook_patch::<MAGIC_AOE_HOOK_LEN>(MAGIC_AOE_HOOK_ADDR, target);
        assert_eq!(magic_aoe_patch[0], 0xE9);
        assert_eq!(magic_aoe_patch[5], 0x90);
        assert_eq!(magic_aoe_patch[6], 0x90);
    }

    #[test]
    fn shellcode_fits_codecave_and_has_expected_entries() {
        let sc = build_shellcode(0x2000_0000, 0x7700_1234);
        assert!(sc.bytes.len() <= CODECAVE_SIZE);
        assert_eq!(sc.attack_entry_offset, 0);
        assert!(sc.aoe_entry_offset > sc.attack_entry_offset);
        assert!(sc.magic_aoe_entry_offset > sc.aoe_entry_offset);
    }

    #[test]
    fn shellcode_branches_back_to_original_paths() {
        let cave = 0x2000_0000u32;
        let sc = build_shellcode(cave, 0x7700_1234);

        let attack_cmp = sc[sc.attack_entry_offset..]
            .windows(6)
            .position(|w| w == [0x83, 0x7D, 0xE0, 0x00, 0x0F, 0x8E])
            .map(|p| sc.attack_entry_offset + p)
            .expect("missing replayed attack damage branch");
        let attack_skip_rel_offset = attack_cmp + 6;
        assert_eq!(
            (cave as i64
                + attack_skip_rel_offset as i64
                + 4
                + i32::from_le_bytes(
                    sc[attack_skip_rel_offset..attack_skip_rel_offset + 4]
                        .try_into()
                        .unwrap()
                ) as i64) as u32,
            ATTACK_SKIP_ADDR
        );
        let attack_fallthrough_rel_offset = attack_skip_rel_offset + 4 + 1;
        assert_eq!(
            (cave as i64
                + attack_fallthrough_rel_offset as i64
                + 4
                + i32::from_le_bytes(
                    sc[attack_fallthrough_rel_offset..attack_fallthrough_rel_offset + 4]
                        .try_into()
                        .unwrap()
                ) as i64) as u32,
            ATTACK_HOOK_FALLTHROUGH_ADDR
        );

        let area_cmp = sc[sc.aoe_entry_offset..]
            .windows(AOE_ORIGINAL_BYTES.len())
            .position(|w| w == AOE_ORIGINAL_BYTES)
            .map(|p| sc.aoe_entry_offset + p)
            .expect("missing replayed aoe cmp");
        let jmp_rel_offset = area_cmp + AOE_ORIGINAL_BYTES.len() + 1;
        let jmp_rel =
            i32::from_le_bytes(sc[jmp_rel_offset..jmp_rel_offset + 4].try_into().unwrap());
        assert_eq!(
            (cave as i64 + jmp_rel_offset as i64 + 4 + jmp_rel as i64) as u32,
            AOE_HOOK_FALLTHROUGH_ADDR
        );

        let magic_cmp = sc[sc.magic_aoe_entry_offset..]
            .windows(MAGIC_AOE_ORIGINAL_BYTES.len())
            .position(|w| w == MAGIC_AOE_ORIGINAL_BYTES)
            .map(|p| sc.magic_aoe_entry_offset + p)
            .expect("missing replayed magic aoe cmp");
        let magic_jmp_rel_offset = magic_cmp + MAGIC_AOE_ORIGINAL_BYTES.len() + 1;
        let magic_jmp_rel = i32::from_le_bytes(
            sc[magic_jmp_rel_offset..magic_jmp_rel_offset + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(
            (cave as i64 + magic_jmp_rel_offset as i64 + 4 + magic_jmp_rel as i64) as u32,
            MAGIC_AOE_HOOK_FALLTHROUGH_ADDR
        );
    }

    #[test]
    fn shellcode_reads_attack_aoe_and_magic_aoe_damage_sources() {
        let sc = build_shellcode(0x2000_0000, 0x7700_1234);
        assert!(sc.windows(3).any(|w| w == [0x8B, 0x55, 0xEC]));
        assert!(sc.windows(3).any(|w| w == [0x8B, 0x4D, 0xF0]));
        assert!(sc.windows(4).any(|w| w == [0x0F, 0xB7, 0x55, 0xCC]));
        assert!(!sc.windows(3).any(|w| w == [0x8B, 0x55, 0xE0]));

        assert!(sc.windows(3).any(|w| w == [0x8B, 0x55, 0xF0]));
        assert!(sc.windows(3).any(|w| w == [0x8B, 0x75, 0xE4]));
        assert!(sc.windows(4).any(|w| w == [0x0F, 0xB7, 0x45, 0xE0]));
        assert!(sc.windows(4).any(|w| w == [0x0F, 0xB7, 0x04, 0x58]));

        assert!(sc.windows(3).any(|w| w == [0x8B, 0x55, 0xE4]));
        assert!(sc.windows(3).any(|w| w == [0x8B, 0x75, 0xBC]));
        assert!(sc.windows(4).any(|w| w == [0x0F, 0xB7, 0x45, 0xB4]));
    }

    #[test]
    fn magic_aoe_calls_overhead_text_fn() {
        let sc = build_shellcode(0x2000_0000, 0x7700_1234);
        let magic_replay = sc[sc.magic_aoe_entry_offset..]
            .windows(MAGIC_AOE_ORIGINAL_BYTES.len())
            .position(|w| w == MAGIC_AOE_ORIGINAL_BYTES)
            .map(|p| sc.magic_aoe_entry_offset + p)
            .expect("missing replayed magic aoe cmp");
        let magic_branch = &sc[sc.magic_aoe_entry_offset..magic_replay];

        assert!(magic_branch
            .windows(5)
            .any(|w| w == [0xB8, 0xB0, 0xB7, 0x42, 0x00]));
    }

    #[test]
    fn shellcode_accepts_known_local_player_id_mirrors() {
        let sc = build_shellcode(0x2000_0000, 0x7700_1234);
        assert!(sc.windows(4).any(|w| w == SELF_CHAR_ID_ADDR.to_le_bytes()));
        assert!(sc
            .windows(3)
            .any(|w| w == [0x8B, 0x48, LOCAL_PLAYER_ID_OFFSET]));
        assert!(sc
            .windows(3)
            .any(|w| w == [0x8B, 0x48, LOCAL_PLAYER_ALT_ID_OFFSET]));
    }

    #[test]
    fn shellcode_uses_expected_overhead_text_style() {
        let sc = build_shellcode(0x2000_0000, 0x7700_1234);
        assert!(sc.windows(5).any(|w| w == [0xB8, 0xB0, 0xB7, 0x42, 0x00]));
        assert!(sc
            .windows(6)
            .any(|w| w == [0x6A, 0x00, 0x6A, 0x00, 0x6A, 0x01]));
        assert!(sc
            .windows(5)
            .any(|w| w[0] == 0x68 && w[1..5] == DAMAGE_TEXT_COLOR.to_le_bytes()));
    }

    #[test]
    fn shellcode_formats_current_and_cumulative_damage() {
        let sc = build_shellcode(0x2000_0000, 0x7700_1234);
        // preamble: "\\fRf>( \\fRf0" (14 bytes via 4 mov writes)
        assert!(sc
            .windows(6)
            .any(|w| w == [0xC7, 0x07, 0x5C, 0x5C, 0x66, 0x52])); // "\\fR" @ [edi+0]
        assert!(sc
            .windows(7)
            .any(|w| w == [0xC7, 0x47, 0x04, 0x66, 0x3E, 0x28, 0x20])); // "f>( " @ [edi+4]
        assert!(sc
            .windows(7)
            .any(|w| w == [0xC7, 0x47, 0x08, 0x5C, 0x5C, 0x66, 0x52])); // "\\fR" @ [edi+8]
        assert!(sc
            .windows(6)
            .any(|w| w == [0x66, 0xC7, 0x47, 0x0C, 0x66, 0x30])); // "f0" word @ [edi+12]
        assert!(sc.windows(3).any(|w| w == [0x83, 0xC7, 0x0E])); // add edi, 14
        // closing escape "\\fRf> )" + ' ' (unchanged)
        assert!(sc
            .windows(7)
            .any(|w| w == [0xC7, 0x47, 0x04, 0x66, 0x3E, 0x20, 0x29]));
        assert!(sc.windows(4).any(|w| w == [0xC6, 0x47, 0x08, 0x20]));
        assert!(sc.windows(4).any(|w| w == b"2222"));
    }

    #[test]
    fn shellcode_preserves_ecx_and_edx_across_get_tick_count_call() {
        // ecx (target_id) and edx (current damage) are caller-save in stdcall.
        // kernel32!GetTickCount may clobber them, so we must push before / pop after,
        // otherwise the accumulator updates with garbage and inflates the displayed total.
        let sc = build_shellcode(0x2000_0000, 0x7700_1234);
        let call_idx = sc
            .windows(2)
            .position(|w| w == [0xFF, 0x15])
            .expect("missing call [gettickcount_ptr]");
        let pre = &sc[..call_idx];
        let pre_tail = &pre[pre.len().saturating_sub(2)..];
        assert!(
            pre_tail == [0x51, 0x52] || pre_tail == [0x52, 0x51],
            "expected push ecx/edx pair immediately before call [gettickcount_ptr], got {:02X?}",
            pre_tail
        );
        let post_start = call_idx + 6; // FF 15 + imm32
        let post = &sc[post_start..(post_start + 2).min(sc.len())];
        assert!(
            post == [0x5A, 0x59] || post == [0x59, 0x5A],
            "expected pop edx/ecx pair immediately after call [gettickcount_ptr], got {:02X?}",
            post
        );
    }

    #[test]
    fn shellcode_uses_single_slot_with_get_tick_count_gating() {
        let sc = build_shellcode(0x2000_0000, 0x7700_1234);

        // call dword [gettickcount_ptr]
        assert!(
            sc.windows(2).any(|w| w == [0xFF, 0x15]),
            "missing call [gettickcount_ptr]"
        );

        // baked GetTickCount address present in data section
        assert!(
            sc.windows(4).any(|w| w == 0x7700_1234u32.to_le_bytes()),
            "missing baked GetTickCount address"
        );

        // cmp ebx, ACCUMULATOR_TIMEOUT_MS  (81 FB <imm32>)
        let timeout_bytes = ACCUMULATOR_TIMEOUT_MS.to_le_bytes();
        assert!(
            sc.windows(6)
                .any(|w| w[0..2] == [0x81, 0xFB] && w[2..6] == timeout_bytes),
            "missing cmp ebx, ACCUMULATOR_TIMEOUT_MS"
        );

        // accumulate path: add [last_total], edx  (01 15 ...)
        assert!(
            sc.windows(2).any(|w| w == [0x01, 0x15]),
            "missing add [last_total], edx"
        );

        // reset path: mov [last_target], ecx  (89 0D)  + mov [last_total], edx (89 15)
        assert!(
            sc.windows(2).any(|w| w == [0x89, 0x0D]),
            "missing mov [last_target], ecx"
        );
        assert!(
            sc.windows(2).any(|w| w == [0x89, 0x15]),
            "missing mov [last_total], edx"
        );

        // 16-slot scan loop must be gone: lea esi, [esi+eax*8] (8D 34 C6)
        assert!(
            !sc.windows(3).any(|w| w == [0x8D, 0x34, 0xC6]),
            "old 16-slot table loop still present"
        );
    }

    #[test]
    fn shellcode_does_not_patch_low_level_name_draw_path() {
        let sc = build_shellcode(0x2000_0000, 0x7700_1234);
        assert!(!sc.windows(5).any(|w| w == [0xB8, 0x50, 0xF1, 0x46, 0x00]));
    }

    #[test]
    fn active_hook_sites_cover_single_and_extended_range_damage_paths() {
        assert_eq!(
            active_hook_sites(),
            &[ATTACK_HOOK_ADDR, MAGIC_AOE_EXT_DAMAGE_HOOK_ADDR]
        );
    }

    #[test]
    fn magic_aoe_ext_entry_consumes_damage_dword_before_replaying_original_parse_epilogue() {
        let sc = build_shellcode(0x2000_0000, 0x7700_1234);
        let ext_entry = &sc[sc.magic_aoe_ext_damage_entry_offset..];

        assert!(ext_entry.windows(2).any(|w| w == [0x8B, 0x10])); // mov edx, [eax]
        assert!(ext_entry.windows(3).any(|w| w == [0x83, 0xC0, 0x04])); // add eax, 4
        assert!(ext_entry
            .windows(MAGIC_AOE_EXT_DAMAGE_ORIGINAL_BYTES.len())
            .any(|w| w == MAGIC_AOE_EXT_DAMAGE_ORIGINAL_BYTES));
    }
}
