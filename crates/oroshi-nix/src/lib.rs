#![forbid(unsafe_code)]

use std::fmt;

mod narinfo;
pub use narinfo::{MAX_NARINFO_BYTES, NarInfo};
mod signature;
pub use signature::{PublicKey, Signature};

const BASE32: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError(&'static str);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ParseError {}

/// Encode bytes using Nix's alphabet and most-significant group first,
/// treating the input bytes as a little-endian integer.
pub fn encode_base32(bytes: &[u8]) -> String {
    let len = (bytes.len() * 8).div_ceil(5);
    let mut encoded = String::with_capacity(len);
    for n in (0..len).rev() {
        let bit = n * 5;
        let i = bit / 8;
        let pair = u16::from(bytes[i]) | (u16::from(*bytes.get(i + 1).unwrap_or(&0)) << 8);
        encoded.push(BASE32[usize::from((pair >> (bit % 8)) & 31)] as char);
    }
    encoded
}

/// Decode a canonical Nix base32 string, rejecting invalid lengths and nonzero padding bits.
pub fn decode_base32(encoded: &str) -> Result<Vec<u8>, ParseError> {
    let len = encoded.len() * 5 / 8;
    if (len * 8).div_ceil(5) != encoded.len() {
        return Err(ParseError("invalid Nix base32 length"));
    }
    let mut bytes = vec![0; len];
    for (n, c) in encoded.bytes().rev().enumerate() {
        let digit = BASE32
            .iter()
            .position(|&value| value == c)
            .ok_or(ParseError("invalid Nix base32 character"))?;
        let bit = n * 5;
        let i = bit / 8;
        let value = (digit as u16) << (bit % 8);
        bytes[i] |= value as u8;
        if i + 1 < len {
            bytes[i + 1] |= (value >> 8) as u8;
        } else if value >> 8 != 0 {
            return Err(ParseError("nonzero Nix base32 padding bits"));
        }
    }
    Ok(bytes)
}

/// Parse the `sha256:<52 Nix base32 characters>` form used in narinfo hashes.
pub fn parse_sha256(hash: &str) -> Result<[u8; 32], ParseError> {
    let encoded = hash
        .strip_prefix("sha256:")
        .ok_or(ParseError("expected sha256: hash prefix"))?;
    if encoded.len() != 52 {
        return Err(ParseError("SHA-256 requires 52 Nix base32 characters"));
    }
    decode_base32(encoded)?
        .try_into()
        .map_err(|_| ParseError("SHA-256 requires 32 bytes"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorePath<'a> {
    pub hash: &'a str,
    pub name: &'a str,
}

/// Parse an exact `/nix/store/<32-character hash>-<name>` path without filesystem access.
pub fn parse_store_path(path: &str) -> Result<StorePath<'_>, ParseError> {
    let base = path
        .strip_prefix("/nix/store/")
        .ok_or(ParseError("expected /nix/store/ prefix"))?;
    let (hash, name) = base
        .split_once('-')
        .ok_or(ParseError("expected store path hash-name separator"))?;
    if hash.len() != 32 {
        return Err(ParseError("store path hash must have 32 characters"));
    }
    if !hash.bytes().all(|c| BASE32.contains(&c)) {
        return Err(ParseError("invalid store path hash character"));
    }
    if name.is_empty() || name.len() > 211 {
        return Err(ParseError("store path name must have 1 to 211 bytes"));
    }
    // Match Nix 2.34.8 checkName: the first dash-separated component cannot be `.` or `..`.
    if matches!(name.split('-').next(), Some("." | "..")) {
        return Err(ParseError("invalid store path name component"));
    }
    if !name
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || b"+-._?=".contains(&c))
    {
        return Err(ParseError("invalid store path name character"));
    }
    Ok(StorePath { hash, name })
}
