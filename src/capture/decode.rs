//! Turning a byte stream into rendered lines. `TextDecoder` is a plain newline
//! splitter; `DefmtDecoder` deframes and decodes against the firmware ELF. The
//! capture loop is generic over `Decode`, so serial and RTT sources feed the
//! same downstream stage in either mode.

use defmt_decoder::{DecodeError, Locations, StreamDecoder, Table};

/// Try to load a defmt [`Table`] from an ELF file. `Ok(None)` means the file has
/// no `.defmt` section → text mode. The bytes are returned alongside because
/// location lookup (`get_locations`) needs them again. `Err` only on read/parse
/// failure.
pub fn load_defmt_table(elf_path: &str) -> Result<Option<(Table, Vec<u8>)>, String> {
    let bytes =
        std::fs::read(elf_path).map_err(|e| format!("Failed to read ELF '{elf_path}': {e}"))?;
    match Table::parse(&bytes) {
        Ok(Some(table)) => Ok(Some((table, bytes))),
        Ok(None) => Ok(None),
        Err(e) => Err(format!("Failed to parse defmt table from '{elf_path}': {e}")),
    }
}

/// Log level, mirroring `defmt`'s. Ordered so `Trace < … < Error`, which is what
/// the `level` (minimum to show) and `stop_on_level` predicates rely on. Kept as
/// our own type so the pipeline carries no defmt types in text mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }

    /// Parse a level name (case-insensitive) for the `level` / `stop_on_level` args.
    pub fn parse(s: &str) -> Result<Level, String> {
        match s.to_ascii_lowercase().as_str() {
            "trace" => Ok(Level::Trace),
            "debug" => Ok(Level::Debug),
            "info" => Ok(Level::Info),
            "warn" | "warning" => Ok(Level::Warn),
            "error" => Ok(Level::Error),
            other => Err(format!(
                "Unknown level '{other}' (use trace/debug/info/warn/error)"
            )),
        }
    }
}

impl From<defmt_parser::Level> for Level {
    fn from(l: defmt_parser::Level) -> Self {
        match l {
            defmt_parser::Level::Trace => Level::Trace,
            defmt_parser::Level::Debug => Level::Debug,
            defmt_parser::Level::Info => Level::Info,
            defmt_parser::Level::Warn => Level::Warn,
            defmt_parser::Level::Error => Level::Error,
        }
    }
}

/// One rendered line of output. `level`/`module` are populated only by the defmt
/// decoder; the text decoder leaves them `None`.
pub struct Line {
    pub text: String,
    pub level: Option<Level>,
    pub module: Option<String>,
}

impl Line {
    /// A plain text line with no structured metadata.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            level: None,
            module: None,
        }
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

/// Decode health, surfaced after a capture so the agent can spot ELF/firmware
/// version skew (which yields garbage, not an error, from defmt).
#[derive(Debug, Clone, Copy)]
pub struct DefmtStats {
    pub decoded: usize,
    pub malformed: usize,
}

/// defmt decoder: feeds bytes to the ELF-derived `StreamDecoder` (which picks
/// rzCOBS-vs-raw framing from the ELF's encoding metadata, so the same decoder
/// serves serial and RTT) and renders each frame to a [`Line`] carrying its
/// level and module. Borrows the `Table` for `'a` via the stream decoder.
pub struct DefmtDecoder<'a> {
    sd: Box<dyn StreamDecoder + Send + Sync + 'a>,
    locations: Option<Locations>,
    has_timestamp: bool,
    decoded: usize,
    malformed: usize,
}

impl<'a> DefmtDecoder<'a> {
    pub fn new(
        sd: Box<dyn StreamDecoder + Send + Sync + 'a>,
        locations: Option<Locations>,
        has_timestamp: bool,
    ) -> Self {
        Self {
            sd,
            locations,
            has_timestamp,
            decoded: 0,
            malformed: 0,
        }
    }

    pub fn stats(&self) -> DefmtStats {
        DefmtStats {
            decoded: self.decoded,
            malformed: self.malformed,
        }
    }
}

impl Decode for DefmtDecoder<'_> {
    fn push(&mut self, bytes: &[u8]) -> Vec<Line> {
        self.sd.received(bytes);
        let mut out = Vec::new();
        loop {
            match self.sd.decode() {
                Ok(frame) => {
                    self.decoded += 1;
                    let level = frame.level().map(Level::from);
                    let module = self
                        .locations
                        .as_ref()
                        .and_then(|locs| locs.get(&frame.index()))
                        .map(|loc| loc.module.clone());

                    let mut text = String::new();
                    if self.has_timestamp
                        && let Some(ts) = frame.display_timestamp()
                    {
                        text.push_str(&ts.to_string());
                        text.push(' ');
                    }
                    if let Some(lv) = level {
                        text.push_str(lv.as_str());
                        text.push(' ');
                    }
                    text.push_str(&frame.display_message().to_string());

                    out.push(Line {
                        text,
                        level,
                        module,
                    });
                }
                // No frame separator yet — wait for more bytes.
                Err(DecodeError::UnexpectedEof) => break,
                // The stream decoder drains past the bad frame, so we can keep
                // going; it resyncs on the next separator.
                Err(DecodeError::Malformed) => self.malformed += 1,
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_complete_lines_and_holds_partial() {
        let mut d = TextDecoder::new();
        let lines = d.push(b"hello\nwor");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "hello");
        assert_eq!(d.pending(), Some("wor"));

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

    #[test]
    fn level_ordering_and_parse() {
        assert!(Level::Trace < Level::Info && Level::Info < Level::Error);
        assert_eq!(Level::parse("ERROR").unwrap(), Level::Error);
        assert_eq!(Level::parse("warning").unwrap(), Level::Warn);
        assert!(Level::parse("bogus").is_err());
    }
}
