//! Defence-in-depth redaction for reports that may be shared publicly.
//!
//! Discovery already avoids reading identifying files. This pass runs on the
//! final JSON anyway and removes MAC-address-like strings and any known
//! sensitive values (hostname, user name) that slipped in.

use serde_json::Value;

/// Placeholder for removed MAC addresses.
pub const MAC_PLACEHOLDER: &str = "<redacted-mac>";
/// Placeholder for removed sensitive values.
pub const PLACEHOLDER: &str = "<redacted>";

/// Removes sensitive data from every string in `value`.
///
/// Tokens shorter than 3 characters only replace exact matches, so a
/// short user name like `ab` doesn't corrupt words such as `about`.
pub fn redact(value: &mut Value, sensitive: &[String]) {
    match value {
        Value::String(s) => *s = redact_str(s, sensitive),
        Value::Array(items) => items.iter_mut().for_each(|v| redact(v, sensitive)),
        Value::Object(map) => {
            let entries: Vec<(String, Value)> = std::mem::take(map).into_iter().collect();
            for (k, mut v) in entries {
                redact(&mut v, sensitive);
                map.insert(redact_str(&k, sensitive), v);
            }
        }
        _ => {}
    }
}

fn redact_str(s: &str, sensitive: &[String]) -> String {
    let mut out = redact_macs(s);
    for token in sensitive.iter().filter(|t| !t.trim().is_empty()) {
        if token.chars().count() >= 3 {
            out = out.replace(token.as_str(), PLACEHOLDER);
        } else if out == *token {
            out = PLACEHOLDER.to_owned();
        }
    }
    out
}

/// Replaces `aa:bb:cc:dd:ee:ff` (also with `-` or `_` separators).
fn redact_macs(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if is_mac_at(b, i) {
            out.push_str(MAC_PLACEHOLDER);
            i += 17;
        } else {
            let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
            out.push(ch);
            i += ch.len_utf8().max(1);
        }
    }
    out
}

fn is_mac_at(b: &[u8], i: usize) -> bool {
    if i + 17 > b.len() {
        return false;
    }
    let sep = b[i + 2];
    if !matches!(sep, b':' | b'-' | b'_') {
        return false;
    }
    (0..6).all(|g| {
        let p = i + g * 3;
        b[p].is_ascii_hexdigit() && b[p + 1].is_ascii_hexdigit() && (g == 5 || b[p + 2] == sep)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_macs() {
        assert_eq!(
            redact_macs("hid-aa:bb:cc:dd:ee:ff-battery"),
            "hid-<redacted-mac>-battery"
        );
        assert_eq!(redact_macs("AA-BB-CC-DD-EE-FF"), "<redacted-mac>");
        assert_eq!(redact_macs("0000:00:02.0"), "0000:00:02.0");
        assert_eq!(redact_macs("no mac here ✓"), "no mac here ✓");
    }

    #[test]
    fn redacts_tokens_everywhere() {
        let mut v =
            json!({"a": ["host examplehost up"], "examplehost": 1, "u": "ab", "w": "about"});
        redact(&mut v, &["examplehost".into(), "ab".into()]);
        assert_eq!(v["a"][0], "host <redacted> up");
        assert!(v.get("<redacted>").is_some());
        assert_eq!(v["u"], "<redacted>");
        assert_eq!(v["w"], "about");
    }
}
