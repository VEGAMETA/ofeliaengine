//! Invites as text (ADR 0106): a connect token, its zero padding cut, in base64url behind
//! a prefix. About 1.4 KB of text a player pastes, or a platform's invite carries.

use engine_net_transport::{CONNECT_TOKEN_BYTES, ConnectToken};

/// What every invite starts with (the format's version in it).
const PREFIX: &str = "ofelia1:";
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// `token` as an invite.
pub(crate) fn encode(token: &ConnectToken) -> String {
    let mut bytes = [0u8; CONNECT_TOKEN_BYTES];
    token.write(&mut bytes);
    let used = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let mut out = String::with_capacity(PREFIX.len() + used.div_ceil(3) * 4);
    out.push_str(PREFIX);
    for chunk in bytes[..used].chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        let chars = chunk.len() + 1;
        for k in 0..chars {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * k)) & 63) as usize]));
        }
    }
    out
}

/// The token of an invite.
pub(crate) fn decode(invite: &str) -> Result<ConnectToken, String> {
    let body = invite
        .trim()
        .strip_prefix(PREFIX)
        .ok_or("not an invite of this game's engine")?;
    let mut bytes = [0u8; CONNECT_TOKEN_BYTES];
    let mut len = 0usize;
    let value = |c: u8| -> Result<u32, String> {
        ALPHABET
            .iter()
            .position(|&a| a == c)
            .map(|v| v as u32)
            .ok_or_else(|| format!("a character `{}` no invite has", char::from(c)))
    };
    for chunk in body.as_bytes().chunks(4) {
        if chunk.len() < 2 {
            return Err(String::from("an invite cut short"));
        }
        let mut n = 0u32;
        for (k, &c) in chunk.iter().enumerate() {
            n |= value(c)? << (18 - 6 * k);
        }
        for i in 0..chunk.len() - 1 {
            let slot = bytes
                .get_mut(len)
                .ok_or_else(|| String::from("an invite too long"))?;
            *slot = (n >> (16 - 8 * i)) as u8;
            len += 1;
        }
    }
    ConnectToken::read(&bytes).map_err(|e| format!("a damaged invite: {e:?}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use engine_net_transport::{Key, TokenParams, USER_DATA_BYTES, unix_now};

    #[test]
    fn an_invite_round_trips_and_damage_is_refused() {
        let key = Key::random();
        let addresses = [
            "127.0.0.1:27015".parse().unwrap(),
            "192.168.1.20:27015".parse().unwrap(),
        ];
        let token = ConnectToken::generate(
            &TokenParams {
                protocol_id: 7,
                client_id: 42,
                expire_seconds: 30,
                timeout_seconds: 10,
                server_addresses: &addresses,
                user_data: [0; USER_DATA_BYTES],
            },
            &key,
            unix_now(),
        )
        .unwrap();
        let invite = encode(&token);
        assert!(invite.starts_with(PREFIX));
        assert!(invite.len() < 2_000, "{}", invite.len());
        let back = decode(&format!("  {invite}\n")).unwrap();
        let (mut a, mut b) = ([0u8; CONNECT_TOKEN_BYTES], [0u8; CONNECT_TOKEN_BYTES]);
        token.write(&mut a);
        back.write(&mut b);
        assert_eq!(a, b);
        assert!(decode("ofelia1:").is_err());
        assert!(decode("hello").is_err());
        assert!(decode(&invite.replace('A', "*")).is_err() || !invite.contains('A'));
        assert!(decode(&invite[..invite.len() / 2]).is_err());
    }
}
