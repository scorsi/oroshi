use oroshi_nix::NarInfo;

const HELLO: &str = include_str!("fixtures/narinfo/cache-f6ivp5fiw1nwq690548fb9qr8p968mj5.narinfo");
const LIBICONV: &str =
    include_str!("fixtures/narinfo/cache-np8ldxalhj7qkmmf3p5gxx2d9nnm5q3x.narinfo");

fn without(input: &str, key: &str) -> String {
    input
        .lines()
        .filter(|line| !line.starts_with(&format!("{key}: ")))
        .map(|line| format!("{line}\n"))
        .collect()
}

#[test]
fn nix_fixtures_round_trip_byte_identically() {
    for fixture in [
        HELLO,
        LIBICONV,
        include_str!("fixtures/narinfo/cache-zstd-f6ivp5fiw1nwq690548fb9qr8p968mj5.narinfo"),
        include_str!("fixtures/narinfo/cache-zstd-np8ldxalhj7qkmmf3p5gxx2d9nnm5q3x.narinfo"),
    ] {
        let info: NarInfo = fixture.parse().unwrap();
        assert_eq!(info.to_string(), fixture);
        assert_eq!(info.to_string().parse::<NarInfo>().unwrap(), info);
    }
    assert_eq!(HELLO.parse::<NarInfo>().unwrap().signatures.len(), 2);
}

#[test]
fn fixture_fingerprint() {
    assert_eq!(
        HELLO.parse::<NarInfo>().unwrap().fingerprint(),
        concat!(
            "1;/nix/store/f6ivp5fiw1nwq690548fb9qr8p968mj5-hello-2.12.3;",
            "sha256:0an2hpw0ia6pvrf7cy877albpgl1aijbsqn747hi6nsfmzpnwmi8;113096;",
            "/nix/store/np8ldxalhj7qkmmf3p5gxx2d9nnm5q3x-libiconv-113"
        )
    );
}

#[test]
fn libiconv_self_reference_is_valid() {
    let info: NarInfo = LIBICONV.parse().unwrap();
    assert_eq!(info.references.len(), 1);
    assert!(info.references.contains(&info.store_path));
    assert!(
        info.fingerprint()
            .ends_with(&format!(";{}", info.store_path))
    );
}

#[test]
fn missing_required_fields_are_refused() {
    for key in ["StorePath", "URL", "NarHash", "NarSize"] {
        let error = without(HELLO, key).parse::<NarInfo>().unwrap_err();
        assert_eq!(error.to_string(), format!("missing {key}"));
    }
}

#[test]
fn duplicate_single_valued_fields_are_refused() {
    for line in HELLO
        .lines()
        .filter(|line| !line.starts_with("Sig: "))
        .chain(["CA: fixed:r:sha256:0000000000000000000000000000000000000000000000000000"])
    {
        let input = if line.starts_with("CA:") {
            format!("{HELLO}{line}\n{line}\n")
        } else {
            format!("{HELLO}{line}\n")
        };
        assert_eq!(
            input.parse::<NarInfo>().unwrap_err().to_string(),
            "duplicate single-valued narinfo field"
        );
    }
}

#[test]
fn malformed_hashes_are_refused() {
    for key in ["FileHash", "NarHash"] {
        for value in [
            "sha256:bad",
            "sha1:abc",
            "sha256:2000000000000000000000000000000000000000000000000000",
        ] {
            assert!(
                format!("{}{key}: {value}\n", without(HELLO, key))
                    .parse::<NarInfo>()
                    .is_err()
            );
        }
    }
}

#[test]
fn malformed_store_paths_are_refused() {
    for (key, value) in [
        (
            "StorePath",
            "/other/np8ldxalhj7qkmmf3p5gxx2d9nnm5q3x-libiconv-113",
        ),
        ("StorePath", "/nix/store/invalid-pkg"),
        ("References", "invalid-pkg"),
        (
            "References",
            "/nix/store/np8ldxalhj7qkmmf3p5gxx2d9nnm5q3x-libiconv-113",
        ),
        ("Deriver", "np8ldxalhj7qkmmf3p5gxx2d9nnm5q3x-../pkg"),
    ] {
        assert!(
            format!("{}{key}: {value}\n", without(HELLO, key))
                .parse::<NarInfo>()
                .is_err()
        );
    }
}

#[test]
fn malformed_numbers_are_refused() {
    for key in ["FileSize", "NarSize"] {
        for value in ["-1", "+1", "1.0", "1x", " 1", "18446744073709551616", ""] {
            assert!(
                format!("{}{key}: {value}\n", without(HELLO, key))
                    .parse::<NarInfo>()
                    .is_err()
            );
        }
    }
}

#[test]
fn optional_fields_and_ca_round_trip() {
    let mut input = HELLO.to_owned();
    for key in [
        "Compression",
        "FileHash",
        "FileSize",
        "References",
        "Deriver",
        "Sig",
    ] {
        input = without(&input, key);
    }
    let info: NarInfo = input.parse().unwrap();
    assert_eq!(info.compression, None);
    assert_eq!(info.file_hash, None);
    assert_eq!(info.file_size, None);
    assert_eq!(info.deriver, None);
    assert!(info.signatures.is_empty());
    assert!(info.references.is_empty());
    assert!(info.fingerprint().ends_with(";113096;"));
    assert!(info.to_string().contains("References: \n"));
    assert_eq!(info.to_string().parse::<NarInfo>().unwrap(), info);

    let ca = "fixed:r:sha256:0000000000000000000000000000000000000000000000000000";
    let input = format!("{HELLO}CA: {ca}\n");
    let info: NarInfo = input.parse().unwrap();
    assert_eq!(info.ca.as_deref(), Some(ca));
    assert_eq!(info.to_string(), input);
}

#[test]
fn references_are_sorted_and_deduplicated_for_fingerprint_and_serialization() {
    let a = "00000000000000000000000000000000-a";
    let b = "11111111111111111111111111111111-b";
    let input = format!("{}References: {b} {a} {b}\n", without(HELLO, "References"));
    let info: NarInfo = input.parse().unwrap();
    assert_eq!(info.references.len(), 2);
    assert!(
        info.fingerprint()
            .ends_with(&format!(";/nix/store/{a},/nix/store/{b}"))
    );
    assert!(info.to_string().contains(&format!("References: {a} {b}\n")));
}

#[test]
fn unknown_fields_and_malformed_lines_are_refused() {
    for line in [
        "FutureField: value",
        "no separator",
        "URL:value",
        "",
        "Sig: ",
        "CA: ",
        "CA: bad\0value",
    ] {
        assert!(
            format!("{HELLO}{line}\n").parse::<NarInfo>().is_err(),
            "{line:?}"
        );
    }
    for key in [
        "StorePath",
        "URL",
        "Compression",
        "FileHash",
        "NarHash",
        "Deriver",
    ] {
        assert!(
            format!("{}{key}: \n", without(HELLO, key))
                .parse::<NarInfo>()
                .is_err()
        );
    }
}
