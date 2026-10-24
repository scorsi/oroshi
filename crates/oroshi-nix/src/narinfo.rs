use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use crate::{ParseError, decode_base32, encode_base32, parse_sha256, parse_store_path};

/// Largest narinfo accepted on upload and stored after a signature merge (ADR 0015, 0016).
pub const MAX_NARINFO_BYTES: usize = 64 * 1024;

/// StorePath, URL, NarHash and NarSize are required; other fields are optional.
/// Sig and CA values are preserved as text; signature verification is separate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarInfo {
    pub store_path: String,
    pub url: String,
    pub compression: Option<String>,
    pub file_hash: Option<[u8; 32]>,
    pub file_size: Option<u64>,
    pub nar_hash: [u8; 32],
    pub nar_size: u64,
    /// Full store paths, sorted as in Nix's reference set; self-references are valid.
    pub references: BTreeSet<String>,
    pub deriver: Option<String>,
    pub signatures: Vec<String>,
    pub ca: Option<String>,
}

fn number(value: &str) -> Result<u64, ParseError> {
    if value.is_empty() || !value.bytes().all(|c| c.is_ascii_digit()) {
        return Err(ParseError("expected unsigned narinfo size"));
    }
    value
        .parse()
        .map_err(|_| ParseError("narinfo size overflow"))
}

fn full_path(base: &str) -> Result<String, ParseError> {
    let path = format!("/nix/store/{base}");
    parse_store_path(&path)?;
    Ok(path)
}

/// `text:sha256:<hash>` or `fixed:[r:|git:]<algo>:<hash>`, the hash in Nix base32,
/// as Nix renders a content address in a narinfo.
fn content_address(value: &str) -> bool {
    let hash = if let Some(hash) = value.strip_prefix("text:") {
        hash.strip_prefix("sha256:").map(|hash| (hash, 32))
    } else if let Some(rest) = value.strip_prefix("fixed:") {
        let rest = rest
            .strip_prefix("r:")
            .or_else(|| rest.strip_prefix("git:"))
            .unwrap_or(rest);
        rest.split_once(':').and_then(|(algo, hash)| {
            let bytes = match algo {
                "md5" => 16,
                "sha1" => 20,
                "sha256" => 32,
                "sha512" => 64,
                _ => return None,
            };
            Some((hash, bytes))
        })
    } else {
        None
    };
    hash.is_some_and(|(hash, bytes)| {
        hash.len() == (bytes * 8usize).div_ceil(5)
            && decode_base32(hash).is_ok_and(|decoded| decoded.len() == bytes)
    })
}

impl FromStr for NarInfo {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let mut fields = BTreeMap::new();
        let mut signatures = Vec::new();
        for line in input.lines() {
            let (key, value) = line
                .split_once(": ")
                .ok_or(ParseError("expected narinfo field: value"))?;
            if value.chars().any(char::is_control) {
                return Err(ParseError("control character in narinfo value"));
            }
            if key == "Sig" {
                if value.is_empty() {
                    return Err(ParseError("empty narinfo signature"));
                }
                signatures.push(value.to_owned());
                continue;
            }
            // The four Nix 2.34.8 fixtures use only these fields; refuse unknowns
            // rather than silently discard metadata (ADR 0006 leaves this choice open).
            if !matches!(
                key,
                "StorePath"
                    | "URL"
                    | "Compression"
                    | "FileHash"
                    | "FileSize"
                    | "NarHash"
                    | "NarSize"
                    | "References"
                    | "Deriver"
                    | "CA"
            ) {
                return Err(ParseError("unknown narinfo field"));
            }
            if fields.insert(key, value).is_some() {
                return Err(ParseError("duplicate single-valued narinfo field"));
            }
            if value.is_empty() && key != "References" {
                return Err(ParseError("empty narinfo field"));
            }
        }
        let store_path = fields
            .remove("StorePath")
            .ok_or(ParseError("missing StorePath"))?;
        parse_store_path(store_path)?;
        let url = fields.remove("URL").ok_or(ParseError("missing URL"))?;
        let nar_hash = parse_sha256(
            fields
                .remove("NarHash")
                .ok_or(ParseError("missing NarHash"))?,
        )?;
        let nar_size = number(
            fields
                .remove("NarSize")
                .ok_or(ParseError("missing NarSize"))?,
        )?;
        let references = fields
            .remove("References")
            .unwrap_or("")
            .split_whitespace()
            .map(full_path)
            .collect::<Result<_, _>>()?;
        let deriver = fields.remove("Deriver").map(full_path).transpose()?;
        Ok(Self {
            store_path: store_path.to_owned(),
            url: url.to_owned(),
            compression: fields.remove("Compression").map(str::to_owned),
            file_hash: fields.remove("FileHash").map(parse_sha256).transpose()?,
            file_size: fields.remove("FileSize").map(number).transpose()?,
            nar_hash,
            nar_size,
            references,
            deriver,
            signatures,
            ca: fields.remove("CA").map(str::to_owned),
        })
    }
}

impl NarInfo {
    /// Ingest rules for the unsigned `Compression` and `CA` fields (ADR 0016).
    /// An absent `Compression` means bzip2, as Nix reads it.
    pub fn check_unsigned_fields(&self) -> Result<(), ParseError> {
        let suffix = match self.compression.as_deref().unwrap_or("bzip2") {
            "none" => ".nar",
            "xz" => ".nar.xz",
            "zstd" => ".nar.zst",
            "bzip2" => ".nar.bz2",
            _ => return Err(ParseError("unsupported narinfo compression")),
        };
        if !self.url.ends_with(suffix) {
            return Err(ParseError("compression does not match the URL suffix"));
        }
        match &self.ca {
            Some(ca) if !content_address(ca) => Err(ParseError("malformed content address")),
            _ => Ok(()),
        }
    }

    /// Nix's version-1 signing payload, without a trailing newline.
    pub fn fingerprint(&self) -> String {
        format!(
            "1;{};sha256:{};{};{}",
            self.store_path,
            encode_base32(&self.nar_hash),
            self.nar_size,
            self.references
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

/// Canonical Nix field order, sorted reference basenames, and a final newline.
/// Missing References becomes an empty line; other optional fields stay absent.
impl fmt::Display for NarInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "StorePath: {}", self.store_path)?;
        writeln!(f, "URL: {}", self.url)?;
        if let Some(value) = &self.compression {
            writeln!(f, "Compression: {value}")?;
        }
        if let Some(value) = &self.file_hash {
            writeln!(f, "FileHash: sha256:{}", encode_base32(value))?;
        }
        if let Some(value) = self.file_size {
            writeln!(f, "FileSize: {value}")?;
        }
        writeln!(f, "NarHash: sha256:{}", encode_base32(&self.nar_hash))?;
        writeln!(f, "NarSize: {}", self.nar_size)?;
        write!(f, "References: ")?;
        for (i, path) in self.references.iter().enumerate() {
            if i != 0 {
                write!(f, " ")?;
            }
            write!(f, "{}", path.strip_prefix("/nix/store/").unwrap_or(path))?;
        }
        writeln!(f)?;
        if let Some(value) = &self.deriver {
            writeln!(
                f,
                "Deriver: {}",
                value.strip_prefix("/nix/store/").unwrap_or(value)
            )?;
        }
        for value in &self.signatures {
            writeln!(f, "Sig: {value}")?;
        }
        if let Some(value) = &self.ca {
            writeln!(f, "CA: {value}")?;
        }
        Ok(())
    }
}
