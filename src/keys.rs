/// Process special key sequences in input text, converting them to their
/// ANSI escape code equivalents.
///
/// Supported sequences:
/// - `^C`, `^A`..`^Z` — control characters
/// - `\xNN` — hex escapes
/// - `\n`, `\r`, `\t` — common escapes
/// - `[UP]`, `[DOWN]`, `[LEFT]`, `[RIGHT]` — arrow keys
/// - `[HOME]`, `[END]`, `[PGUP]`, `[PGDN]`, `[DELETE]`, `[BACKSPACE]`
/// - `[F1]`..`[F12]`
/// - `[TAB]`, `[SHIFT+TAB]`, `[ENTER]`, `[ESCAPE]`
/// - `[CTRL+A]`..`[CTRL+Z]`
/// - `[PASTE_START]`, `[PASTE_END]`
pub fn process_special_keys(input: &str) -> Vec<u8> {
    let mut result = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'^' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next.is_ascii_uppercase() {
                result.push(next - b'A' + 1);
                i += 2;
                continue;
            } else if next.is_ascii_lowercase() {
                result.push(next - b'a' + 1);
                i += 2;
                continue;
            }
        }

        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' => {
                    result.push(b'\n');
                    i += 2;
                    continue;
                }
                b'r' => {
                    result.push(b'\r');
                    i += 2;
                    continue;
                }
                b't' => {
                    result.push(b'\t');
                    i += 2;
                    continue;
                }
                b'x' if i + 3 < bytes.len() => {
                    let hex = &input[i + 2..i + 4];
                    if let Ok(val) = u8::from_str_radix(hex, 16) {
                        result.push(val);
                        i += 4;
                        continue;
                    }
                }
                _ => {}
            }
        }

        if bytes[i] == b'[' {
            if let Some(end) = input[i..].find(']') {
                let tag = &input[i + 1..i + end];
                if let Some(seq) = bracket_sequence(tag) {
                    result.extend_from_slice(seq);
                    i += end + 1;
                    continue;
                }
            }
        }

        result.push(bytes[i]);
        i += 1;
    }

    result
}

fn bracket_sequence(tag: &str) -> Option<&'static [u8]> {
    match tag {
        "UP" => Some(b"\x1b[A"),
        "DOWN" => Some(b"\x1b[B"),
        "RIGHT" => Some(b"\x1b[C"),
        "LEFT" => Some(b"\x1b[D"),
        "HOME" => Some(b"\x1b[H"),
        "END" => Some(b"\x1b[F"),
        "PGUP" => Some(b"\x1b[5~"),
        "PGDN" => Some(b"\x1b[6~"),
        "DELETE" => Some(b"\x1b[3~"),
        "BACKSPACE" => Some(b"\x7f"),
        "TAB" => Some(b"\t"),
        "SHIFT+TAB" => Some(b"\x1b[Z"),
        "ENTER" => Some(b"\r"),
        "ESCAPE" => Some(b"\x1b"),
        "F1" => Some(b"\x1bOP"),
        "F2" => Some(b"\x1bOQ"),
        "F3" => Some(b"\x1bOR"),
        "F4" => Some(b"\x1bOS"),
        "F5" => Some(b"\x1b[15~"),
        "F6" => Some(b"\x1b[17~"),
        "F7" => Some(b"\x1b[18~"),
        "F8" => Some(b"\x1b[19~"),
        "F9" => Some(b"\x1b[20~"),
        "F10" => Some(b"\x1b[21~"),
        "F11" => Some(b"\x1b[23~"),
        "F12" => Some(b"\x1b[24~"),
        "CTRL+A" => Some(b"\x01"),
        "CTRL+B" => Some(b"\x02"),
        "CTRL+C" => Some(b"\x03"),
        "CTRL+D" => Some(b"\x04"),
        "CTRL+E" => Some(b"\x05"),
        "CTRL+F" => Some(b"\x06"),
        "CTRL+G" => Some(b"\x07"),
        "CTRL+H" => Some(b"\x08"),
        "CTRL+I" => Some(b"\x09"),
        "CTRL+J" => Some(b"\x0a"),
        "CTRL+K" => Some(b"\x0b"),
        "CTRL+L" => Some(b"\x0c"),
        "CTRL+M" => Some(b"\x0d"),
        "CTRL+N" => Some(b"\x0e"),
        "CTRL+O" => Some(b"\x0f"),
        "CTRL+P" => Some(b"\x10"),
        "CTRL+Q" => Some(b"\x11"),
        "CTRL+R" => Some(b"\x12"),
        "CTRL+S" => Some(b"\x13"),
        "CTRL+T" => Some(b"\x14"),
        "CTRL+U" => Some(b"\x15"),
        "CTRL+V" => Some(b"\x16"),
        "CTRL+W" => Some(b"\x17"),
        "CTRL+X" => Some(b"\x18"),
        "CTRL+Y" => Some(b"\x19"),
        "CTRL+Z" => Some(b"\x1a"),
        "PASTE_START" => Some(b"\x1b[200~"),
        "PASTE_END" => Some(b"\x1b[201~"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passthrough() {
        assert_eq!(process_special_keys("hello"), b"hello");
    }

    #[test]
    fn ctrl_c() {
        assert_eq!(process_special_keys("^C"), vec![3]);
    }

    #[test]
    fn ctrl_lowercase_treated_same() {
        assert_eq!(process_special_keys("^c"), vec![3]);
    }

    #[test]
    fn hex_escape() {
        assert_eq!(process_special_keys("\\x1b"), vec![0x1b]);
    }

    #[test]
    fn common_escapes() {
        assert_eq!(process_special_keys("\\n\\r\\t"), vec![b'\n', b'\r', b'\t']);
    }

    #[test]
    fn enter_key() {
        assert_eq!(process_special_keys("ls[ENTER]"), b"ls\r");
    }

    #[test]
    fn arrow_keys() {
        assert_eq!(process_special_keys("[UP]"), b"\x1b[A");
        assert_eq!(process_special_keys("[DOWN]"), b"\x1b[B");
    }

    #[test]
    fn function_keys() {
        assert_eq!(process_special_keys("[F5]"), b"\x1b[15~");
    }

    #[test]
    fn unknown_bracket_passthrough() {
        assert_eq!(process_special_keys("[UNKNOWN]"), b"[UNKNOWN]");
    }

    #[test]
    fn caret_not_followed_by_letter() {
        assert_eq!(process_special_keys("^1"), b"^1");
    }
}
