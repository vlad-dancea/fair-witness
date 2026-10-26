//! Paths as one line of text that the model can pass back to its tools.

use std::fmt::Write as _;
use std::path::{self, Path, PathBuf};
use std::str::Chars;

/// Writes `path` with `/` between its parts on every platform, a backslash in a name as `\\`,
/// line breaks and other control characters as `\n`, `\r`, `\t` or `\u{1b}`, and each byte that
/// is not part of a UTF-8 character as `\xe9`, so that a name cannot break the line it is on or
/// pass for other text, and every name can be named.
pub fn path(path: &Path) -> String {
    let mut text = String::new();
    for chunk in bytes(path).utf8_chunks() {
        for c in chunk.valid().chars() {
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
        for byte in chunk.invalid() {
            let _ = write!(text, "\\x{byte:02x}");
        }
    }
    text
}

/// Reads a path written by [`path`]. Characters the model sends unescaped stay as they are.
pub fn unescape(text: &str) -> Result<PathBuf, String> {
    let mut path = Vec::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        let c = if c != '\\' {
            c
        } else {
            match chars.next() {
                Some('\\') => '\\',
                Some('n') => '\n',
                Some('r') => '\r',
                Some('t') => '\t',
                Some('u') => code_point(&mut chars).ok_or(
                    "`\\u` needs a character code in braces, such as `\\u{1b}`.".to_string(),
                )?,
                Some('x') => {
                    path.push(byte(&mut chars).ok_or(
                        "`\\x` needs two hexadecimal digits, such as `\\xe9`.".to_string(),
                    )?);
                    continue;
                }
                Some(other) => {
                    return Err(format!(
                        "`\\{other}` is not an escape. Write a backslash as `\\\\`."
                    ));
                }
                None => return Err("ends in a lone backslash. Write one as `\\\\`.".to_string()),
            }
        };
        path.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
    }
    from_bytes(path)
}

#[cfg(unix)]
fn bytes(path: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes()
}

/// Windows names are UTF-16, and the rare one that is not valid UTF-16 shows bytes that
/// [`unescape`] cannot turn back.
#[cfg(not(unix))]
fn bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}

#[cfg(unix)]
fn from_bytes(bytes: Vec<u8>) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn from_bytes(bytes: Vec<u8>) -> Result<PathBuf, String> {
    String::from_utf8(bytes)
        .map(PathBuf::from)
        .map_err(|_| "`\\x` escapes here must spell UTF-8, as names are Unicode.".to_string())
}

/// Reads the `{1b}` after `\u`.
fn code_point(chars: &mut Chars) -> Option<char> {
    let (hex, rest) = chars.as_str().strip_prefix('{')?.split_once('}')?;
    let c = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)?;
    *chars = rest.chars();
    Some(c)
}

/// Reads the `e9` after `\x`.
fn byte(chars: &mut Chars) -> Option<u8> {
    let rest = chars.as_str();
    let hex = rest
        .get(..2)
        .filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))?;
    let byte = u8::from_str_radix(hex, 16).ok()?;
    *chars = rest[2..].chars();
    Some(byte)
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
    #[cfg(unix)] // Only Unix names can hold bytes that are not UTF-8.
    fn writes_bytes_that_are_not_utf8_and_reads_them_back() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // Latin-1 é, a UTF-8 é cut short, a byte that never starts a character, and a name
        // that looks like an escape.
        let name = Path::new(OsStr::from_bytes(b"latin1-\xe9t\xe9 \xc3 \xff \\xe9.csv"));
        let written = path(name);

        assert_eq!(written, "latin1-\\xe9t\\xe9 \\xc3 \\xff \\\\xe9.csv");
        assert_eq!(unescape(&written).unwrap(), name);
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
        // `from_str_radix` alone would take the sign.
        assert_eq!(
            unescape("a\\x+e.csv"),
            Err("`\\x` needs two hexadecimal digits, such as `\\xe9`.".to_string())
        );
        assert_eq!(
            unescape("a\\"),
            Err("ends in a lone backslash. Write one as `\\\\`.".to_string())
        );
    }
}
