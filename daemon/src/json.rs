//! Minimal flat-object JSON writer/reader.
//!
//! The IPC surface is tiny (a handful of keys), so pulling in serde would add
//! hundreds of crates for no benefit.

pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub fn s(key: &str, val: &str) -> String {
    format!("\"{}\":\"{}\"", esc(key), esc(val))
}

pub fn n(key: &str, val: f64) -> String {
    format!("\"{}\":{}", esc(key), val)
}

pub fn i(key: &str, val: i64) -> String {
    format!("\"{}\":{}", esc(key), val)
}

pub fn b(key: &str, val: bool) -> String {
    format!("\"{}\":{}", esc(key), val)
}

pub fn obj(pairs: &[String]) -> String {
    format!("{{{}}}", pairs.join(","))
}

/// A JSON array of strings, escaped for the wire/state file.
pub fn str_array(key: &str, vals: &[String]) -> String {
    let items: Vec<String> = vals
        .iter()
        .map(|v| format!("\"{}\"", esc(v)))
        .collect();
    format!("\"{}\":[{}]", esc(key), items.join(","))
}

/// Extract a string value for `key` from a flat JSON object.
pub fn get_str(src: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\"", key);
    let start = src.find(&pat)? + pat.len();
    let rest = src[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = stripped.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => return Some(out),
                '\\' => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some(o) => out.push(o),
                    None => break,
                },
                c => out.push(c),
            }
        }
        None
    } else {
        None
    }
}

/// Read a bare JSON boolean. Deliberately strict: only a `true`/`false` literal
/// directly after `"key":` counts, so the text `"enable":true` smuggled inside
/// another string field (a label, say) can never flip the decision.
pub fn get_bool(src: &str, key: &str) -> Option<bool> {
    let pat = format!("\"{}\"", key);
    let start = src.find(&pat)? + pat.len();
    let rest = src[start..].trim_start().strip_prefix(':')?.trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// Extract a flat JSON array of strings for `key`. Only used for the persisted
/// tracked-service list, so it is deliberately small and strict: anything that
/// is not a string element makes the whole array unreadable rather than being
/// guessed at.
pub fn get_str_array(src: &str, key: &str) -> Option<Vec<String>> {
    let pat = format!("\"{}\"", key);
    let start = src.find(&pat)? + pat.len();
    let rest = src[start..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    if !rest.starts_with('[') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::new();
    let mut i = 1usize; // byte index just after '['
    loop {
        while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        match bytes[i] {
            b']' => return Some(out),
            b',' => {
                i += 1;
            }
            b'"' => {
                i += 1;
                let mut s = String::new();
                let mut closed = false;
                while i < bytes.len() {
                    match bytes[i] {
                        b'"' => {
                            i += 1;
                            closed = true;
                            break;
                        }
                        b'\\' => {
                            i += 1;
                            if i >= bytes.len() {
                                return None;
                            }
                            match bytes[i] {
                                b'n' => s.push('\n'),
                                b't' => s.push('\t'),
                                b'r' => s.push('\r'),
                                b'"' => s.push('"'),
                                b'\\' => s.push('\\'),
                                b'/' => s.push('/'),
                                b'u' => {
                                    if i + 4 >= bytes.len() {
                                        return None;
                                    }
                                    let hex = std::str::from_utf8(&bytes[i + 1..i + 5]).ok()?;
                                    let cp = u32::from_str_radix(hex, 16).ok()?;
                                    if let Some(c) = char::from_u32(cp) {
                                        s.push(c);
                                    }
                                    i += 4;
                                }
                                other => s.push(other as char),
                            }
                            i += 1;
                        }
                        _ => {
                            let ch = rest[i..].chars().next()?;
                            s.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                }
                if !closed {
                    return None;
                }
                out.push(s);
            }
            _ => return None,
        }
    }
}

/// Only used by the test suite to assert on numbers we emit; kept test-only so
/// the release build stays warning-free.
#[cfg(test)]
pub fn get_num(src: &str, key: &str) -> Option<f64> {
    let pat = format!("\"{}\"", key);
    let start = src.find(&pat)? + pat.len();
    let rest = src[start..].trim_start().strip_prefix(':')?.trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E'))
        .unwrap_or(rest.len());
    rest[..end].parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_strings_with_escapes() {
        let body = obj(&[s("cmd", "say \"hi\"\nnow")]);
        assert_eq!(get_str(&body, "cmd").unwrap(), "say \"hi\"\nnow");
    }

    #[test]
    fn enable_is_only_read_from_a_real_boolean_field() {
        assert_eq!(get_bool("{\"enable\":true}", "enable"), Some(true));
        assert_eq!(get_bool("{\"enable\" : false }", "enable"), Some(false));
        // A label that merely contains the text must not count as the flag.
        let smuggled = "{\"label\":\"x\\\"enable\\\":true\",\"enable\":false}";
        assert_eq!(get_bool(smuggled, "enable"), Some(false));
        assert_eq!(get_bool("{\"cmd\":\"agents\"}", "enable"), None);
    }

    #[test]
    fn reads_numbers_and_bools() {
        let body = obj(&[
            json_num(),
            b("flag", true),
            s("mode", "balanced"),
        ]);
        assert_eq!(get_num(&body, "cpu_temp"), Some(71.5));
        assert!(body.contains("\"flag\":true"));
        assert_eq!(get_str(&body, "mode").unwrap(), "balanced");
    }

    fn json_num() -> String {
        n("cpu_temp", 71.5)
    }

    #[test]
    fn missing_keys_are_none() {
        let body = obj(&[s("a", "1")]);
        assert!(get_str(&body, "b").is_none());
        assert!(get_num(&body, "b").is_none());
    }

    #[test]
    fn escapes_control_characters() {
        assert_eq!(esc("a\u{1}b"), "a\\u0001b");
    }

    #[test]
    fn string_arrays_round_trip_and_reject_junk() {
        let vals = vec![
            "com.apple.photoanalysisd".to_string(),
            "a \"quoted\" label".to_string(),
        ];
        let body = obj(&[str_array("auto_trimmed", &vals)]);
        assert_eq!(get_str_array(&body, "auto_trimmed").unwrap(), vals);

        assert_eq!(get_str_array("{\"auto_trimmed\":[]}", "auto_trimmed").unwrap(), Vec::<String>::new());
        assert!(get_str_array("{\"auto_trimmed\":[1,2]}", "auto_trimmed").is_none());
        assert!(get_str_array("{\"auto_trimmed\":\"nope\"}", "auto_trimmed").is_none());
        assert!(get_str_array("{\"other\":[]}", "auto_trimmed").is_none());
    }
}
