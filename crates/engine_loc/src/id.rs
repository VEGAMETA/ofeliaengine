//! Message identifiers: a name and its 64-bit hash, the key tables are looked up by.

/// FNV-1a over `bytes` (64-bit): the hash of identifiers, attribute names and variant keys,
/// the same at compile time ([`crate::loc!`]) and in the cook.
pub const fn hash(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut i = 0;
    while i < bytes.len() {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    h
}

/// A message (or one of its attributes) to format: what [`crate::loc!`] makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageId {
    /// The hash of the message's identifier.
    pub hash: u64,
    /// The hash of the attribute's name; 0 for the message's value.
    pub attribute: u64,
    /// The identifier as written, for what is shown when the message is missing.
    pub name: &'static str,
}

impl MessageId {
    /// The value of the message `name`.
    pub const fn new(name: &'static str) -> Self {
        Self {
            hash: hash(name.as_bytes()),
            attribute: 0,
            name,
        }
    }

    /// The attribute `attribute` of the message `name` (`menu-start.tooltip`).
    pub const fn attribute(name: &'static str, attribute: &'static str) -> Self {
        Self {
            hash: hash(name.as_bytes()),
            attribute: hash(attribute.as_bytes()),
            name,
        }
    }
}

/// A message's identifier hashed at compile time: `loc!("items")`, or an attribute,
/// `loc!("menu-start", "tooltip")`.
///
/// ```
/// use engine_loc::{MessageId, loc};
///
/// const ITEMS: MessageId = loc!("items");
/// assert_eq!(ITEMS, MessageId::new("items"));
/// assert_ne!(loc!("menu-start", "tooltip").attribute, 0);
/// ```
#[macro_export]
macro_rules! loc {
    ($name:literal) => {
        const { $crate::MessageId::new($name) }
    };
    ($name:literal, $attribute:literal) => {
        const { $crate::MessageId::attribute($name, $attribute) }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_fnv1a() {
        // The reference values of FNV-1a 64.
        assert_eq!(hash(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(hash(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(hash(b"foobar"), 0x8594_4171_f739_67e8);
        const ID: MessageId = crate::loc!("items");
        assert_eq!(ID.hash, hash(b"items"));
        assert_eq!(ID.attribute, 0);
    }
}
