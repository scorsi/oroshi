use oroshi_nix::{decode_base32, encode_base32, parse_sha256, parse_store_path};

const STORE_HASH: &str = "0123456789abcdfghijklmnpqrsvwxyz";

#[test]
fn nix_generated_vectors() {
    for line in include_str!("fixtures/hashes.txt").lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        assert_eq!(fields.len(), 3);
        let hex = fields[1];
        let bytes: Vec<_> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(encode_base32(&bytes), fields[2]);
        assert_eq!(decode_base32(fields[2]).unwrap(), bytes);
        if fields[0] == "sha256" {
            assert_eq!(
                parse_sha256(&format!("sha256:{}", fields[2]))
                    .unwrap()
                    .as_slice(),
                bytes
            );
        }
    }
}

#[test]
fn base32_round_trips() {
    assert_eq!(encode_base32(&[]), "");
    assert_eq!(decode_base32("").unwrap(), []);
    for len in 0..=128 {
        for bytes in [
            vec![0; len],
            vec![255; len],
            (0..len).map(|i| (i * 37 + len) as u8).collect(),
        ] {
            assert_eq!(decode_base32(&encode_base32(&bytes)).unwrap(), bytes);
        }
    }
    for byte in 0..=255 {
        let encoded = encode_base32(&[byte]);
        assert_eq!(decode_base32(&encoded).unwrap(), [byte]);
    }
}

#[test]
fn base32_bit_order() {
    for (bytes, encoded) in [
        (&[0][..], "00"),
        (&[1], "01"),
        (&[255], "7z"),
        (&[1, 0], "0001"),
        (&[0, 1], "0080"),
        (&[255, 255], "1zzz"),
    ] {
        assert_eq!(encode_base32(bytes), encoded);
        assert_eq!(decode_base32(encoded).unwrap(), bytes);
    }
}

#[test]
fn base32_rejects_invalid_characters() {
    for c in [
        'e', 'o', 't', 'u', 'A', 'Z', ' ', '\n', '-', '=', '/', 'é', '\0',
    ] {
        let encoded = format!("0{c}");
        assert!(decode_base32(&encoded).is_err(), "{encoded:?}");
    }
}

#[test]
fn base32_rejects_wrong_lengths_and_padding() {
    for len in [1, 3, 6, 9, 11, 14] {
        assert!(decode_base32(&"0".repeat(len)).is_err(), "length {len}");
    }
    for encoded in ["80", "zz", "2000", "z000", "h0000", "4000000", "2"] {
        assert!(decode_base32(encoded).is_err(), "{encoded}");
    }
}

#[test]
fn sha256_rejects_bad_prefix_length_characters_and_padding() {
    for prefix in ["", "sha1:", "SHA256:", "sha256-", " sha256:"] {
        assert!(parse_sha256(&format!("{prefix}{}", "0".repeat(52))).is_err());
    }
    for len in [0, 32, 51, 53, 64] {
        assert!(parse_sha256(&format!("sha256:{}", "0".repeat(len))).is_err());
    }
    for c in ['e', 'o', 't', 'u', 'A', '=', ' ', '\n', '2', 'z'] {
        assert!(parse_sha256(&format!("sha256:{c}{}", "0".repeat(51))).is_err());
    }
}

#[test]
fn store_path_accepts_nix_names_and_length_boundary() {
    for name in [
        "a",
        "pkg-1.0.drv",
        "AZaz09+-._?=",
        ".hidden",
        "..hidden",
        "...",
        "-",
        "...-pkg",
        &"a".repeat(211),
    ] {
        let path = format!("/nix/store/{STORE_HASH}-{name}");
        let parsed = parse_store_path(&path).unwrap();
        assert_eq!(parsed.hash, STORE_HASH);
        assert_eq!(parsed.name, name);
        assert_eq!(format!("/nix/store/{}-{}", parsed.hash, parsed.name), path);
    }
}

#[test]
fn store_path_rejects_bad_prefix_hash_length_and_separator() {
    for prefix in [
        "",
        "/store/",
        "nix/store/",
        "/nix/store//",
        "/nix/store/../",
    ] {
        assert!(parse_store_path(&format!("{prefix}{STORE_HASH}-pkg")).is_err());
    }
    for len in [0, 1, 31, 33, 52] {
        assert!(parse_store_path(&format!("/nix/store/{}-pkg", "0".repeat(len))).is_err());
    }
    assert!(parse_store_path(&format!("/nix/store/{STORE_HASH}")).is_err());
    assert!(parse_store_path(&format!("/nix/store/{STORE_HASH}+pkg")).is_err());
}

#[test]
fn store_path_rejects_bad_hash_characters() {
    for c in ['e', 'o', 't', 'u', 'A', '/', '.', 'é', '\0'] {
        assert!(parse_store_path(&format!("/nix/store/{}{c}-pkg", "0".repeat(31))).is_err());
    }
}

#[test]
fn store_path_rejects_bad_names() {
    for name in ["", ".", "..", ".-pkg", "..-pkg", &"a".repeat(212)] {
        assert!(
            parse_store_path(&format!("/nix/store/{STORE_HASH}-{name}")).is_err(),
            "{name:?}"
        );
    }
    for c in ['/', ' ', '\n', '\t', ':', '@', '%', '\\', 'é', '\0'] {
        assert!(parse_store_path(&format!("/nix/store/{STORE_HASH}-pkg{c}")).is_err());
    }
}
