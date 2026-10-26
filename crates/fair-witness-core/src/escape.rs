//! Paths as one line of text that the model can pass back to its tools.

use std::borrow::Cow;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{self, Path, PathBuf};
use std::str::{self, Chars};

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
fn bytes(path: &Path) -> Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    Cow::Borrowed(path.as_os_str().as_bytes())
}

/// Windows names are UTF-16, so this is their WTF-8, which a lone surrogate of a name that is
/// not valid UTF-16 turns into bytes that are not UTF-8.
#[cfg(windows)]
fn bytes(path: &Path) -> Cow<'_, [u8]> {
    use std::os::windows::ffi::OsStrExt;
    Cow::Owned(wtf8(path.as_os_str().encode_wide()))
}

#[cfg(unix)]
fn from_bytes(bytes: Vec<u8>) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(windows)]
fn from_bytes(bytes: Vec<u8>) -> Result<PathBuf, String> {
    use std::os::windows::ffi::OsStringExt;
    let wide = wide(&bytes).ok_or(
        "`\\x` escapes here must spell UTF-8, or a lone surrogate such as `\\xed\\xa0\\x80`."
            .to_string(),
    )?;
    Ok(PathBuf::from(OsString::from_wide(&wide)))
}

/// Writes UTF-16 as WTF-8: UTF-8, with a lone surrogate as the three bytes UTF-8 would give it
/// if it were a character.
#[cfg(any(windows, test))]
fn wtf8(units: impl IntoIterator<Item = u16>) -> Vec<u8> {
    let mut bytes = Vec::new();
    for c in char::decode_utf16(units) {
        match c {
            Ok(c) => bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
            Err(lone) => {
                let unit = lone.unpaired_surrogate();
                bytes.extend([
                    0xed,
                    0x80 | ((unit >> 6) & 0x3f) as u8,
                    0x80 | (unit & 0x3f) as u8,
                ]);
            }
        }
    }
    bytes
}

/// Reads [`wtf8`] back into UTF-16.
#[cfg(any(windows, test))]
fn wide(mut bytes: &[u8]) -> Option<Vec<u16>> {
    let mut units = Vec::with_capacity(bytes.len());
    while !bytes.is_empty() {
        let valid = match str::from_utf8(bytes) {
            Ok(valid) => valid,
            Err(e) => str::from_utf8(&bytes[..e.valid_up_to()]).ok()?,
        };
        units.extend(valid.encode_utf16());
        bytes = &bytes[valid.len()..];
        match bytes {
            [] => {}
            [0xed, high @ 0xa0..=0xbf, low @ 0x80..=0xbf, rest @ ..] => {
                units.push(0xd000 | (u16::from(high & 0x3f) << 6) | u16::from(low & 0x3f));
                bytes = rest;
            }
            _ => return None,
        }
    }
    Some(units)
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
    #[cfg(windows)] // Only Windows names can hold a lone surrogate.
    fn writes_a_name_that_is_not_utf16_and_reads_it_back() {
        use std::os::windows::ffi::OsStringExt;

        let name = PathBuf::from(OsString::from_wide(&[0x61, 0xd800, 0x2e, 0x63]));
        let written = path(&name);

        assert_eq!(written, "a\\xed\\xa0\\x80.c");
        assert_eq!(unescape(&written).unwrap(), name);
    }

    #[test]
    fn reads_back_the_lone_surrogates_of_windows_names() {
        // A lone high surrogate, a lone low one and a pair, which makes 😀.
        let units = [0x61, 0xd800, 0x62, 0xdfff, 0xd83d, 0xde00];
        let bytes = wtf8(units);

        assert_eq!(bytes, b"a\xed\xa0\x80b\xed\xbf\xbf\xf0\x9f\x98\x80");
        assert_eq!(wide(&bytes), Some(units.to_vec()));
        assert_eq!(wide(b"a\xff"), None);
        assert_eq!(wide(b"a\xed\xa0"), None);
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
