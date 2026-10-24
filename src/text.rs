//! Byte-level output cleanup shared by the output panel.  Kept out of the
//! backend so saved files preserve exactly what the driver sent.

use std::borrow::Cow;

/// Remove ANSI CSI/OSC escape sequences.  Sage and matplotlib occasionally emit
/// colours or terminal-title sequences even when stdout is a pipe; a notebook
/// must show the traceback, not the control bytes.
pub fn strip_ansi(text: &str) -> Cow<'_, str> {
    if !text.contains('\x1b') {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('[') => {
                // CSI: parameter/intermediate bytes, then a final byte 0x40..=0x7e.
                for next in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                // OSC: terminated by BEL or ST (ESC \).
                while let Some(next) = chars.next() {
                    if next == '\x07' {
                        break;
                    }
                    if next == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {}
            None => break,
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

    #[test]
    fn plain_text_is_borrowed() {
        assert!(matches!(
            strip_ansi("a+b"),
            std::borrow::Cow::Borrowed("a+b")
        ));
    }

    #[test]
    fn csi_colours_are_removed() {
        assert_eq!(strip_ansi("\x1b[31merror\x1b[0m: bad"), "error: bad");
        assert_eq!(strip_ansi("a\x1b[1;34mb\x1b[0mc"), "abc");
    }

    #[test]
    fn osc_titles_are_removed() {
        assert_eq!(strip_ansi("\x1b]0;title\x07ok"), "ok");
        assert_eq!(strip_ansi("\x1b]8;;https://example.test\x1b\\link"), "link");
    }

    #[test]
    fn incomplete_escape_is_dropped() {
        assert_eq!(strip_ansi("before\x1b[31"), "before");
        assert_eq!(strip_ansi("before\x1b"), "before");
    }
}
