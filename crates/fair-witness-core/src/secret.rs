//! Hiding an API key in what an API answered, which may repeat it.

/// Stands in for the API key.
const HIDDEN_KEY: &str = "[API key]";

/// Whether `hide` finds `key` behind every escape JSON has for it: a key of printable ASCII
/// without a backslash, which could not be told from the backslashes of the escapes.
pub fn can_hide(key: &str) -> bool {
    key.chars().all(|c| c.is_ascii_graphic() && c != '\\')
}

/// Replaces `secret` in `line`, in case an error message from the API repeats the key. JSON may
/// escape any character of it, such as `/` as `\/` or `s` as `\u0073`, and a line that quotes a
/// JSON response body, such as an event, holds it escaped twice.
pub fn hide(line: String, secret: Option<&str>) -> String {
    let Some(secret) = secret.filter(|secret| !secret.is_empty()) else {
        return line;
    };
    let line = replace_escaped(line, secret);
    if !reveals(&line, secret) {
        return line;
    }
    // Escapes can escape the backslashes of other escapes, so what is left of the key is
    // found only by reading the escapes over and over. Every string that holds it goes.
    if let Ok(mut event) = serde_json::from_str::<serde_json::Value>(&line) {
        wipe(&mut event, secret);
        let wiped = event.to_string();
        if !reveals(&wiped, secret) {
            return wiped;
        }
    }
    HIDDEN_KEY.to_string()
}

/// `line` with each place that holds `secret`, escaped or not, replaced.
fn replace_escaped(line: String, secret: &str) -> String {
    let secret: Vec<char> = secret.chars().collect();
    let mut hidden = String::with_capacity(line.len());
    let mut rest = line.as_str();
    while let Some(next) = rest.chars().next() {
        match escaped_len(rest, &secret) {
            Some(len) => {
                hidden.push_str(HIDDEN_KEY);
                rest = &rest[len..];
            }
            None => {
                hidden.push(next);
                rest = &rest[next.len_utf8()..];
            }
        }
    }
    hidden
}

/// How long `secret` is at the start of `text`, with each of its characters escaped as JSON
/// any number of times or not at all.
fn escaped_len(text: &str, secret: &[char]) -> Option<usize> {
    let Some((&c, secret_rest)) = secret.split_first() else {
        return Some(0);
    };
    let after = text.trim_start_matches('\\');
    let slashes = text.len() - after.len();
    // The `u` that starts an escape can also be the key's own `u`, so both readings are tried.
    [
        (slashes > 0 && unicode_escape(after) == Some(c)).then_some(slashes + "u0000".len()),
        after.starts_with(c).then_some(slashes + c.len_utf8()),
    ]
    .into_iter()
    .flatten()
    .find_map(|at| escaped_len(&text[at..], secret_rest).map(|rest| at + rest))
}

/// Replaces each string in `value` that reveals `secret`.
fn wipe(value: &mut serde_json::Value, secret: &str) {
    match value {
        serde_json::Value::String(text) if reveals(text, secret) => *text = HIDDEN_KEY.to_string(),
        serde_json::Value::Array(items) => items.iter_mut().for_each(|item| wipe(item, secret)),
        serde_json::Value::Object(fields) => {
            fields.values_mut().for_each(|field| wipe(field, secret))
        }
        _ => {}
    }
}

/// Whether `text` holds `secret`, as it is or behind JSON escapes, as deep as they go.
fn reveals(text: &str, secret: &str) -> bool {
    let mut text = text.to_string();
    loop {
        if text.contains(secret) {
            return true;
        }
        // Each read makes the text shorter, so this ends.
        let read = unescape(&text);
        if read == text {
            return false;
        }
        text = read;
    }
}

/// `text` with the JSON escapes in it read once, wherever they stand, and the ones that do
/// not matter for finding a key, such as `\n`, left as they are.
fn unescape(text: &str) -> String {
    let mut read = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('\\') {
        read.push_str(&rest[..at]);
        let escape = &rest[at + 1..];
        let (c, len) = match escape.chars().next() {
            Some(c @ ('"' | '\\' | '/')) => (c, 1),
            Some('u') => match unicode_escape(escape) {
                Some(c) => (c, "u0000".len()),
                None => ('\\', 0),
            },
            _ => ('\\', 0),
        };
        read.push(c);
        rest = &escape[len..];
    }
    read.push_str(rest);
    read
}

/// The character that a `uXXXX` escape at the start of `text` stands for.
fn unicode_escape(text: &str) -> Option<char> {
    let hex = text.strip_prefix('u')?.get(..4)?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    char::from_u32(u32::from_str_radix(hex, 16).ok()?)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn hides_an_api_key_however_json_escapes_it() {
        // An error event that names the key and quotes the response body of the API, which
        // repeats it with escapes that serde_json does not use.
        let event = |key: &str, in_body: &str| {
            let body = format!(r#"{{"error":"{in_body} is not a key"}}"#);
            let error = format!("{key} failed: Response body:\n{body}");
            json!({ "event": "review_failed", "error": error }).to_string()
        };
        let key = r#"sk-u/"secret"key"#;
        let escape = |c: char| match c {
            '"' | '/' => format!("\\{c}"),
            c => c.to_string(),
        };

        // Each character in turn as a Unicode escape, and the others as JSON may write them.
        for i in 0..key.chars().count() {
            let in_body: String = key
                .chars()
                .enumerate()
                .map(|(j, c)| {
                    if i == j {
                        format!("\\u{:04x}", u32::from(c))
                    } else {
                        escape(c)
                    }
                })
                .collect();
            assert_eq!(
                hide(event(key, &in_body), Some(key)),
                event(HIDDEN_KEY, HIDDEN_KEY),
                "{in_body}"
            );
        }
    }

    #[test]
    fn hides_an_api_key_behind_escaped_escapes() {
        let key = "sk-secret";
        // A response body whose backslash, written as an escape, starts the escape of `s`
        // once the body is read twice.
        let body = r#"{"detail": "{\"error\":\"\u005cu0073k-secret\"}"}"#;
        let event = json!({ "event": "review_failed", "set": 1, "error": body }).to_string();

        let hidden: serde_json::Value = serde_json::from_str(&hide(event, Some(key))).unwrap();

        assert_eq!(
            hidden,
            json!({ "event": "review_failed", "set": 1, "error": HIDDEN_KEY })
        );
        assert_eq!(
            hide(r"error: \u005cu0073k-secret".to_string(), Some(key)),
            HIDDEN_KEY
        );
    }

    #[test]
    fn leaves_text_alone_without_a_key() {
        assert_eq!(hide("sk-secret".to_string(), Some("")), "sk-secret");
        assert_eq!(hide("sk-secret".to_string(), None), "sk-secret");
    }

    #[test]
    fn takes_only_keys_it_can_hide() {
        assert_eq!(
            [
                "sk-u/\"secret\"+=~",
                "sk-\\secret",
                "sk-\tsecret",
                "sk-secret ",
                "sk-\u{1f600}-secret"
            ]
            .map(can_hide),
            [true, false, false, false, false]
        );
    }
}
