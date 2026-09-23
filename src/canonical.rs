//! Hashing and RFC 8785 canonical JSON.

use base64::Engine;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn sha1_hex(data: &[u8]) -> String {
    hex(&Sha1::digest(data))
}

pub fn sha512_base64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha512::digest(data))
}

pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Serialises a JSON value per RFC 8785 (JCS): sorted keys, no whitespace, ES6 number formatting.
pub fn canonical_bytes(value: &serde_json::Value) -> Vec<u8> {
    serde_json_canonicalizer::to_vec(value).expect("JSON value is always serialisable")
}

pub fn canonical_string(value: &serde_json::Value) -> String {
    serde_json_canonicalizer::to_string(value).expect("JSON value is always serialisable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(sha512_base64(b"abc").len(), 88);
    }

    #[test]
    fn canonical() {
        let value = serde_json::json!({"b": 1.0, "a": [ {"z": "x", "y": null} ], "c": 1e21});
        assert_eq!(canonical_string(&value), "{\"a\":[{\"y\":null,\"z\":\"x\"}],\"b\":1,\"c\":1e+21}");
    }
}
