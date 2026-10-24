use base64::{Engine, engine::general_purpose::STANDARD};
use oroshi_nix::{NarInfo, PublicKey, Signature};

const BUILDER: &str = include_str!("fixtures/narinfo/test.example.org-1.pub");
const UPSTREAM: &str = include_str!("fixtures/narinfo/cache.nixos.org-1.pub");
const XZ: [&str; 2] = [
    include_str!("fixtures/narinfo/cache-f6ivp5fiw1nwq690548fb9qr8p968mj5.narinfo"),
    include_str!("fixtures/narinfo/cache-np8ldxalhj7qkmmf3p5gxx2d9nnm5q3x.narinfo"),
];
const ZSTD: [&str; 2] = [
    include_str!("fixtures/narinfo/cache-zstd-f6ivp5fiw1nwq690548fb9qr8p968mj5.narinfo"),
    include_str!("fixtures/narinfo/cache-zstd-np8ldxalhj7qkmmf3p5gxx2d9nnm5q3x.narinfo"),
];

fn key(input: &str) -> PublicKey {
    input.trim().parse().unwrap()
}

#[test]
fn real_nix_signatures_verify_with_both_keys() {
    for (public, fixtures) in [
        (BUILDER, XZ.as_slice()),
        (UPSTREAM, XZ.as_slice()),
        (UPSTREAM, ZSTD.as_slice()),
    ] {
        for fixture in fixtures {
            let info: NarInfo = fixture.parse().unwrap();
            let name = public.split_once(':').unwrap().0;
            let matching: Vec<_> = info
                .signatures
                .iter()
                .filter(|s| s.starts_with(&format!("{name}:")))
                .collect();
            assert_eq!(matching.len(), 1);
            let signature: Signature = matching[0].parse().unwrap();
            assert!(key(public).verify(&info.fingerprint(), &signature));
            info.verify_signatures(&[key(public)]).unwrap();
        }
    }
}

#[test]
fn tampered_fingerprint_is_refused() {
    for fixture in XZ.into_iter().chain(ZSTD) {
        let mut info: NarInfo = fixture.parse().unwrap();
        info.nar_size += 1;
        assert!(
            info.verify_signatures(&[key(BUILDER), key(UPSTREAM)])
                .is_err()
        );
    }
}

#[test]
fn substituted_dependencies_require_upstream_trust() {
    for fixture in ZSTD {
        let info: NarInfo = fixture.parse().unwrap();
        assert_eq!(info.signatures.len(), 1);
        assert!(info.verify_signatures(&[key(BUILDER)]).is_err());
        info.verify_signatures(&[key(UPSTREAM)]).unwrap();
        info.verify_signatures(&[key(BUILDER), key(UPSTREAM)])
            .unwrap();
    }
}

#[test]
fn unsigned_untrusted_and_empty_trust_are_refused() {
    let mut info: NarInfo = XZ[0].parse().unwrap();
    assert_eq!(
        info.verify_signatures(&[]).unwrap_err().to_string(),
        "no valid signature by a trusted key"
    );
    info.signatures
        .retain(|s| s.starts_with("test.example.org-1:"));
    assert!(info.verify_signatures(&[key(UPSTREAM)]).is_err());
    info.signatures.clear();
    assert!(
        info.verify_signatures(&[key(BUILDER), key(UPSTREAM)])
            .is_err()
    );
}

