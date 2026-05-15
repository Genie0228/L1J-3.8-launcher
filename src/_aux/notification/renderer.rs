//! Render list 展開 + 序列化 + draw-loop shellcode emitter。
//!
//! v1 範圍(text-draw 不可用,Phase 2 Task 2.2 排除):
//! - Toast = 2 個 RenderEntry(bg PNG + item icon)
//! - Float = 1 個 RenderEntry(EXP/Gold icon,**沒數字**)
//!
//! 數字 / 物品名 文字渲染留 v2(需要 pre-baked 數字 PNG 或 GDI TextOut 路徑)。

use super::layout::{float_layout, toast_layout};
use super::types::{FloatKind, LiveFloat, LiveToast};

// ===== PNG gfxid 常數(對齊使用者已載入的 PNG)=====

pub const TOAST_BG_GFXID: u16 = 1888;
pub const TOAST_ICON_OFFSET_X: i32 = 4;
pub const TOAST_ICON_OFFSET_Y: i32 = 2;

pub const FLOAT_EXP_GFXID: u16 = 1971;
pub const FLOAT_GOLD_GFXID: u16 = 1973;

// ===== Game function & global addresses(Phase 2 RE 結果)=====

/// `surface_data* GetSurfResource(SurfManager*, u16 gfxid)` — thiscall(ECX=this)
pub const GET_SURF_RESOURCE: u32 = 0x00759F20;
/// PNG SurfManager 全域指標(放進 ECX 給 GetSurfResource)
pub const PNG_SURF_MANAGER: u32 = 0x00C31434;
/// `void blit_B(surface, src_x, src_y, dst_x, dst_y)` — cdecl 5 args
pub const BLIT_B: u32 = 0x00555CE0;

/// 單筆繪製指令 — 給 shellcode 消費的最小單位(packed 12 bytes)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawCmd {
    pub gfxid: u16,
    pub _pad: u16,
    pub x: i32,
    pub y: i32,
}

impl DrawCmd {
    pub fn new(gfxid: u16, x: i32, y: i32) -> Self {
        Self {
            gfxid,
            _pad: 0,
            x,
            y,
        }
    }
}

/// 把 queue snapshot 展開成 DrawCmd 陣列。
///
/// 輸入:active toasts / floats(來自 NotificationQueue)、螢幕尺寸、現在時間。
pub fn expand_snapshot(
    toasts: &[LiveToast],
    floats: &[LiveFloat],
    screen_w: i32,
    screen_h: i32,
    now: std::time::Instant,
) -> Vec<DrawCmd> {
    let mut out = Vec::with_capacity(toasts.len() * 2 + floats.len());

    for (slot, t) in toasts.iter().enumerate() {
        let l = toast_layout(t, slot, screen_w, screen_h, now);
        // bg
        out.push(DrawCmd::new(TOAST_BG_GFXID, l.x, l.y));
        // item icon
        if t.gfxid != 0 {
            out.push(DrawCmd::new(
                t.gfxid,
                l.x + TOAST_ICON_OFFSET_X,
                l.y + TOAST_ICON_OFFSET_Y,
            ));
        }
    }

    for f in floats {
        let l = float_layout(f, screen_w, screen_h, now);
        let gfxid = match f.kind {
            FloatKind::Exp => FLOAT_EXP_GFXID,
            FloatKind::Gold => FLOAT_GOLD_GFXID,
        };
        out.push(DrawCmd::new(gfxid, l.x, l.y));
    }

    out
}

/// Serialize render list 給 shellcode 用。
///
/// Format: `[count: u32 LE][DrawCmd × N]`,DrawCmd = 12 bytes packed
/// (`gfxid:u16, _pad:u16, x:i32, y:i32`)。
pub fn serialize(cmds: &[DrawCmd]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + cmds.len() * 12);
    buf.extend_from_slice(&(cmds.len() as u32).to_le_bytes());
    for c in cmds {
        buf.extend_from_slice(&c.gfxid.to_le_bytes());
        buf.extend_from_slice(&c._pad.to_le_bytes());
        buf.extend_from_slice(&c.x.to_le_bytes());
        buf.extend_from_slice(&c.y.to_le_bytes());
    }
    buf
}

