//! The shared, transport-agnostic capture pipeline.
//!
//! A [`ByteSource`] feeds bytes, a [`Decode`]r turns them into [`Line`]s, and
//! [`run_capture`] applies the early-exiting stop/bound logic — the programmatic
//! equivalent of a human watching the log and pressing Ctrl-C the instant the
//! expected line appears. This loop is the asset preserved from the original
//! serial-only `read_serial_output`; only the byte source differs per backend.

pub mod decode;
pub mod filter;
pub mod render;
pub mod source;

pub use decode::{Decode, DefmtDecoder, DefmtFraming, DefmtStats, Level, Line, TextDecoder};
pub use source::ByteSource;

use defmt_decoder::Table;
use regex::Regex;
use std::time::{Duration, Instant};

/// How to decode a capture: plain text, or defmt against a parsed ELF table.
/// `framing` selects the wire format (esp-println marker vs raw RTT rzCOBS).
pub enum DecodeMode<'a> {
    Text,
    Defmt {
        table: &'a Table,
        elf: &'a [u8],
        framing: DefmtFraming,
    },
}

/// Run a capture with the decoder selected by `mode`, returning the result and
/// (in defmt mode) decode stats. This centralizes decoder construction so the
/// tool methods don't juggle the `Table`/`StreamDecoder` lifetimes.
pub fn capture(
    source: &mut dyn ByteSource,
    mode: &DecodeMode,
    opts: &CaptureOpts,
) -> Result<(CaptureResult, Option<DefmtStats>), String> {
    let text_mode = DecodeMode::Text;
    let mode = if source.text_only() { &text_mode } else { mode };
    match mode {
        DecodeMode::Text => {
            let mut decoder = TextDecoder::new();
            let result = run_capture(source, &mut decoder, opts)?;
            Ok((result, None))
        }
        DecodeMode::Defmt {
            table,
            elf,
            framing,
        } => {
            let locations = table.get_locations(elf).ok();
            let mut decoder = DefmtDecoder::new(table, locations, table.has_timestamp(), *framing);
            let result = run_capture(source, &mut decoder, opts)?;
            let stats = decoder.stats();
            Ok((result, Some(stats)))
        }
    }
}

/// Why a capture stopped. Display strings are preserved verbatim from the
/// original implementation so rendered output is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Matched,
    Timeout,
    Idle,
    Cap,
    ReadError,
    Finished,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::Matched => "stop pattern matched",
            StopReason::Timeout => "timeout reached",
            StopReason::Idle => "idle timeout (no new data)",
            StopReason::Cap => "output cap reached",
            StopReason::ReadError => "source read error",
            StopReason::Finished => "source completed",
        }
    }
}

/// Bounds and stop conditions for a capture. Any one stop condition ending the
/// capture is the Ctrl-C replacement.
pub struct CaptureOpts {
    pub timeout: Duration,
    pub idle: Duration,
    /// Stop when this regex matches a rendered line (both modes).
    pub stop: Option<Regex>,
    /// Stop on the first frame at or above this level (defmt mode only; text
    /// lines carry no level, so this never fires on them).
    pub stop_on_level: Option<Level>,
    pub flush: bool,
    pub max_bytes: usize,
    /// Bytes to send to the target once capture starts (the `send` option).
    ///
    /// This lives here, rather than at the call site, purely for ordering: the
    /// send has to happen *after* [`CaptureOpts::flush`] discards stale input
    /// but *before* the read loop, or the flush would swallow the very reply we
    /// are waiting for.
    pub send: Option<Vec<u8>>,
}

pub struct CaptureResult {
    pub lines: Vec<Line>,
    /// The un-terminated tail present when capture stopped.
    pub pending: String,
    /// Total bytes read from the source.
    pub raw_bytes: usize,
    pub stop_reason: StopReason,
    pub matched: bool,
    pub truncated: bool,
}

/// Reconstruct the raw captured text from a result (lines rejoined with `\n`,
/// plus any pending tail). This is the text the rendering/filtering stage cleans.
pub fn raw_text(result: &CaptureResult) -> String {
    let mut s = result
        .lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !result.pending.is_empty() {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(&result.pending);
    }
    s
}

