//! The JIT config GitHub hands out, as the broker passes it on (§3, §8): base64
//! of a JSON map from file name to base64 file contents. The broker sets
//! `disableUpdate` in `.runner` and accepts only the three files it expects.

use std::collections::BTreeMap;

/// The files of a JIT config: nothing else is ever written.
pub const FILES: [&str; 3] = [".runner", ".credentials", ".credentials_rsaparams"];

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64, padding optional.
///
/// # Errors
/// A character outside the alphabet.
pub fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = ALPHABET.iter().position(|a| *a == c).ok_or("not base64")?;
        acc = (acc << 6) | u32::try_from(v).map_err(|e| e.to_string())?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).map_err(|e| e.to_string())?);
        }
    }
    Ok(out)
}

/// The three files, `.runner` with updates off. A config with any other file
/// is refused: a name from outside never picks a path here.
///
/// # Errors
/// A config that isn't what GitHub documents, without quoting it: it holds
/// credentials.
pub fn files(encoded: &str) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let json = b64_decode(encoded).map_err(|_| "the JIT config isn't base64")?;
    let map: BTreeMap<String, String> =
        serde_json::from_slice(&json).map_err(|_| "the JIT config isn't a map of files")?;
    let mut out = BTreeMap::new();
    for name in FILES {
        if !map.contains_key(name) {
            return Err(format!("the JIT config has no {name}"));
        }
    }
    for (name, body) in map {
        if !FILES.contains(&name.as_str()) {
            return Err("the JIT config has a file the broker doesn't know".to_owned());
        }
        let bytes =
            b64_decode(&body).map_err(|_| format!("the JIT config's {name} isn't base64"))?;
        let bytes = if name == ".runner" {
            updates_off(&bytes)?
        } else {
            bytes
        };
        out.insert(name, bytes);
    }
    Ok(out)
}

/// `.runner` with `"disableUpdate": true`, its other fields untouched. The
/// runner writes these with a UTF-8 byte order mark, so one is allowed for.
fn updates_off(runner: &[u8]) -> Result<Vec<u8>, String> {
    let body = runner.strip_prefix(b"\xef\xbb\xbf").unwrap_or(runner);
    let mut v: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(body).map_err(|_| "the JIT config's .runner isn't JSON")?;
    v.insert("disableUpdate".into(), serde_json::Value::Bool(true));
    serde_json::to_vec(&v).map_err(|e| e.to_string())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn b64_encode(b: &[u8]) -> String {
        let mut out = String::new();
        for c in b.chunks(3) {
            let n = (u32::from(c[0]) << 16)
                | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
                | u32::from(*c.get(2).unwrap_or(&0));
            for i in 0..4 {
                if i <= c.len() {
                    out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    pub fn config(runner: &[u8]) -> String {
        let m = serde_json::json!({
            ".runner": b64_encode(runner),
            ".credentials": b64_encode(b"{\"scheme\":\"OAuth\"}"),
            ".credentials_rsaparams": b64_encode(b"{\"d\":\"secret\"}"),
        });
        b64_encode(m.to_string().as_bytes())
    }

    #[test]
    fn base64_round_trips() {
        for s in ["", "a", "ab", "abc", "abcd", "\u{1F600} bytes"] {
            assert_eq!(b64_decode(&b64_encode(s.as_bytes())).unwrap(), s.as_bytes());
        }
        assert!(b64_decode("not*base64").is_err());
    }

    #[test]
    fn disable_update_is_set_and_the_rest_is_left_alone() {
        let f = files(&config(
            br#"{"agentId":7,"ephemeral":true,"disableUpdate":false}"#,
        ))
        .unwrap();
        assert_eq!(
            f.keys().collect::<Vec<_>>(),
            [".credentials", ".credentials_rsaparams", ".runner"]
        );
        let r: serde_json::Value = serde_json::from_slice(&f[".runner"]).unwrap();
        assert_eq!(r["disableUpdate"], true);
        assert_eq!(r["agentId"], 7);
        assert_eq!(r["ephemeral"], true);
        assert_eq!(f[".credentials"], br#"{"scheme":"OAuth"}"#);
    }

    #[test]
    fn a_byte_order_mark_is_allowed_for() {
        let mut r = b"\xef\xbb\xbf".to_vec();
        r.extend_from_slice(br#"{"agentId":7}"#);
        let f = files(&config(&r)).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&f[".runner"]).unwrap()["disableUpdate"],
            true
        );
    }

    #[test]
    fn a_config_naming_another_file_is_refused() {
        for name in ["../../config/state.json", ".env", "x"] {
            let m = serde_json::json!({
                ".runner": b64_encode(b"{}"), ".credentials": "", ".credentials_rsaparams": "",
                name: "",
            });
            let e = files(&b64_encode(m.to_string().as_bytes())).unwrap_err();
            assert!(e.contains("doesn't know"), "{e}");
        }
    }

    #[test]
    fn a_config_missing_a_file_is_refused_and_errors_quote_nothing() {
        let m = serde_json::json!({".runner": b64_encode(b"{}")});
        assert!(files(&b64_encode(m.to_string().as_bytes())).is_err());
        let e = files(&b64_encode(b"secret-not-json")).unwrap_err();
        assert!(!e.contains("secret"), "{e}");
    }
}