/// 反序列化(testing/debugging 用)。
pub fn deserialize(buf: &[u8]) -> Option<Vec<DrawCmd>> {
    if buf.len() < 4 {
        return None;
    }
    let count = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + count * 12 {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = 4 + i * 12;
        out.push(DrawCmd {
            gfxid: u16::from_le_bytes([buf[off], buf[off + 1]]),
            _pad: u16::from_le_bytes([buf[off + 2], buf[off + 3]]),
            x: i32::from_le_bytes([
                buf[off + 4],
                buf[off + 5],
                buf[off + 6],
                buf[off + 7],
            ]),
            y: i32::from_le_bytes([
                buf[off + 8],
                buf[off + 9],
                buf[off + 10],
                buf[off + 11],
            ]),
        });
    }
    Some(out)
}

/// 編出 draw-loop shellcode(position-independent — call 用 register-indirect)。
///
/// Register layout(進入後 pushad/pushfd,出來前 popfd/popad,呼叫端不受影響):
/// - `esi` = 當前 entry 指標
/// - `edi` = 剩餘 entry 數
/// - `ecx` / `eax` = 呼叫 scratch
///
/// 流程:
/// ```pseudo
/// pushad / pushfd
/// esi = list_addr
/// edi = [esi]            ; count
/// add esi, 4
/// .loop:
///   test edi, edi; jz .end
///   movzx eax, word [esi]  ; gfxid
///   push eax                ; arg
///   mov ecx, PNG_SURF_MANAGER
///   mov eax, GET_SURF_RESOURCE
///   call eax                  ; eax = surface (NULL or valid),callee 清 stack
///   test eax, eax; jz .skip
///   push [esi+8]; push [esi+4]; push 0; push 0; push eax
///   mov ecx, BLIT_B
///   call ecx
///   add esp, 0x14
/// .skip:
///   add esi, 12
///   dec edi
///   jmp .loop
/// .end:
/// popfd / popad / ret
/// ```
pub fn build_draw_loop_shellcode(list_addr: u32) -> Vec<u8> {
    let mut sc = Vec::with_capacity(96);

    // pushad / pushfd
    sc.push(0x60);
    sc.push(0x9C);

    // mov esi, list_addr
    sc.push(0xBE);
    sc.extend_from_slice(&list_addr.to_le_bytes());
    // mov edi, [esi] — count
    sc.extend_from_slice(&[0x8B, 0x3E]);
    // add esi, 4
    sc.extend_from_slice(&[0x83, 0xC6, 0x04]);

    let loop_start = sc.len();

    // test edi, edi
    sc.extend_from_slice(&[0x85, 0xFF]);
    // jz .end (rel32, patch later)
    sc.extend_from_slice(&[0x0F, 0x84]);
    let jz_end = sc.len();
    sc.extend_from_slice(&0i32.to_le_bytes());

    // === 取 surface ===
    // movzx eax, word ptr [esi]
    sc.extend_from_slice(&[0x0F, 0xB7, 0x06]);
    // push eax
    sc.push(0x50);
    // mov ecx, PNG_SURF_MANAGER
    sc.push(0xB9);
    sc.extend_from_slice(&PNG_SURF_MANAGER.to_le_bytes());
    // mov eax, GET_SURF_RESOURCE
    sc.push(0xB8);
    sc.extend_from_slice(&GET_SURF_RESOURCE.to_le_bytes());
    // call eax
    sc.extend_from_slice(&[0xFF, 0xD0]);
    // (thiscall callee 清 stack — 不需 add esp)

    // test eax, eax
    sc.extend_from_slice(&[0x85, 0xC0]);
    // jz .skip (rel8, patch later)
    sc.push(0x74);
    let jz_skip = sc.len();
    sc.push(0);

    // === blit_B(surface, 0, 0, x, y) cdecl 5-arg ===
    sc.extend_from_slice(&[0xFF, 0x76, 0x08]); // push [esi+8] (y)
    sc.extend_from_slice(&[0xFF, 0x76, 0x04]); // push [esi+4] (x)
    sc.extend_from_slice(&[0x6A, 0x00]); // push 0 (src_y)
    sc.extend_from_slice(&[0x6A, 0x00]); // push 0 (src_x)
    sc.push(0x50); // push eax (surface)
    // mov ecx, BLIT_B
    sc.push(0xB9);
    sc.extend_from_slice(&BLIT_B.to_le_bytes());
    // call ecx
    sc.extend_from_slice(&[0xFF, 0xD1]);
    // add esp, 0x14
    sc.extend_from_slice(&[0x83, 0xC4, 0x14]);

    // .skip:
    let skip_off = sc.len();
    sc[jz_skip] = (skip_off - jz_skip - 1) as u8;

    // add esi, 12
    sc.extend_from_slice(&[0x83, 0xC6, 0x0C]);
    // dec edi
    sc.push(0x4F);
    // jmp .loop (rel32)
    sc.push(0xE9);
    let next_ip = sc.len() + 4;
    let rel = (loop_start as i32) - (next_ip as i32);
    sc.extend_from_slice(&rel.to_le_bytes());

    // .end:
    let end_off = sc.len();
    let next_after_jz = jz_end + 4;
    sc[jz_end..jz_end + 4]
        .copy_from_slice(&((end_off as i32) - (next_after_jz as i32)).to_le_bytes());

    // popfd / popad / ret
    sc.push(0x9D);
    sc.push(0x61);
    sc.push(0xC3);

    sc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn dummy_toast(now: Instant, gfxid: u16) -> LiveToast {
        LiveToast {
            spawned_at: now,
            gfxid,
            text: vec![],
        }
    }

    fn dummy_float(now: Instant, kind: FloatKind, cascade: u8) -> LiveFloat {
        LiveFloat {
            spawned_at: now,
            kind,
            amount: 100,
            cascade_offset: cascade,
        }
    }

    // ===== expand_snapshot =====

    #[test]
    fn expand_empty_returns_empty() {
        let now = Instant::now();
        let cmds = expand_snapshot(&[], &[], 800, 600, now);
        assert!(cmds.is_empty());
    }

    #[test]
    fn expand_single_toast_emits_bg_plus_icon() {
        let now = Instant::now();
        let toasts = vec![dummy_toast(now, 1234)];
        let cmds = expand_snapshot(&toasts, &[], 800, 600, now);
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].gfxid, TOAST_BG_GFXID);
        assert_eq!(cmds[1].gfxid, 1234);
        // icon 在 bg 偏移
        assert_eq!(cmds[1].x - cmds[0].x, TOAST_ICON_OFFSET_X);
        assert_eq!(cmds[1].y - cmds[0].y, TOAST_ICON_OFFSET_Y);
    }

    #[test]
    fn expand_toast_with_zero_gfxid_skips_icon() {
        let now = Instant::now();
        let toasts = vec![dummy_toast(now, 0)];
        let cmds = expand_snapshot(&toasts, &[], 800, 600, now);
        assert_eq!(cmds.len(), 1); // 只有 bg
        assert_eq!(cmds[0].gfxid, TOAST_BG_GFXID);
    }

    #[test]
    fn expand_float_exp_uses_exp_gfxid() {
        let now = Instant::now();
        let floats = vec![dummy_float(now, FloatKind::Exp, 0)];
        let cmds = expand_snapshot(&[], &floats, 800, 600, now);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].gfxid, FLOAT_EXP_GFXID);
    }

    #[test]
    fn expand_float_gold_uses_gold_gfxid() {
        let now = Instant::now();
        let floats = vec![dummy_float(now, FloatKind::Gold, 0)];
        let cmds = expand_snapshot(&[], &floats, 800, 600, now);
        assert_eq!(cmds[0].gfxid, FLOAT_GOLD_GFXID);
    }

    #[test]
    fn expand_multiple_toasts_stack_with_slot_offset() {
        let now = Instant::now();
        let toasts = vec![dummy_toast(now, 1), dummy_toast(now, 2)];
        let cmds = expand_snapshot(&toasts, &[], 800, 600, now);
        assert_eq!(cmds.len(), 4);
        // 第二個 toast 的 bg 應比第一個 y 小(往上 stack)
        assert!(cmds[2].y < cmds[0].y);
    }

    #[test]
    fn expand_float_drifts_up_over_time() {
        let now = Instant::now();
        let floats = vec![dummy_float(now, FloatKind::Exp, 0)];
        let cmds_t0 = expand_snapshot(&[], &floats, 800, 600, now);
        let cmds_later = expand_snapshot(
            &[],
            &floats,
            800,
            600,
            now + Duration::from_millis(750),
        );
        assert!(cmds_later[0].y < cmds_t0[0].y);
    }

    #[test]
    fn expand_mixed_toasts_and_floats() {
        let now = Instant::now();
        let toasts = vec![dummy_toast(now, 1234)];
        let floats = vec![
            dummy_float(now, FloatKind::Exp, 0),
            dummy_float(now, FloatKind::Gold, 0),
        ];
        let cmds = expand_snapshot(&toasts, &floats, 800, 600, now);
        assert_eq!(cmds.len(), 4); // bg + icon + exp + gold
    }

    // ===== serialize / deserialize =====

    #[test]
    fn serialize_empty() {
        let buf = serialize(&[]);
        assert_eq!(buf, vec![0, 0, 0, 0]);
    }

    #[test]
    fn serialize_one_entry_byte_equal() {
        let cmds = vec![DrawCmd::new(1888, 10, 550)];
        let buf = serialize(&cmds);
        // count=1
        assert_eq!(&buf[..4], &[1, 0, 0, 0]);
        // gfxid=1888=0x0760
        assert_eq!(&buf[4..6], &[0x60, 0x07]);
        // pad
        assert_eq!(&buf[6..8], &[0, 0]);
        // x=10
        assert_eq!(&buf[8..12], &[10, 0, 0, 0]);
        // y=550=0x0226
        assert_eq!(&buf[12..16], &[0x26, 0x02, 0, 0]);
    }

    #[test]
    fn serialize_three_entries_size() {
        let cmds = vec![
            DrawCmd::new(1, 0, 0),
            DrawCmd::new(2, 0, 0),
            DrawCmd::new(3, 0, 0),
        ];
        let buf = serialize(&cmds);
        assert_eq!(buf.len(), 4 + 3 * 12);
    }

    #[test]
    fn round_trip_serialize_deserialize() {
        let cmds = vec![
            DrawCmd::new(1888, 10, 550),
            DrawCmd::new(1234, 14, 552),
            DrawCmd::new(1971, 460, 280),
        ];
        let buf = serialize(&cmds);
        let restored = deserialize(&buf).expect("decode");
        assert_eq!(restored, cmds);
    }

    #[test]
    fn deserialize_truncated_returns_none() {
        assert_eq!(deserialize(&[]), None);
        assert_eq!(deserialize(&[1, 0, 0, 0]), None); // count=1 but no entry
    }

    // ===== draw-loop shellcode =====

    #[test]
    fn shellcode_starts_with_pushad_pushfd() {
        let sc = build_draw_loop_shellcode(0x10000000);
        assert_eq!(sc[0], 0x60); // pushad
        assert_eq!(sc[1], 0x9C); // pushfd
    }

    #[test]
    fn shellcode_loads_list_addr_into_esi() {
        let sc = build_draw_loop_shellcode(0xDEADBEEF);
        // offset 2: BE EF BE AD DE = mov esi, 0xDEADBEEF
        assert_eq!(sc[2], 0xBE);
        assert_eq!(&sc[3..7], &0xDEADBEEF_u32.to_le_bytes());
    }

    #[test]
    fn shellcode_ends_with_popfd_popad_ret() {
        let sc = build_draw_loop_shellcode(0x10000000);
        let n = sc.len();
        assert_eq!(sc[n - 3], 0x9D); // popfd
        assert_eq!(sc[n - 2], 0x61); // popad
        assert_eq!(sc[n - 1], 0xC3); // ret
    }

    #[test]
    fn shellcode_fits_reasonable_size() {
        let sc = build_draw_loop_shellcode(0x10000000);
        assert!(sc.len() < 128, "shellcode {} bytes 過大", sc.len());
        assert!(sc.len() > 30, "shellcode {} bytes 過小,可能漏 emit", sc.len());
    }

    /// 驗證 loop back-jump:結尾的 `jmp .loop` rel32 應指回 `test edi, edi`。
    #[test]
    fn shellcode_loop_jmp_back_to_loop_start() {
        let sc = build_draw_loop_shellcode(0x10000000);
        // loop_start = pushad(1) + pushfd(1) + mov esi(5) + mov edi(2) + add esi(3) = 12
        let loop_start = 12;
        // Find the back-jump (E9 followed by negative rel32) — should be the second-to-last
        // before popfd/popad/ret. End block: 9D 61 C3 (3 bytes). jmp是其前 5 bytes。
        let end_block = sc.len() - 3;
        let jmp_at = end_block - 5;
        assert_eq!(sc[jmp_at], 0xE9, "expected E9 at offset {}", jmp_at);
        let rel = i32::from_le_bytes([
            sc[jmp_at + 1],
            sc[jmp_at + 2],
            sc[jmp_at + 3],
            sc[jmp_at + 4],
        ]);
        let target = (jmp_at + 5) as isize + rel as isize;
        assert_eq!(target as usize, loop_start);
    }

    /// 驗證 jz .end rel32 確實跳到 popfd 起點。
    #[test]
    fn shellcode_jz_end_targets_popfd() {
        let sc = build_draw_loop_shellcode(0x10000000);
        // jz .end @ offset loop_start + 2(test) = 14
        let jz_at = 14;
        assert_eq!(&sc[jz_at..jz_at + 2], &[0x0F, 0x84]);
        let rel = i32::from_le_bytes([
            sc[jz_at + 2],
            sc[jz_at + 3],
            sc[jz_at + 4],
            sc[jz_at + 5],
        ]);
        let target = (jz_at + 6) as isize + rel as isize;
        // target 應該是 popfd(0x9D)
        assert_eq!(sc[target as usize], 0x9D);
    }

    /// 驗證 jz .skip rel8 確實跳過 blit_B call(落在 add esi, 12 之前的 skip 點)。
    #[test]
    fn shellcode_jz_skip_bypasses_blit() {
        let sc = build_draw_loop_shellcode(0x10000000);
        // jz .skip 在 test eax, eax 之後 — 找 0x85 0xC0 0x74
        let mut jz_at = None;
        for i in 0..sc.len() - 3 {
            if sc[i] == 0x85 && sc[i + 1] == 0xC0 && sc[i + 2] == 0x74 {
                jz_at = Some(i + 2);
                break;
            }
        }
        let jz_at = jz_at.expect("jz .skip not found");
        let rel = sc[jz_at + 1] as i8 as isize;
        let target = (jz_at + 2) as isize + rel;
        // target 應落在 add esi, 12(83 C6 0C)
        assert_eq!(&sc[target as usize..target as usize + 3], &[0x83, 0xC6, 0x0C]);
    }

    /// 驗證 GetSurfResource thiscall pattern:`mov ecx, MGR; mov eax, GET; call eax`
    #[test]
    fn shellcode_thiscall_uses_correct_addresses() {
        let sc = build_draw_loop_shellcode(0x10000000);
        // 找 mov ecx, MGR(B9 + addr)
        let mut found = false;
        for i in 0..sc.len() - 12 {
            if sc[i] == 0xB9 {
                let mgr = u32::from_le_bytes([sc[i + 1], sc[i + 2], sc[i + 3], sc[i + 4]]);
                if mgr == PNG_SURF_MANAGER {
                    // 接下來應是 mov eax, GET_SURF(B8 + addr)
                    assert_eq!(sc[i + 5], 0xB8);
                    let target = u32::from_le_bytes([
                        sc[i + 6],
                        sc[i + 7],
                        sc[i + 8],
                        sc[i + 9],
                    ]);
                    assert_eq!(target, GET_SURF_RESOURCE);
                    // 然後是 call eax (FF D0)
                    assert_eq!(&sc[i + 10..i + 12], &[0xFF, 0xD0]);
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "thiscall pattern not emitted");
    }
}
