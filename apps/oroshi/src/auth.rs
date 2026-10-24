use std::hint::black_box;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

use crate::config::Config;

/// Route-specific state for `from_fn_with_state(state, auth::require_scope)`.
pub struct Tokens {
    hashes: Vec<(String, bool)>,
}

impl Tokens {
    pub fn new(config: &Config, scope: &str) -> Self {
        Self {
            hashes: config
                .tokens
                .values()
                .map(|token| {
                    (
                        token.sha256.clone(),
                        token.scopes.iter().any(|value| value == scope),
                    )
                })
                .collect(),
        }
    }

    fn authorize(&self, secret: &[u8]) -> StatusCode {
        let presented = format!("{:x}", Sha256::digest(secret));
        let mut known = 0;
        let mut allowed = 0;
        for (hash, has_scope) in &self.hashes {
            let mut difference = usize::from(hash.len() != 64);
            // Black-box each byte so optimization cannot turn this into an early-exit comparison.
            for index in 0..64 {
                difference |= usize::from(
                    black_box(presented.as_bytes()[index])
                        ^ black_box(hash.as_bytes().get(index).copied().unwrap_or(0)),
                );
            }
            let matches = u8::from(black_box(difference) == 0);
            known |= matches;
            allowed |= matches & u8::from(*has_scope);
        }
        match (black_box(known), black_box(allowed)) {
            (0, _) => StatusCode::UNAUTHORIZED,
            (_, 0) => StatusCode::FORBIDDEN,
            _ => StatusCode::OK,
        }
    }
}

fn credential(headers: &HeaderMap) -> Option<Vec<u8>> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let secret = if let Some((scheme, value)) = value.split_once(' ') {
        if scheme.eq_ignore_ascii_case("Bearer") {
            value.as_bytes().to_vec()
        } else if scheme.eq_ignore_ascii_case("Basic") {
            let decoded = STANDARD.decode(value).ok()?;
            let colon = decoded.iter().position(|&byte| byte == b':')?;
            decoded[colon + 1..].to_vec()
        } else {
            return None;
        }
    } else {
        value.as_bytes().to_vec()
    };
    if secret.is_empty() || secret.iter().any(u8::is_ascii_whitespace) {
        return None;
    }
    Some(secret)
}

pub async fn require_scope(
    State(tokens): State<Arc<Tokens>>,
    request: Request,
    next: Next,
) -> Response {
    let status = credential(request.headers())
        .map(|secret| tokens.authorize(&secret))
        .unwrap_or(StatusCode::UNAUTHORIZED);
    match status {
        StatusCode::OK => next.run(request).await,
        StatusCode::UNAUTHORIZED => (
            status,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"oroshi\"")],
        )
            .into_response(),
        _ => status.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, middleware, routing::put};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn tokens(scope: &str) -> Tokens {
        let config = Config::parse(&format!(
            "[tokens.first]\nsha256 = {:?}\nscopes = []\n\
             [tokens.writer]\nsha256 = {:?}\nscopes = ['nix:write']\n\
             [tokens.last]\nsha256 = {:?}\nscopes = ['nix:pin']\n",
            format!("{:x}", Sha256::digest(b"first")),
            format!("{:x}", Sha256::digest(b"secret")),
            format!("{:x}", Sha256::digest(b"last")),
        ))
        .unwrap();
        Tokens::new(&config, scope)
    }

    #[test]
    fn compares_all_positions_and_checks_scope() {
        let tokens = tokens("nix:write");
        assert_eq!(tokens.authorize(b"first"), StatusCode::FORBIDDEN);
        assert_eq!(tokens.authorize(b"secret"), StatusCode::OK);
        assert_eq!(tokens.authorize(b"last"), StatusCode::FORBIDDEN);
        assert_eq!(tokens.authorize(b"unknown"), StatusCode::UNAUTHORIZED);
        assert_eq!(self::tokens("nix:pin").authorize(b"last"), StatusCode::OK);
        assert_eq!(
            self::tokens("nix:pin").authorize(b"secret"),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            Tokens::new(&Config::default(), "nix:write").authorize(b"secret"),
            StatusCode::UNAUTHORIZED
        );
        for index in 0..64 {
            let mut tokens = self::tokens("nix:write");
            let hash = &mut tokens.hashes.last_mut().unwrap().0;
            let replacement = if hash.as_bytes()[index] == b'0' {
                "1"
            } else {
                "0"
            };
            hash.replace_range(index..index + 1, replacement);
            assert_eq!(tokens.authorize(b"secret"), StatusCode::UNAUTHORIZED);
        }
    }

    #[test]
    fn parses_three_forms_and_refuses_malformed_credentials() {
        for value in [
            "secret",
            "Bearer secret",
            "bearer secret",
            "Basic dXNlcjpzZWNyZXQ=",
            "basic OnNlY3JldA==",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, value.parse().unwrap());
            assert_eq!(credential(&headers).unwrap(), b"secret");
        }
        for value in [
            "",
            "Bearer ",
            "Bearer secret extra",
            "Basic !!!",
            "Basic dXNlcg==",
            "Basic dXNlcjo=",
            "Digest secret",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, value.parse().unwrap());
            assert!(credential(&headers).is_none(), "{value}");
        }
        let mut headers = HeaderMap::new();
        assert!(credential(&headers).is_none());
        headers.append(header::AUTHORIZATION, "secret".parse().unwrap());
        headers.append(header::AUTHORIZATION, "secret".parse().unwrap());
        assert!(credential(&headers).is_none());
    }

    #[tokio::test]
    async fn http_credentials_challenge_and_scope_refusals() {
        let app = Router::new()
            .route("/protected", put(|| async { StatusCode::NO_CONTENT }))
            .route_layer(middleware::from_fn_with_state(
                Arc::new(tokens("nix:write")),
                require_scope,
            ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for (authorization, status) in [
            ("Authorization: secret\r\n", 204),
            ("Authorization: Bearer secret\r\n", 204),
            ("Authorization: Basic dXNlcjpzZWNyZXQ=\r\n", 204),
            ("", 401),
            ("Authorization: unknown\r\n", 401),
            ("Authorization: Basic !!!\r\n", 401),
            ("Authorization: Bearer \r\n", 401),
            ("Authorization: secret\r\nAuthorization: unknown\r\n", 401),
            ("Authorization: last\r\n", 403),
        ] {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream.write_all(format!("PUT /protected HTTP/1.1\r\nHost: example.org\r\n{authorization}Content-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            assert!(
                response.starts_with(&format!("HTTP/1.1 {status}")),
                "{response}"
            );
            assert_eq!(
                response.contains("www-authenticate: Basic realm=\"oroshi\"\r\n"),
                status == 401,
                "{response}"
            );
        }
        server.abort();
    }
}
