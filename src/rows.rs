//! Strict decoding of native DDlog CLI output records. This is transport
//! decoding only: projections and selection remain in the running program.
use serde_json::Value;
use std::collections::BTreeSet;

struct Cursor<'a> {
    rest: &'a str,
}
impl Cursor<'_> {
    fn ws(&mut self) {
        self.rest = self.rest.trim_start_matches(char::is_whitespace);
    }
    fn token(&mut self, token: &str) -> Result<(), String> {
        self.ws();
        self.rest = self
            .rest
            .strip_prefix(token)
            .ok_or_else(|| format!("Expected {token} in DDlog row"))?;
        Ok(())
    }
    fn string(&mut self) -> Result<String, String> {
        self.token("\"")?;
        let mut result = String::new();
        loop {
            let c = self
                .rest
                .chars()
                .next()
                .ok_or("Unterminated DDlog string")?;
            self.rest = &self.rest[c.len_utf8()..];
            match c {
                '"' => return Ok(result),
                '\\' => {
                    let escape = self
                        .rest
                        .chars()
                        .next()
                        .ok_or("Unterminated DDlog escape")?;
                    self.rest = &self.rest[escape.len_utf8()..];
                    result.push(match escape {
                        '"' => '"',
                        '\\' => '\\',
                        '/' => '/',
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        'b' => '\u{8}',
                        'f' => '\u{c}',
                        '0' => '\0',
                        'u' => {
                            if let Some(tail) = self.rest.strip_prefix('{') {
                                let end = tail.find('}').ok_or("Unterminated Unicode escape")?;
                                let digits = &tail[..end];
                                if digits.is_empty()
                                    || digits.len() > 6
                                    || !digits.bytes().all(|b| b.is_ascii_hexdigit())
                                {
                                    return Err("Invalid Unicode escape".into());
                                }
                                self.rest = &tail[end + 1..];
                                char::from_u32(
                                    u32::from_str_radix(digits, 16).map_err(|e| e.to_string())?,
                                )
                                .ok_or("Invalid Unicode scalar")?
                            } else {
                                let first = self.hex4()?;
                                let scalar = if (0xd800..=0xdbff).contains(&first) {
                                    self.rest = self
                                        .rest
                                        .strip_prefix("\\u")
                                        .ok_or("Missing low surrogate")?;
                                    let low = self.hex4()?;
                                    if !(0xdc00..=0xdfff).contains(&low) {
                                        return Err("Invalid low surrogate".into());
                                    }
                                    0x10000 + ((first - 0xd800) << 10) + low - 0xdc00
                                } else {
                                    first
                                };
                                char::from_u32(scalar).ok_or("Invalid Unicode scalar")?
                            }
                        }
                        _ => return Err("Unsupported DDlog string escape".into()),
                    });
                }
                c if c.is_control() => {
                    return Err("Unescaped control character in DDlog string".into())
                }
                c => result.push(c),
            }
        }
    }
    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self.rest.get(..4).ok_or("Short Unicode escape")?;
        if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("Invalid Unicode escape".into());
        }
        self.rest = &self.rest[4..];
        u32::from_str_radix(digits, 16).map_err(|e| e.to_string())
    }
    fn int(&mut self) -> Result<Value, String> {
        self.ws();
        let mut end = usize::from(self.rest.starts_with('-'));
        while self
            .rest
            .as_bytes()
            .get(end)
            .is_some_and(u8::is_ascii_digit)
        {
            end += 1;
        }
        let text = &self.rest[..end];
        let n = text
            .parse::<i64>()
            .map_err(|_| "Expected signed 64-bit integer")?;
        self.rest = &self.rest[end..];
        Ok(Value::from(n))
    }
}

/// Decode only full record snapshots (`R_name{.f0 = ..., .f1 = ...}`).
/// Delta weights, wrong predicates, duplicate rows, trailing data, missing or
/// reordered fields and values outside the declared schema are rejected.
pub fn decode_rows(
    text: &str,
    predicate: &str,
    fields: &[String],
) -> Result<Vec<Vec<Value>>, String> {
    if !crate::lower::ident(predicate)
        || fields.is_empty()
        || fields.iter().any(|f| f != "int" && f != "string")
    {
        return Err("Unsupported row schema".into());
    }
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut cursor = Cursor { rest: line };
        cursor.token(&format!("R_{predicate}"))?;
        cursor.token("{")?;
        let mut row = Vec::new();
        for (index, field) in fields.iter().enumerate() {
            if index != 0 {
                cursor.token(",")?;
            }
            cursor.token(&format!(".f{index}"))?;
            cursor.token("=")?;
            row.push(if field == "int" {
                cursor.int()?
            } else {
                Value::String(cursor.string()?)
            });
        }
        cursor.token("}")?;
        cursor.ws();
        if !cursor.rest.is_empty() {
            return Err("Trailing data in DDlog row".into());
        }
        let identity = serde_json::to_string(&row).map_err(|e| e.to_string())?;
        if !seen.insert(identity) {
            return Err("Duplicate DDlog snapshot row".into());
        }
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fields() -> Vec<String> {
        vec!["int".into(), "string".into()]
    }
    #[test]
    fn native_records_signed_values_and_escaped_strings() {
        let text =
            r#"R_item{.f0 = -9223372036854775808, .f1 = "a\n\"b\"\\é\u{8}\u{1f680}\uD83D\uDE80"}"#;
        assert_eq!(
            decode_rows(text, "item", &fields()).unwrap(),
            vec![vec![json!(i64::MIN), json!("a\n\"b\"\\é\u{8}🚀🚀")]]
        );
        assert!(decode_rows(" \n", "item", &fields()).unwrap().is_empty());
    }
    #[test]
    fn malformed_or_ambiguous_snapshots_fail_closed() {
        for text in [
            "R_other{.f0 = 1, .f1 = \"x\"}",
            "R_item{.f1 = 1, .f0 = \"x\"}",
            "R_item{.f0 = 1}",
            "R_item{.f0 = 1, .f1 = 2}",
            "R_item{.f0 = 9223372036854775808, .f1 = \"x\"}",
            "R_item{.f0 = 1, .f1 = \"x\"}: +1",
            "R_item{.f0 = 1, .f1 = \"x\"} garbage",
            "R_item{.f0 = 1, .f1 = \"\\q\"}",
            "R_item{.f0 = 1, .f1 = \"\\u{d800}\"}",
            "R_item{.f0 = 1, .f1 = \"\\uD800x\"}",
            "R_item{.f0 = 1, .f1 = \"unterminated}",
            "R_item{.f0 = 1, .f1 = \"x\"}\nR_item{.f0 = 1, .f1 = \"x\"}",
        ] {
            assert!(decode_rows(text, "item", &fields()).is_err(), "{text}");
        }
    }
}