/// Run the bounded, early-exiting capture loop. Stops on the first of: a stop
/// match (then lingers briefly for the matched line to complete), the wall-clock
/// timeout, the idle timeout, or the byte cap.
pub fn run_capture(
    source: &mut dyn ByteSource,
    decoder: &mut dyn Decode,
    opts: &CaptureOpts,
) -> Result<CaptureResult, String> {
    if opts.flush {
        let _ = source.flush_input();
    }
    // Strictly after the flush (see `CaptureOpts::send`) and before the first
    // read, so the reply lands inside the capture window.
    if let Some(data) = &opts.send {
        source::send_all(source, data)?;
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut raw_bytes = 0usize;
    let mut buf = [0u8; 4096];
    let start = Instant::now();
    let mut last_data = Instant::now();

    // After a stop match, linger briefly so the rest of the matched line arrives
    // (it is often not newline-terminated at the instant of match).
    let grace = Duration::from_millis(250);
    let mut matched = false;
    let mut match_grace_until: Option<Instant> = None;
    // Whether the matched line is newline-terminated (so we can stop at once) or
    // is still a partial line we should let finish within the grace window.
    let mut match_line_complete = false;

    let stop = opts.stop.as_ref();

    let outcome: (StopReason, bool, bool) = loop {
        if matched {
            if match_line_complete {
                break (StopReason::Matched, true, false);
            }
            if match_grace_until.is_some_and(|t| Instant::now() >= t) {
                break (StopReason::Matched, true, false);
            }
        }

        if start.elapsed() >= opts.timeout {
            break (StopReason::Timeout, matched, false);
        }

        let has_content = !lines.is_empty() || decoder.pending().is_some_and(|p| !p.is_empty());
        if !matched && has_content && last_data.elapsed() >= opts.idle {
            break (StopReason::Idle, false, false);
        }

        match source.read(&mut buf) {
            Ok(0) => {
                if source.finished() {
                    break (StopReason::Finished, matched, false);
                }
                let nap = source.idle_nap();
                if !nap.is_zero() {
                    std::thread::sleep(nap);
                }
            }
            Ok(n) => {
                last_data = Instant::now();
                raw_bytes += n;

                let new_lines = decoder.push(&buf[..n]);
                let had_new_lines = !new_lines.is_empty();
                for line in new_lines {
                    let is_match = !matched
                        && (stop.is_some_and(|re| re.is_match(&line.text))
                            || matches!((opts.stop_on_level, line.level),
                                (Some(threshold), Some(level)) if level >= threshold));
                    lines.push(line);
                    if is_match {
                        matched = true;
                        match_line_complete = true; // an emitted line is newline-terminated
                        match_grace_until = Some(Instant::now() + grace);
                    }
                }

                if !matched {
                    // The match may land on the not-yet-terminated tail.
                    if let Some(p) = decoder.pending()
                        && stop.is_some_and(|re| re.is_match(p))
                    {
                        matched = true;
                        match_line_complete = false;
                        match_grace_until = Some(Instant::now() + grace);
                    }
                } else if !match_line_complete && had_new_lines {
                    // A previously-matched partial line has since been completed.
                    match_line_complete = true;
                }

                if raw_bytes >= opts.max_bytes {
                    break (StopReason::Cap, matched, true);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                let empty = lines.is_empty() && decoder.pending().is_none_or(|p| p.is_empty());
                if empty {
                    return Err(format!("Source read error: {e}"));
                }
                if let Some(tail) = decoder.pending().filter(|p| !p.is_empty()) {
                    lines.push(Line::text(tail));
                }
                lines.push(Line::text(format!("Source read error: {e}")));
                break (StopReason::ReadError, matched, false);
            }
        }
    };

    let pending = if outcome.0 == StopReason::ReadError {
        String::new() // The unterminated tail was emitted before the diagnostic.
    } else {
        decoder.pending().unwrap_or("").to_string()
    };
    Ok(CaptureResult {
        lines,
        pending,
        raw_bytes,
        stop_reason: outcome.0,
        matched: outcome.1,
        truncated: outcome.2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::source::ByteSource;

    /// Records the order in which the loop touches the source, and replies with
    /// canned bytes once something has been sent.
    #[derive(Default)]
    struct Recorder {
        ops: Vec<&'static str>,
        reply: Vec<u8>,
        replied: bool,
    }

    impl ByteSource for Recorder {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.replied || self.reply.is_empty() {
                return Ok(0);
            }
            self.replied = true;
            buf[..self.reply.len()].copy_from_slice(&self.reply);
            Ok(self.reply.len())
        }
        fn flush_input(&mut self) -> std::io::Result<()> {
            self.ops.push("flush");
            Ok(())
        }
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.ops.push("send");
            Ok(buf.len())
        }
    }

    fn opts(flush: bool, send: Option<&[u8]>) -> CaptureOpts {
        CaptureOpts {
            timeout: Duration::from_millis(150),
            idle: Duration::from_millis(50),
            stop: None,
            stop_on_level: None,
            flush,
            max_bytes: 4096,
            send: send.map(<[u8]>::to_vec),
        }
    }

    /// The whole reason `send` lives in `CaptureOpts`: sending before the flush
    /// would let the flush discard the reply we are waiting for.
    #[test]
    fn send_happens_after_the_flush() {
        let mut src = Recorder::default();
        let mut dec = TextDecoder::new();
        run_capture(&mut src, &mut dec, &opts(true, Some(b"cmd\n"))).unwrap();
        assert_eq!(src.ops, vec!["flush", "send"]);
    }

    /// With flush disabled the send still happens, and still before reading.
    #[test]
    fn send_happens_without_a_flush() {
        let mut src = Recorder::default();
        let mut dec = TextDecoder::new();
        run_capture(&mut src, &mut dec, &opts(false, Some(b"cmd\n"))).unwrap();
        assert_eq!(src.ops, vec!["send"]);
    }

    /// No `send` must leave the write path untouched, so read-only backends keep
    /// working exactly as before.
    #[test]
    fn no_send_never_writes() {
        let mut src = Recorder::default();
        let mut dec = TextDecoder::new();
        run_capture(&mut src, &mut dec, &opts(true, None)).unwrap();
        assert_eq!(src.ops, vec!["flush"]);
    }

    /// The reply to a sent command must land inside the capture window.
    #[test]
    fn reply_to_the_sent_command_is_captured() {
        let mut src = Recorder {
            reply: b"pong\n".to_vec(),
            ..Default::default()
        };
        let mut dec = TextDecoder::new();
        let result = run_capture(&mut src, &mut dec, &opts(true, Some(b"ping\n"))).unwrap();
        assert_eq!(raw_text(&result).trim(), "pong");
    }

    /// A failed send aborts the capture rather than silently reading nothing.
    #[test]
    fn failed_send_aborts_the_capture() {
        struct NoWrite;
        impl ByteSource for NoWrite {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Ok(0)
            }
        }
        let mut dec = TextDecoder::new();
        let Err(err) = run_capture(&mut NoWrite, &mut dec, &opts(false, Some(b"x"))) else {
            panic!("capture should fail when the payload cannot be delivered");
        };
        assert!(err.contains("does not support sending"), "got: {err}");
    }
}

#[cfg(test)]
mod finite_source_tests {
    use super::*;
    struct Finite {
        chunks: std::collections::VecDeque<Vec<u8>>,
    }
    impl ByteSource for Finite {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let Some(bytes) = self.chunks.pop_front() else {
                return Ok(0);
            };
            buf[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
        fn finished(&self) -> bool {
            self.chunks.is_empty()
        }
    }
    fn run(chunks: &[&[u8]], stop: Option<&str>, cap: usize) -> CaptureResult {
        let mut source = Finite {
            chunks: chunks.iter().map(|s| s.to_vec()).collect(),
        };
        let opts = CaptureOpts {
            timeout: Duration::from_secs(1),
            idle: Duration::from_secs(1),
            stop: stop.map(|s| Regex::new(s).unwrap()),
            stop_on_level: None,
            flush: false,
            max_bytes: cap,
            send: None,
        };
        run_capture(&mut source, &mut TextDecoder::new(), &opts).unwrap()
    }
    #[test]
    fn completion_preserves_unterminated_output() {
        let r = run(&[b"first\n", b"last"], None, 4096);
        assert_eq!(r.stop_reason, StopReason::Finished);
        assert_eq!(raw_text(&r), "first\nlast");
        assert_eq!(r.raw_bytes, 10);
    }
    #[test]
    fn split_summary_matches_before_completion() {
        let r = run(
            &[b"test a ... ok\ntest res", b"ult: ok. 1 passed; 0 failed\n"],
            Some("test result:"),
            4096,
        );
        assert_eq!(r.stop_reason, StopReason::Matched);
        assert!(r.matched);
        assert!(raw_text(&r).contains("1 passed; 0 failed"));
    }
    #[test]
    fn finite_source_still_obeys_cap() {
        let r = run(&[b"1234", b"5678"], None, 4);
        assert_eq!(r.stop_reason, StopReason::Cap);
        assert!(r.truncated);
        assert_eq!(r.raw_bytes, 4);
    }
}
