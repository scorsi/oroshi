// Applicable envelope rules from nisshi/docs/log-schema.md; examples copied from
// nisshi/fixtures/logs/service-lines.ndjson. No external checkout is needed in Nix.
use serde_json::Value;

pub const RESERVED: &[&str] = &["node", "service", "stream", "seq", "source", "recv_ts"];

pub fn valid(line: &str) -> bool {
    let Ok(Value::Object(m)) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    let ts = m.get("ts").and_then(Value::as_str).unwrap_or("");
    let event = m.get("event").and_then(Value::as_str).unwrap_or("");
    m.get("v").and_then(Value::as_u64) == Some(1)
        && ts.len() == 30
        && ts.ends_with('Z')
        && ts.as_bytes()[19] == b'.'
        && ts.as_bytes()[20..29].iter().all(u8::is_ascii_digit)
        && matches!(
            m.get("level").and_then(Value::as_str),
            Some("debug" | "info" | "warn" | "error")
        )
        && event.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && event.split('-').all(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
        && m.get("msg").is_none_or(Value::is_string)
        && m.get("err")
            .is_none_or(|v| v.is_string() && matches!(m["level"].as_str(), Some("warn" | "error")))
        && m.keys()
            .all(|s| !RESERVED.contains(&s.as_str()) && !s.starts_with('_'))
        && line.len() <= 1_048_576
        && !line.contains(['\r', '\n'])
}
