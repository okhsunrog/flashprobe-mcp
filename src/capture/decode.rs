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
        Err(e) => Err(format!(
            "Failed to parse defmt table from '{elf_path}': {e}"
        )),
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

/// How defmt frames are framed on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefmtFraming {
    /// esp-println over serial: each rzCOBS frame is prefixed with the marker
    /// `0xFF 0x00` (so frames can be told apart from interleaved ASCII like boot
    /// logs / `println!`) and terminated with `0x00`. We deframe on the marker,
    /// decode each frame in isolation, and surface the bytes between frames as
    /// plain text lines.
    EspPrintln,
    /// A raw rzCOBS stream with no marker (defmt-rtt over RTT / probe-rs). Bytes
    /// feed straight into a persistent stream decoder. Only constructed by the
    /// probe-rs backend, so it is dead code in an espflash-only build.
    #[cfg_attr(not(feature = "probe-rs"), allow(dead_code))]
    Raw,
}

/// A run of bytes the delimiter has classified.
enum Chunk {
    /// A complete rzCOBS defmt frame, without its markers.
    Frame(Vec<u8>),
    /// Bytes outside any frame: boot ROM output, the bootloader log, `println!`.
    Text(Vec<u8>),
}

/// Extracts esp-println defmt frames from a byte stream by the `0xFF 0x00`
/// start marker / `0x00` end marker. Mirrors espflash's `FrameDelimiter`.
///
/// Bytes between frames are returned as [`Chunk::Text`] rather than discarded.
/// A serial stream routinely carries both: the ROM and the ESP-IDF bootloader
/// print plain text long before the application's first defmt frame, and
/// dropping it makes a target that fails early look like a target that said
/// nothing at all.
struct FrameDelimiter {
    buffer: Vec<u8>,
    in_frame: bool,
}

const FRAME_START: &[u8] = &[0xFF, 0x00];

impl FrameDelimiter {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
            in_frame: false,
        }
    }

    /// Feed bytes; return the frames and the interleaved text, in stream order.
    fn feed(&mut self, bytes: &[u8]) -> Vec<Chunk> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            // When inside a frame we look for the `0x00` terminator, skipping any
            // leading zeros; otherwise we look for the `0xFF 0x00` start marker.
            let (needle, start): (&[u8], usize) = if self.in_frame {
                match self.buffer.iter().position(|&b| b != 0) {
                    Some(p) => (&[0x00], p),
                    None => break,
                }
            } else {
                (FRAME_START, 0)
            };

            let found = self.buffer[start..]
                .windows(needle.len())
                .position(|w| w == needle);

            let Some(rel) = found else {
                if !self.in_frame {
                    // No frame is starting in what we hold. Release it as text
                    // now rather than waiting for a marker that may never come:
                    // a target that only ever prints text would otherwise stay
                    // silent forever. Keep a trailing `0xFF`, which may yet turn
                    // out to be the first half of a start marker.
                    let keep = usize::from(self.buffer.last() == Some(&FRAME_START[0]));
                    let upto = self.buffer.len() - keep;
                    if upto > 0 {
                        out.push(Chunk::Text(self.buffer[..upto].to_vec()));
                        self.buffer.drain(..upto);
                    }
                }
                break;
            };

            let consumed = start + rel + needle.len();
            if self.in_frame {
                out.push(Chunk::Frame(self.buffer[start..start + rel].to_vec()));
            } else if rel > 0 {
                // Text that preceded this frame's start marker.
                out.push(Chunk::Text(self.buffer[..rel].to_vec()));
            }
            self.in_frame = !self.in_frame;
            self.buffer.drain(..consumed);
        }
        out
    }
}

enum Framing<'a> {
    /// Persistent stream decoder for a raw rzCOBS stream (RTT).
    Raw(Box<dyn StreamDecoder + Send + Sync + 'a>),
    /// Marker-based deframer for esp-println serial streams.
    EspPrintln(FrameDelimiter),
}

