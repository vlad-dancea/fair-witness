//! Paths as one line of text that the model can pass back to its tools.

use std::fmt::Write as _;
use std::path::{self, Path, PathBuf};
use std::str::Chars;

/// Writes `path` with `/` between its parts on every platform, a backslash in a name as `\\`,
/// and line breaks and other control characters as `\n`, `\r`, `\t` or `\u{1b}`, so that a name
/// cannot break the line it is on or pass for other text.
pub fn path(path: &Path) -> String {
    let mut text = String::new();
    // A name that is not UTF-8 loses its odd bytes here, so the model cannot name it yet.
    for c in path.to_string_lossy().chars() {
        match c {
            '\\' if path::is_separator(c) => text.push('/'),
            '\\' => text.push_str("\\\\"),
            '\n' => text.push_str("\\n"),
            '\r' => text.push_str("\\r"),
            '\t' => text.push_str("\\t"),
            // Line and paragraph separators break lines too, though they are not controls.
            c if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') => {
                let _ = write!(text, "\\u{{{:x}}}", u32::from(c));
            }
            c => text.push(c),
        }
    }
    text
}

/// Reads a path written by [`path`]. Characters the model sends unescaped stay as they are.
pub fn unescape(text: &str) -> Result<PathBuf, String> {
    let mut path = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            path.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => path.push('\\'),
            Some('n') => path.push('\n'),
            Some('r') => path.push('\r'),
            Some('t') => path.push('\t'),
            Some('u') => {
                path.push(code_point(&mut chars).ok_or(
                    "`\\u` needs a character code in braces, such as `\\u{1b}`.".to_string(),
                )?)
            }
            Some(other) => {
                return Err(format!(
                    "`\\{other}` is not an escape. Write a backslash as `\\\\`."
                ));
            }
            None => return Err("ends in a lone backslash. Write one as `\\\\`.".to_string()),
        }
    }
    Ok(PathBuf::from(path))
}

/// Reads the `{1b}` after `\u`.
fn code_point(chars: &mut Chars) -> Option<char> {
    let (hex, rest) = chars.as_str().strip_prefix('{')?.split_once('}')?;
    let c = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)?;
    *chars = rest.chars();
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)] // On Windows a backslash separates names and cannot be in one.
    fn writes_every_name_on_one_line_and_reads_it_back() {
        let name = "a\\n b\nc\r\td\u{1b}[31m\u{85}e\u{2028}f — Größe 土壤.csv";
        let written = path(&Path::new("data").join(name));

        assert_eq!(
            written,
            "data/a\\\\n b\\nc\\r\\td\\u{1b}[31m\\u{85}e\\u{2028}f — Größe 土壤.csv"
        );
        assert_eq!(unescape(&written).unwrap(), Path::new("data").join(name));
    }

    #[test]
    fn explains_what_it_cannot_read() {
        assert_eq!(
            unescape("data\\q.csv"),
            Err("`\\q` is not an escape. Write a backslash as `\\\\`.".to_string())
        );
        assert_eq!(
            unescape("a\\u{110000}.csv"),
            Err("`\\u` needs a character code in braces, such as `\\u{1b}`.".to_string())
        );
        assert_eq!(
            unescape("a\\"),
            Err("ends in a lone backslash. Write one as `\\\\`.".to_string())
        );
    }
}
