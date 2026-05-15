//! Packet box sub-id → Notification 解析。

use super::types::{FloatKind, Notification};

/// 解析 PACKETBOX payload(從 sub_id byte 開始)。
/// 不認識的 sub_id / 越界 / 非法 → None。
pub fn parse_packet_box(payload: &[u8]) -> Option<Notification> {
    let (&sub_id, rest) = payload.split_first()?;
    match sub_id {
        190 => parse_item_board(rest),
        192 => parse_show_drop(rest),
        _ => None,
    }
}

fn parse_item_board(p: &[u8]) -> Option<Notification> {
    if p.len() < 3 {
        return None;
    }
    let gfxid = u16::from_le_bytes([p[0], p[1]]);
    let rest = &p[2..];
    let null_pos = rest.iter().position(|&b| b == 0)?;
    let take = null_pos.min(MAX_NAME_LEN);
    Some(Notification::ToastBottomLeft {
        gfxid,
        text: rest[..take].to_vec(),
    })
}

fn parse_show_drop(p: &[u8]) -> Option<Notification> {
    if p.len() < 5 {
        return None;
    }
    let kind = match p[0] {
        0 => FloatKind::Exp,
        1 => FloatKind::Gold,
        _ => return None,
    };
    let amount = u32::from_le_bytes([p[1], p[2], p[3], p[4]]);
    Some(Notification::FloatingScreen { kind, amount })
}

/// `name` 上限 byte 數(防 server 灌爆)。
const MAX_NAME_LEN: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_payload_is_none() {
        assert_eq!(parse_packet_box(&[]), None);
    }

    #[test]
    fn unknown_sub_id_is_none() {
        assert_eq!(parse_packet_box(&[100, 0x01, 0x02]), None);
    }

    #[test]
    fn item_board_basic() {
        // sub=190, gfxid=0x1234 (LE), name="ABC"+null
        let bytes = [190u8, 0x34, 0x12, b'A', b'B', b'C', 0x00];
        let n = parse_packet_box(&bytes).expect("應解析成功");
        assert_eq!(
            n,
            Notification::ToastBottomLeft {
                gfxid: 0x1234,
                text: b"ABC".to_vec(),
            }
        );
    }

    #[test]
    fn item_board_too_short_is_none() {
        // sub=190, 只有 1 byte(沒有 gfxid 完整)
        assert_eq!(parse_packet_box(&[190u8, 0x01]), None);
    }

    #[test]
    fn item_board_no_terminator_is_none() {
        // sub=190, gfxid 完整,但 name 沒有 0x00 terminator
        let bytes = [190u8, 0x00, 0x00, b'A', b'B'];
        assert_eq!(parse_packet_box(&bytes), None);
    }

    #[test]
    fn item_board_name_clamped_to_max() {
        // 100 byte name,應 clamp 到 64
        let mut bytes = vec![190u8, 0x00, 0x00];
        bytes.extend(std::iter::repeat(b'X').take(100));
        bytes.push(0x00);
        let n = parse_packet_box(&bytes).expect("應解析成功");
        if let Notification::ToastBottomLeft { text, .. } = n {
            assert_eq!(text.len(), MAX_NAME_LEN);
            assert_eq!(text, vec![b'X'; MAX_NAME_LEN]);
        } else {
            panic!("預期 ToastBottomLeft");
        }
    }

    #[test]
    fn show_drop_exp() {
        // sub=192, type=0 (EXP), amount=6260 (LE = 0x00001874)
        let bytes = [192u8, 0x00, 0x74, 0x18, 0x00, 0x00];
        assert_eq!(
            parse_packet_box(&bytes),
            Some(Notification::FloatingScreen {
                kind: FloatKind::Exp,
                amount: 6260,
            })
        );
    }

    #[test]
    fn show_drop_gold() {
        // sub=192, type=1 (Gold), amount=79
        let bytes = [192u8, 0x01, 0x4F, 0x00, 0x00, 0x00];
        assert_eq!(
            parse_packet_box(&bytes),
            Some(Notification::FloatingScreen {
                kind: FloatKind::Gold,
                amount: 79,
            })
        );
    }

    #[test]
    fn show_drop_invalid_type_is_none() {
        // sub=192, type=99(無效)
        let bytes = [192u8, 99, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(parse_packet_box(&bytes), None);
    }

    #[test]
    fn show_drop_too_short_is_none() {
        assert_eq!(parse_packet_box(&[192u8, 0x00, 0x01, 0x02]), None); // amount 缺 1 byte
    }
}