#[test]
fn key_names_and_key_bytes_must_both_match() {
    let info: NarInfo = XZ[0].parse().unwrap();
    let signature: Signature = info
        .signatures
        .iter()
        .find(|s| s.starts_with("test.example.org-1:"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(!key(UPSTREAM).verify(&info.fingerprint(), &signature));
    let renamed = BUILDER
        .trim()
        .replacen("test.example.org-1", "other.example.org-1", 1);
    assert!(!key(&renamed).verify(&info.fingerprint(), &signature));
    let wrong_bytes = UPSTREAM
        .trim()
        .replacen("cache.nixos.org-1", "test.example.org-1", 1);
    assert!(!key(&wrong_bytes).verify(&info.fingerprint(), &signature));
}

#[test]
fn one_valid_trusted_signature_is_enough() {
    let mut info: NarInfo = XZ[0].parse().unwrap();
    let valid = info
        .signatures
        .iter()
        .find(|s| s.starts_with("test.example.org-1:"))
        .unwrap()
        .clone();
    let invalid = format!("test.example.org-1:{}", STANDARD.encode([0; 64]));
    info.signatures.insert(0, invalid);
    info.verify_signatures(&[key(BUILDER)]).unwrap();
    info.verify_signatures(&[key(UPSTREAM)]).unwrap();
    info.signatures
        .retain(|s| s.starts_with("test.example.org-1:"));
    info.signatures.retain(|s| *s != valid);
    assert!(info.verify_signatures(&[key(BUILDER)]).is_err());
}

#[test]
fn malformed_key_and_signature_encodings_are_refused() {
    for value in [
        "",
        "missing-colon",
        ":AAAA",
        "bad name:AAAA",
        "bad\0name:AAAA",
        "name:!",
        "name:AA=A",
        "name:AAAA:",
    ] {
        assert!(value.parse::<PublicKey>().is_err(), "{value:?}");
        assert!(value.parse::<Signature>().is_err(), "{value:?}");
    }
    for name in ["", "bad name", "bad\nname", "bad\0name", "bad:name"] {
        assert!(
            format!("{name}:{}", BUILDER.trim().split_once(':').unwrap().1)
                .parse::<PublicKey>()
                .is_err()
        );
        assert!(
            format!("{name}:{}", STANDARD.encode([0; 64]))
                .parse::<Signature>()
                .is_err()
        );
    }
    for size in [0, 31, 33, 64] {
        assert!(
            format!("example.org:{}", STANDARD.encode(vec![0; size]))
                .parse::<PublicKey>()
                .is_err()
        );
    }
    for size in [0, 32, 63, 65] {
        assert!(
            format!("example.org:{}", STANDARD.encode(vec![0; size]))
                .parse::<Signature>()
                .is_err()
        );
    }
}

#[test]
fn malformed_sig_lines_are_refused_even_alongside_valid_signatures() {
    for value in [
        "example.org:!".to_owned(),
        format!("example.org:{}", STANDARD.encode([0; 63])),
    ] {
        let mut info: NarInfo = XZ[0].parse().unwrap();
        info.signatures.push(value);
        assert!(
            info.verify_signatures(&[key(BUILDER), key(UPSTREAM)])
                .is_err()
        );
    }
}

#[test]
fn merge_is_a_stable_deduplicated_idempotent_union() {
    let mut info: NarInfo = XZ[0].parse().unwrap();
    let original = info.clone();
    let mut other = info.clone();
    info.signatures = vec![original.signatures[1].clone(); 2];
    other.signatures = vec![
        original.signatures[0].clone(),
        original.signatures[1].clone(),
        original.signatures[0].clone(),
    ];
    info.merge_signatures(&other).unwrap();
    assert_eq!(
        info.signatures,
        vec![
            original.signatures[1].clone(),
            original.signatures[0].clone()
        ]
    );
    let merged = info.clone();
    info.merge_signatures(&other).unwrap();
    info.merge_signatures(&info.clone()).unwrap();
    assert_eq!(info, merged);
    let mut unsigned = original.clone();
    unsigned.signatures.clear();
    unsigned.merge_signatures(&original).unwrap();
    assert_eq!(unsigned, original);
}

#[test]
fn conflicting_merge_is_refused_without_mutation() {
    let original: NarInfo = XZ[0].parse().unwrap();
    let mut conflicts = Vec::new();
    let mut other = original.clone();
    other.store_path.push_str("-other");
    conflicts.push(other);
    let mut other = original.clone();
    other.nar_size += 1;
    conflicts.push(other);
    let mut other = original.clone();
    other.url.push_str("-other");
    conflicts.push(other);
    let mut other = original.clone();
    other.ca = Some("other".to_owned());
    conflicts.push(other);
    for other in conflicts {
        let mut info = original.clone();
        assert_eq!(
            info.merge_signatures(&other).unwrap_err().to_string(),
            "conflicting narinfo metadata"
        );
        assert_eq!(info, original);
    }
}

#[test]
fn retain_trusted_signatures_drops_untrusted_and_malformed_ones() {
    let mut info: NarInfo = XZ[1].parse().unwrap();
    let all = info.signatures.clone();
    info.signatures.push("garbage".into());
    info.signatures
        .push(format!("other.example.org-1:{}", STANDARD.encode([0; 64])));
    info.retain_trusted_signatures(&[key(BUILDER)]);
    assert_eq!(info.signatures.len(), 1);
    assert!(info.signatures[0].starts_with("test.example.org-1:"));
    info.signatures.clone_from(&all);
    info.retain_trusted_signatures(&[key(BUILDER), key(UPSTREAM)]);
    assert_eq!(info.signatures, all);
    info.retain_trusted_signatures(&[]);
    assert!(info.signatures.is_empty());
}
