//! JSON spellings the generated code relies on: enums by name, bytes as
//! base64 (RFC 4648, padded).

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serializer};

/// Accept an enum as its name or, from a newer peer, its number.
pub fn enum_value<'de, D: Deserializer<'de>>(
    d: D,
    by_name: impl Fn(&str) -> Option<i32>,
) -> Result<i32, D::Error> {
    struct V<F>(F);
    impl<F: Fn(&str) -> Option<i32>> Visitor<'_> for V<F> {
        type Value = i32;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an enum name or number")
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<i32, E> {
            (self.0)(v).ok_or_else(|| E::custom(format!("unknown enum value `{v}`")))
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<i32, E> {
            i32::try_from(v).map_err(E::custom)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<i32, E> {
            i32::try_from(v).map_err(E::custom)
        }
    }
    d.deserialize_any(V(by_name))
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn decode_base64(text: &str) -> Result<Vec<u8>, String> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.bytes() {
        let v = ALPHABET
            .iter()
            .position(|&a| a == c)
            .ok_or_else(|| format!("invalid base64 character {:?}", c as char))?;
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if bits >= 6 {
        return Err("truncated base64".into());
    }
    Ok(out)
}

pub mod base64 {
    use super::*;

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&encode_base64(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        decode_base64(&s).map_err(de::Error::custom)
    }
}

pub mod base64_option {
    use super::*;

    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(b) => s.serialize_str(&encode_base64(b)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|s| decode_base64(&s).map_err(de::Error::custom))
            .transpose()
    }
}

pub mod base64_list {
    use super::*;
    use serde::ser::SerializeSeq;

    pub fn serialize<S: Serializer>(v: &[Vec<u8>], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(v.len()))?;
        for b in v {
            seq.serialize_element(&encode_base64(b))?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Vec<u8>>, D::Error> {
        Vec::<String>::deserialize(d)?
            .iter()
            .map(|s| decode_base64(s).map_err(de::Error::custom))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (raw, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode_base64(raw.as_bytes()), enc);
            assert_eq!(decode_base64(enc).expect("decode"), raw.as_bytes());
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode_base64(&encode_base64(&all)).expect("decode"), all);
        assert!(decode_base64("Zm9v!").is_err());
        assert!(decode_base64("Z").is_err());
    }
}
