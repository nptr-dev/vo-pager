use std::io::{self, Read, Write};

/// Removes escape sequences without retaining CSI, OSC, or DCS payloads.
#[derive(Default)]
pub(crate) enum Text {
    #[default]
    Ground,
    Escape,
    Csi,
    String,
    StringEscape,
}

impl Text {
    pub(crate) fn push(&mut self, byte: u8) -> bool {
        match self {
            Self::Ground if byte == 0x1b => *self = Self::Escape,
            Self::Ground => return true,
            Self::Escape => {
                *self = match byte {
                    b'[' => Self::Csi,
                    b']' | b'P' | b'X' | b'^' | b'_' => Self::String,
                    0x20..=0x2f => Self::Escape,
                    _ => Self::Ground,
                }
            }
            Self::Csi if (0x40..=0x7e).contains(&byte) => *self = Self::Ground,
            Self::Csi => {}
            Self::String if byte == 7 => *self = Self::Ground,
            Self::String if byte == 0x1b => *self = Self::StringEscape,
            Self::String => {}
            Self::StringEscape => {
                *self = if byte == b'\\' {
                    Self::Ground
                } else {
                    Self::String
                }
            }
        }
        false
    }
}

/// Terminal fallback has no cursor controls; redirected output remains byte exact.
pub fn copy_plain(mut input: impl Read, mut output: impl Write) -> io::Result<()> {
    let mut text = Text::default();
    let mut bytes = [0u8; 65536];
    let mut plain = Vec::with_capacity(bytes.len());
    loop {
        let count = input.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        plain.clear();
        for byte in &bytes[..count] {
            if text.push(*byte) {
                match *byte {
                    b'\n' | b'\t' => plain.push(*byte),
                    b'\r' => {}
                    0..=31 | 127 => {
                        plain.push(b'^');
                        plain.push(*byte ^ 0x40);
                    }
                    _ => plain.push(*byte),
                }
            }
        }
        output.write_all(&plain)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_removes_unbounded_escape_strings_and_retains_text() {
        let bytes = b"\x1b[31mred\x1b[0m\x1b]0;title\x07\x1b[2J\n\x1bPpayload\x1b\\tail\r\n";
        let mut output = Vec::new();
        copy_plain(bytes.as_slice(), &mut output).unwrap();
        assert_eq!(output, b"red\ntail\n");
    }
}
