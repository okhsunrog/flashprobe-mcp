//! Turning a byte stream into rendered lines. In text mode this is a plain
//! newline splitter; in defmt mode (added in a later milestone) it will deframe
//! and decode against the ELF. The capture loop is generic over `Decode`, so
//! both sources feed the same downstream stage.

/// One rendered line of output. `level`/`module` are populated only by the defmt
/// decoder (added later); the text decoder leaves them `None`.
pub struct Line {
    pub text: String,
}

impl Line {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

pub trait Decode {
    /// Feed raw bytes; return any newly completed lines.
    fn push(&mut self, bytes: &[u8]) -> Vec<Line>;

    /// The un-terminated tail not yet emitted as a [`Line`] (text mode). Stop
    /// conditions are also tested against this so a match fires before the
    /// terminating newline arrives. `None` when there is no pending tail.
    fn pending(&self) -> Option<&str> {
        None
    }
}

/// Plain text decoder: accumulates bytes and emits a [`Line`] per `\n`. A
/// trailing `\r` is preserved in the line text (so a rejoin reproduces the
/// original CRLF stream); the un-terminated tail is exposed via `pending`.
pub struct TextDecoder {
    buf: String,
}

impl TextDecoder {
    pub fn new() -> Self {
        Self { buf: String::new() }
    }
}

impl Decode for TextDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<Line> {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        let mut out = Vec::new();
        while let Some(i) = self.buf.find('\n') {
            let mut line: String = self.buf.drain(..=i).collect();
            line.pop(); // drop the '\n'; keep any trailing '\r'
            out.push(Line::text(line));
        }
        out
    }

    fn pending(&self) -> Option<&str> {
        if self.buf.is_empty() {
            None
        } else {
            Some(&self.buf)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_complete_lines_and_holds_partial() {
        let mut d = TextDecoder::new();
        // first chunk: one complete line + a partial
        let lines = d.push(b"hello\nwor");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "hello");
        assert_eq!(d.pending(), Some("wor"));

        // second chunk completes the partial and starts another
        let lines = d.push(b"ld\nmore");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "world");
        assert_eq!(d.pending(), Some("more"));
    }

    #[test]
    fn preserves_carriage_return() {
        let mut d = TextDecoder::new();
        let lines = d.push(b"a\r\nb\r\n");
        assert_eq!(lines[0].text, "a\r");
        assert_eq!(lines[1].text, "b\r");
        assert_eq!(d.pending(), None);
    }

    #[test]
    fn no_pending_when_buffer_empty() {
        let mut d = TextDecoder::new();
        assert_eq!(d.pending(), None);
        d.push(b"x\n");
        assert_eq!(d.pending(), None);
    }
}
