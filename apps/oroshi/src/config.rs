use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub metrics: Metrics,
    pub log: Log,
    pub storage: Storage,
    pub gc: Gc,
    pub retention: Retention,
    pub tokens: BTreeMap<String, Token>,
    pub nix: Nix,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    pub listen: SocketAddr,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            listen: ([127, 0, 0, 1], 8080).into(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Metrics {
    pub listen: Option<SocketAddr>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Log {
    pub level: String,
    pub access: bool,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            level: "info".into(),
            access: false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Storage {
    pub dir: PathBuf,
    pub max_bytes: Option<u64>,
    pub gc_target_ratio: f64,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            dir: "/var/lib/oroshi".into(),
            max_bytes: None,
            gc_target_ratio: 0.9,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Gc {
    /// Seconds before an unreferenced blob can be removed.
    pub orphan_grace: u64,
}

impl Default for Gc {
    fn default() -> Self {
        Self { orphan_grace: 3600 }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retention {
    pub days: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self { days: 30 }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub sha256: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Nix {
    pub priority: u32,
    pub trusted_keys: Vec<String>,
    pub max_nar_bytes: u64,
}

impl Default for Nix {
    fn default() -> Self {
        Self {
            priority: 30,
            trusted_keys: Vec::new(),
            max_nar_bytes: 16 * 1024 * 1024 * 1024,
        }
    }
}

impl Config {
    pub fn parse(input: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(input).map_err(|error: toml::de::Error| {
            let line = error.span().map(|span| {
                input[..span.start]
                    .bytes()
                    .filter(|&byte| byte == b'\n')
                    .count()
                    + 1
            });
            // Only field names from serde diagnostics are safe to retain.
            let field = ["unknown field `", "missing field `"]
                .into_iter()
                .find_map(|prefix| {
                    error
                        .message()
                        .strip_prefix(prefix)
                        .and_then(|rest| rest.split_once('`'))
                        .map(|(key, _)| format!("{} `{key}`", prefix.trim_end_matches(" `")))
                });
            format!(
                "invalid configuration{}{}",
                field.map(|field| format!(": {field}")).unwrap_or_default(),
                line.map(|line| format!(" at line {line}"))
                    .unwrap_or_default()
            )
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        crate::log::filter(&self.log.level, None)
            .map_err(|_| "log.level must be a valid log filter".to_owned())?;
        for (index, key) in self.nix.trusted_keys.iter().enumerate() {
            key.parse::<oroshi_nix::PublicKey>()
                .map_err(|_| format!("nix.trusted_keys[{index}] must be a valid public key"))?;
        }
        if let Some(listen) = self.metrics.listen
            && !listen.ip().is_loopback()
        {
            return Err("metrics.listen must be a loopback address".into());
        }
        for (name, token) in &self.tokens {
            if token.sha256.len() != 64
                || !token
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(format!(
                    "tokens.{name}.sha256 must be 64 lowercase hexadecimal characters"
                ));
            }
            for scope in &token.scopes {
                if !matches!(
                    scope.as_str(),
                    "nix:write"
                        | "nix:read"
                        | "nix:pin"
                        | "go:write"
                        | "go:read"
                        | "cargo:write"
                        | "cargo:read"
                        | "hex:write"
                        | "hex:read"
                        | "npm:write"
                        | "npm:read"
                ) {
                    return Err(format!("tokens.{name}.scopes contains an unknown scope"));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(hash: &str, scopes: &str) -> String {
        format!("[tokens.builder]\nsha256 = {hash:?}\nscopes = [{scopes}]\n")
    }

    #[test]
    fn invalid_configuration_never_echoes_values() {
        let secret = "token-looking-secret-0123456789";
        for input in [
            format!("[server]\nlisten = '{secret}'"),
            format!("[nix]\npriority = '{secret}'"),
            format!("unexpected = '{secret}'"),
            format!("[log]\nlevel = 'oroshi={secret}'"),
            format!("[nix]\ntrusted_keys = ['{secret}']"),
            format!("[server]\nlisten = '{secret}"),
        ] {
            let error = Config::parse(&input).unwrap_err();
            assert!(!error.contains(secret), "{error}");
            assert!(
                error.contains("line")
                    || error.contains("log.level")
                    || error.contains("nix.trusted_keys"),
                "{error}"
            );
        }
    }

    #[test]
    fn refuses_malformed_trusted_keys() {
        for key in ["example.org:!!!!", "example.org:YQ=="] {
            let error = Config::parse(&format!("[nix]\ntrusted_keys = [{key:?}]")).unwrap_err();
            assert!(error.contains("nix.trusted_keys[0]"), "{error}");
            assert!(!error.contains(key), "{error}");
        }
    }

    #[test]
    fn defaults_apply_to_absent_sections_and_fields() {
        for input in [
            "",
            "[server]\n[metrics]\n[log]\n[storage]\n[gc]\n[retention]\n[tokens]\n[nix]",
        ] {
            let config = Config::parse(input).unwrap();
            assert_eq!(config.server.listen, "127.0.0.1:8080".parse().unwrap());
            assert_eq!(config.metrics.listen, None);
            assert_eq!(config.log.level, "info");
            assert!(!config.log.access);
            assert_eq!(config.storage.dir, PathBuf::from("/var/lib/oroshi"));
            assert_eq!(config.storage.max_bytes, None);
            assert_eq!(config.storage.gc_target_ratio, 0.9);
            assert_eq!(config.gc.orphan_grace, 3600);
            assert_eq!(config.retention.days, 30);
            assert!(config.tokens.is_empty());
            assert_eq!(config.nix.priority, 30);
            assert!(config.nix.trusted_keys.is_empty());
            assert_eq!(config.nix.max_nar_bytes, 16 * 1024 * 1024 * 1024);
        }
    }

    #[test]
    fn explicit_values_override_defaults() {
        let config = Config::parse(
            r#"
[server]
listen = "[::1]:8081"
[log]
level = "debug"
access = true
[storage]
dir = "/var/lib/oroshi-example"
max_bytes = 2048
gc_target_ratio = 0.8
[gc]
orphan_grace = 7200
[retention]
days = 7
[nix]
priority = 20
trusted_keys = ["example.org:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="]
max_nar_bytes = 1024
"#,
        )
        .unwrap();
        assert_eq!(config.server.listen, "[::1]:8081".parse().unwrap());
        assert_eq!(config.log.level, "debug");
        assert!(config.log.access);
        assert_eq!(config.storage.dir, PathBuf::from("/var/lib/oroshi-example"));
        assert_eq!(config.storage.max_bytes, Some(2048));
        assert_eq!(config.storage.gc_target_ratio, 0.8);
        assert_eq!(config.gc.orphan_grace, 7200);
        assert_eq!(config.retention.days, 7);
        assert_eq!(config.nix.priority, 20);
        assert_eq!(
            config.nix.trusted_keys,
            ["example.org:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="]
        );
        assert_eq!(config.nix.max_nar_bytes, 1024);
    }

    #[test]
    fn refuses_unknown_keys_in_every_table() {
        let valid_token = token(&"a".repeat(64), "\"nix:write\"");
        for prefix in [
            "",
            "[server]\n",
            "[metrics]\n",
            "[log]\n",
            "[storage]\n",
            "[gc]\n",
            "[retention]\n",
            "[nix]\n",
            &valid_token,
        ] {
            let error = Config::parse(&format!("{prefix}unexpected = true\n")).unwrap_err();
            assert!(error.contains("unexpected"), "{error}");
        }
    }

    #[test]
    fn refuses_non_loopback_metrics() {
        for address in [
            "0.0.0.0:9000",
            "192.0.2.1:9000",
            "[::]:9000",
            "[2001:db8::1]:9000",
        ] {
            let error = Config::parse(&format!("[metrics]\nlisten = {address:?}")).unwrap_err();
            assert!(error.contains("metrics.listen"), "{error}");
        }
    }

    #[test]
    fn accepts_ipv4_and_ipv6_loopback_metrics() {
        for address in ["127.0.0.1:9000", "127.1.2.3:9000", "[::1]:9000"] {
            let config = Config::parse(&format!("[metrics]\nlisten = {address:?}")).unwrap();
            assert_eq!(config.metrics.listen, Some(address.parse().unwrap()));
        }
    }

    #[test]
    fn refuses_malformed_token_hashes() {
        for hash in [
            String::new(),
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            "g".repeat(64),
            "é".repeat(32),
        ] {
            let error = Config::parse(&token(&hash, "\"nix:write\"")).unwrap_err();
            assert!(error.contains("tokens.builder.sha256"), "{error}");
        }
    }

    #[test]
    fn refuses_unknown_scopes() {
        for scope in ["nix:admin", "other:write", "cargo:pin", "NIX:write", ""] {
            let error = Config::parse(&token(&"a".repeat(64), &format!("{scope:?}"))).unwrap_err();
            assert!(error.contains("tokens.builder.scopes"), "{error}");
        }
    }

    #[test]
    fn accepts_known_scopes_and_lowercase_hex() {
        let scopes = "\"nix:write\", \"nix:read\", \"nix:pin\", \"go:write\", \"go:read\", \"cargo:write\", \"cargo:read\", \"hex:write\", \"hex:read\", \"npm:write\", \"npm:read\"";
        Config::parse(&token(&"0123456789abcdef".repeat(4), scopes)).unwrap();
        Config::parse(&token(&"a".repeat(64), "")).unwrap();
    }

    #[test]
    fn token_fields_are_required() {
        let hash_field = format!("sha256 = {:?}", "a".repeat(64));
        for (fields, key) in [("scopes = []", "sha256"), (hash_field.as_str(), "scopes")] {
            let error = Config::parse(&format!("[tokens.builder]\n{fields}")).unwrap_err();
            assert!(error.contains(key), "{error}");
        }
    }

    #[test]
    fn complete_example_is_valid() {
        let config = Config::parse(include_str!("../../../nix/example.toml")).unwrap();
        assert_eq!(config.server.listen, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(
            config.metrics.listen.unwrap(),
            "127.0.0.1:9000".parse().unwrap()
        );
        assert_eq!(config.log.level, "info");
        assert!(config.log.access);
        assert_eq!(config.storage.dir, PathBuf::from("/var/lib/oroshi"));
        assert_eq!(config.storage.max_bytes, Some(107374182400));
        assert_eq!(config.storage.gc_target_ratio, 0.9);
        assert_eq!(config.gc.orphan_grace, 3600);
        assert_eq!(config.retention.days, 30);
        assert_eq!(
            config.tokens["builder"].sha256,
            "0123456789abcdef".repeat(4)
        );
        assert_eq!(config.tokens["builder"].scopes, ["nix:write", "nix:pin"]);
        assert_eq!(config.nix.priority, 30);
        assert_eq!(
            config.nix.trusted_keys,
            ["builder.example.org:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="]
        );
        assert_eq!(config.nix.max_nar_bytes, 17179869184);
    }
}
