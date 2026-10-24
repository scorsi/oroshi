use std::str::FromStr;

use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::VerifyingKey;

use crate::{NarInfo, ParseError};

#[derive(Debug, Clone)]
pub struct PublicKey {
    name: String,
    key: VerifyingKey,
}

#[derive(Debug, Clone)]
pub struct Signature {
    name: String,
    signature: ed25519_dalek::Signature,
}

fn decode<const N: usize>(input: &str) -> Result<(String, [u8; N]), ParseError> {
    let (name, encoded) = input
        .split_once(':')
        .ok_or(ParseError("expected name:base64"))?;
    if name.is_empty()
        || name.chars().any(char::is_whitespace)
        || name.chars().any(char::is_control)
    {
        return Err(ParseError("invalid signature key name"));
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| ParseError("invalid key or signature base64"))?
        .try_into()
        .map_err(|_| ParseError("invalid key or signature length"))?;
    Ok((name.to_owned(), bytes))
}

impl FromStr for PublicKey {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let (name, bytes) = decode(input)?;
        let key = VerifyingKey::from_bytes(&bytes)
            .map_err(|_| ParseError("invalid ed25519 public key"))?;
        Ok(Self { name, key })
    }
}

impl FromStr for Signature {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let (name, bytes) = decode(input)?;
        Ok(Self {
            name,
            signature: ed25519_dalek::Signature::from_bytes(&bytes),
        })
    }
}

impl PublicKey {
    /// Verify the exact fingerprint bytes and the signature's key name.
    pub fn verify(&self, fingerprint: &str, signature: &Signature) -> bool {
        self.name == signature.name
            && self
                .key
                .verify_strict(fingerprint.as_bytes(), &signature.signature)
                .is_ok()
    }
}

impl NarInfo {
    /// Accept only if a signature verifies against a key parsed from `nix.trusted_keys`.
    /// Malformed signatures are refused even when another signature is valid.
    pub fn verify_signatures(&self, trusted_keys: &[PublicKey]) -> Result<(), ParseError> {
        let signatures = self
            .signatures
            .iter()
            .map(|value| value.parse::<Signature>())
            .collect::<Result<Vec<_>, _>>()?;
        let fingerprint = self.fingerprint();
        if signatures.iter().any(|signature| {
            trusted_keys
                .iter()
                .any(|key| key.verify(&fingerprint, signature))
        }) {
            Ok(())
        } else {
            Err(ParseError("no valid signature by a trusted key"))
        }
    }

    /// Keep only well-formed signatures that verify against `trusted_keys` (merge input, ADR 0017).
    pub fn retain_trusted_signatures(&mut self, trusted_keys: &[PublicKey]) {
        let fingerprint = self.fingerprint();
        self.signatures.retain(|value| {
            value.parse::<Signature>().is_ok_and(|signature| {
                trusted_keys
                    .iter()
                    .any(|key| key.verify(&fingerprint, &signature))
            })
        });
    }

    /// Union Sig lines in first-seen order, refusing any other metadata difference.
    pub fn merge_signatures(&mut self, other: &Self) -> Result<(), ParseError> {
        let mut metadata = other.clone();
        metadata.signatures.clone_from(&self.signatures);
        if *self != metadata {
            return Err(ParseError("conflicting narinfo metadata"));
        }
        let mut seen = std::collections::BTreeSet::new();
        self.signatures.extend(other.signatures.iter().cloned());
        self.signatures.retain(|value| seen.insert(value.clone()));
        Ok(())
    }
}