/// defmt decoder. In `Raw` framing it feeds bytes to a persistent rzCOBS stream
/// decoder; in `EspPrintln` framing it deframes on the `0xFF 0x00` marker and
/// decodes each frame in isolation (so interleaved ASCII never corrupts a
/// frame). Each decoded frame becomes a [`Line`] carrying its level and module.
pub struct DefmtDecoder<'a> {
    table: &'a Table,
    framing: Framing<'a>,
    locations: Option<Locations>,
    has_timestamp: bool,
    decoded: usize,
    malformed: usize,
    /// Partial interleaved text line, awaiting its newline.
    text: String,
}

impl<'a> DefmtDecoder<'a> {
    pub fn new(
        table: &'a Table,
        locations: Option<Locations>,
        has_timestamp: bool,
        framing: DefmtFraming,
    ) -> Self {
        let framing = match framing {
            DefmtFraming::Raw => Framing::Raw(table.new_stream_decoder()),
            DefmtFraming::EspPrintln => Framing::EspPrintln(FrameDelimiter::new()),
        };
        Self {
            table,
            framing,
            locations,
            has_timestamp,
            decoded: 0,
            malformed: 0,
            text: String::new(),
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
    fn pending(&self) -> Option<&str> {
        (!self.text.is_empty()).then_some(self.text.as_str())
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<Line> {
        let mut out = Vec::new();
        match &mut self.framing {
            Framing::Raw(sd) => {
                sd.received(bytes);
                loop {
                    match sd.decode() {
                        Ok(frame) => {
                            self.decoded += 1;
                            out.push(line_from_frame(
                                &frame,
                                self.locations.as_ref(),
                                self.has_timestamp,
                            ));
                        }
                        // No frame separator yet — wait for more bytes.
                        Err(DecodeError::UnexpectedEof) => break,
                        // The stream decoder drains past the bad frame, so keep
                        // going; it resyncs on the next separator.
                        Err(DecodeError::Malformed) => self.malformed += 1,
                    }
                }
            }
            Framing::EspPrintln(delim) => {
                // Borrow split: take the chunks out first, then decode (needs
                // &self.table / &self.locations).
                let chunks = delim.feed(bytes);
                for chunk in chunks {
                    match chunk {
                        Chunk::Frame(fb) => {
                            // Each esp-println frame is a self-contained rzCOBS
                            // frame; feed it plus the terminating zero to a
                            // fresh decoder.
                            let mut sd = self.table.new_stream_decoder();
                            sd.received(&fb);
                            sd.received(&[0x00]);
                            match sd.decode() {
                                Ok(frame) => {
                                    self.decoded += 1;
                                    out.push(line_from_frame(
                                        &frame,
                                        self.locations.as_ref(),
                                        self.has_timestamp,
                                    ));
                                }
                                Err(_) => self.malformed += 1,
                            }
                        }
                        Chunk::Text(bytes) => {
                            // Interleaved plain text, split into lines the same
                            // way the text decoder does. Carries no level or
                            // module, so level filters leave it alone.
                            self.text.push_str(&String::from_utf8_lossy(&bytes));
                            while let Some(nl) = self.text.find('\n') {
                                let line: String = self.text.drain(..=nl).collect();
                                let line = line.trim_end_matches('\n');
                                if !line.is_empty() {
                                    out.push(Line::text(line));
                                }
                            }
                        }
                    }
                }
            }
        }
        out
    }
}

/// Free-function form of [`DefmtDecoder::line_from_frame`] to sidestep borrow
/// conflicts with the `&mut self.framing` in `push`.
fn line_from_frame(
    frame: &defmt_decoder::Frame<'_>,
    locations: Option<&Locations>,
    has_timestamp: bool,
) -> Line {
    let level = frame.level().map(Level::from);
    let module = locations
        .and_then(|locs| locs.get(&frame.index()))
        .map(|loc| loc.module.clone());

    let mut text = String::new();
    if has_timestamp && let Some(ts) = frame.display_timestamp() {
        text.push_str(&ts.to_string());
        text.push(' ');
    }
    if let Some(lv) = level {
        text.push_str(lv.as_str());
        text.push(' ');
    }
    text.push_str(&frame.display_message().to_string());

    Line {
        text,
        level,
        module,
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
    fn esp_println_framing_extracts_frames_and_keeps_text() {
        // The rzCOBS frame bytes are extracted, and the text on either side of
        // it is kept rather than dropped.
        let mut d = FrameDelimiter::new();
        assert_eq!(
            chunks(d.feed(b"boot log\xFF\x00\x06\x7E\x00more text")),
            vec![
                ("text", b"boot log".to_vec()),
                ("frame", vec![0x06, 0x7E]),
                ("text", b"more text".to_vec()),
            ]
        );
    }

    /// Renders chunks compactly so a test failure shows what actually came out.
    fn chunks(cs: Vec<Chunk>) -> Vec<(&'static str, Vec<u8>)> {
        cs.into_iter()
            .map(|c| match c {
                Chunk::Frame(b) => ("frame", b),
                Chunk::Text(b) => ("text", b),
            })
            .collect()
    }

    #[test]
    fn esp_println_framing_spans_feeds_and_back_to_back() {
        let mut d = FrameDelimiter::new();
        // A frame split across two feeds isn't emitted until its terminator arrives.
        assert!(d.feed(b"\xFF\x00fra").is_empty());
        // Completing frame + a second back-to-back frame.
        assert_eq!(
            chunks(d.feed(b"me\x00\xFF\x00f2\x00")),
            vec![
                ("frame", b"frame".to_vec()),
                ("frame", b"f2".to_vec()),
            ]
        );
    }

    #[test]
    fn text_before_a_frame_is_kept() {
        let mut d = FrameDelimiter::new();
        // What a serial line actually carries: bootloader output, then defmt.
        assert_eq!(
            chunks(d.feed(b"boot: hello\n\xFF\x00frame\x00")),
            vec![
                ("text", b"boot: hello\n".to_vec()),
                ("frame", b"frame".to_vec()),
            ]
        );
    }

    #[test]
    fn text_flows_without_waiting_for_a_frame() {
        // The regression this guards: a target that only ever prints text used
        // to stay silent, because the delimiter held everything back waiting
        // for a start marker that never arrived.
        let mut d = FrameDelimiter::new();
        assert_eq!(
            chunks(d.feed(b"ESP-ROM:esp32c5\n")),
            vec![("text", b"ESP-ROM:esp32c5\n".to_vec())]
        );
    }

    #[test]
    fn a_trailing_marker_byte_is_held_back() {
        let mut d = FrameDelimiter::new();
        // The final 0xFF may be the first half of a start marker, so it must not
        // be released as text yet.
        assert_eq!(
            chunks(d.feed(b"abc\xFF")),
            vec![("text", b"abc".to_vec())]
        );
        // Next feed completes the marker: the frame is found and no stray 0xFF
        // leaks into the text.
        assert_eq!(
            chunks(d.feed(b"\x00frame\x00")),
            vec![("frame", b"frame".to_vec())]
        );
    }

    #[test]
    fn text_and_frames_keep_their_order() {
        let mut d = FrameDelimiter::new();
        assert_eq!(
            chunks(d.feed(b"a\n\xFF\x00f1\x00b\n\xFF\x00f2\x00c\n")),
            vec![
                ("text", b"a\n".to_vec()),
                ("frame", b"f1".to_vec()),
                ("text", b"b\n".to_vec()),
                ("frame", b"f2".to_vec()),
                ("text", b"c\n".to_vec()),
            ]
        );
    }

    #[test]
    fn level_ordering_and_parse() {
        assert!(Level::Trace < Level::Info && Level::Info < Level::Error);
        assert_eq!(Level::parse("ERROR").unwrap(), Level::Error);
        assert_eq!(Level::parse("warning").unwrap(), Level::Warn);
        assert!(Level::parse("bogus").is_err());
    }
}
