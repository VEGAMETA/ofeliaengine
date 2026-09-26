//! One tick of action state as bytes: the user command a client sends to the server
//! (ADR 0019).
//!
//! Layout: the pressed flags as a bitset, then for every pressed action its value(s) and
//! press fraction as zigzag varints. Released actions cost one bit. Values are the
//! quantized fixed-point numbers the client simulated with, so the server reproduces
//! them exactly.

use crate::map::{ActionKind, InputMap};
use crate::state::{ActionState, NO_FRACTION};

/// Errors from [`ActionState::decode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The bytes end early.
    Truncated,
    /// A number does not fit.
    Overflow,
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(f, "input command is truncated"),
            Self::Overflow => write!(f, "input command holds an oversized number"),
        }
    }
}

impl std::error::Error for DecodeError {}

fn put_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(bytes: &[u8], at: &mut usize) -> Result<u32, DecodeError> {
    let mut v = 0u32;
    for shift in (0..35).step_by(7) {
        let b = *bytes.get(*at).ok_or(DecodeError::Truncated)?;
        *at += 1;
        if shift == 28 && b > 0x0F {
            return Err(DecodeError::Overflow);
        }
        v |= u32::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(DecodeError::Overflow)
}

fn zigzag(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

fn unzigzag(v: u32) -> i32 {
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

impl ActionState {
    /// Appends this state's encoding to `out`.
    pub fn encode(&self, map: &InputMap, out: &mut Vec<u8>) {
        let count = map.action_count();
        let words = self.pressed_words();
        for byte in 0..count.div_ceil(8) {
            out.push((words[byte / 8] >> ((byte % 8) * 8)) as u8);
        }
        for (a, action) in map.actions.iter().enumerate() {
            if words[a / 64] & (1 << (a % 64)) == 0 {
                continue;
            }
            let [x, y] = self.raw_value(a);
            put_varint(out, zigzag(x));
            if action.kind == ActionKind::Axis2d {
                put_varint(out, zigzag(y));
            }
            // NO_FRACTION (65535) encodes as 0, so held buttons cost one byte.
            put_varint(out, u32::from(self.raw_fraction(a).wrapping_add(1)));
        }
    }

    /// Replaces this state with one decoded from `bytes`; the current pressed flags
    /// become the previous ones, as sampling does. Returns the bytes read.
    pub fn decode(&mut self, map: &InputMap, bytes: &[u8]) -> Result<usize, DecodeError> {
        let count = map.action_count();
        self.resize(count);
        self.begin();
        let flag_bytes = count.div_ceil(8);
        let flags = bytes.get(..flag_bytes).ok_or(DecodeError::Truncated)?;
        let mut at = flag_bytes;
        for (a, action) in map.actions.iter().enumerate() {
            if flags[a / 8] & (1 << (a % 8)) == 0 {
                continue;
            }
            self.set_pressed(a, true);
            let x = unzigzag(get_varint(bytes, &mut at)?);
            let y = if action.kind == ActionKind::Axis2d {
                unzigzag(get_varint(bytes, &mut at)?)
            } else {
                0
            };
            self.set_value(a, [x, y]);
            let fraction = get_varint(bytes, &mut at)?;
            let fraction = u16::try_from(fraction).map_err(|_| DecodeError::Overflow)?;
            self.set_fraction(a, fraction.wrapping_sub(1));
            debug_assert!(fraction != 0 || self.raw_fraction(a) == NO_FRACTION);
        }
        Ok(at)
    }
}
