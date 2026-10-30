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
}
