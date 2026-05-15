use anyhow::Result;
use windows::Win32::Foundation::HANDLE;

use crate::aux::address::{
    CURRENT_HP_OBJ, CURRENT_MP_OBJ, CURRENT_WEIGHT, FOOD_LEVEL, FOOD_LEVEL_DIVISOR, G_GAME_STATE,
    G_MAP_ID, MAX_HP, MAX_MP, STAT_XOR_MAGIC, WEIGHT_DIVISOR,
};
use crate::memory::{read_bytes, read_u32};

#[derive(Default, Clone, Copy, Debug)]
pub struct PlayerState {
    pub hp: u32,
    pub max_hp: u32,
    pub mp: u32,
    pub max_mp: u32,
    pub food: u8,
    pub weight: u8,
    pub food_raw: u8,
    pub weight_raw: u8,
    pub map_id: u32,
}

fn read_byte(h: HANDLE, addr: u32) -> Result<u8> {
    Ok(read_bytes(h, addr, 1)?[0])
}

pub fn decode_xor_stat(h: HANDLE, obj_addr: u32) -> Result<u32> {
    let enc_idx = read_u32(h, obj_addr)?;
    let key_ptr = read_u32(h, obj_addr + 4)?;
    let salt = read_u32(h, obj_addr + 8)?;
    let plain_idx = enc_idx ^ STAT_XOR_MAGIC;
    if plain_idx >= 16 {
        anyhow::bail!("0x{obj_addr:08X} decoded plain_idx={plain_idx} out of range");
    }
    let enc_value = read_u32(h, key_ptr + plain_idx * 4)?;
    Ok(enc_value ^ salt)
}

pub fn read_player_state(h: HANDLE) -> Result<PlayerState> {
    if read_u32(h, G_GAME_STATE)? != 3 {
        return Ok(PlayerState::default());
    }

    let max_hp = read_u32(h, MAX_HP)?;
    let max_mp = read_u32(h, MAX_MP)?;
    let hp = decode_xor_stat(h, CURRENT_HP_OBJ).unwrap_or(0);
    let mp = decode_xor_stat(h, CURRENT_MP_OBJ).unwrap_or(0);
    let food_raw = FOOD_LEVEL.and_then(|a| read_byte(h, a).ok()).unwrap_or(0);
    let weight_raw = CURRENT_WEIGHT
        .and_then(|a| read_byte(h, a).ok())
        .unwrap_or(0);
    let map_id = read_u32(h, G_MAP_ID).unwrap_or(0);

    Ok(PlayerState {
        hp,
        max_hp,
        mp,
        max_mp,
        food: ((food_raw as u32) * 100 / FOOD_LEVEL_DIVISOR) as u8,
        weight: ((weight_raw as u32) * 100 / WEIGHT_DIVISOR) as u8,
        food_raw,
        weight_raw,
        map_id,
    })
}
