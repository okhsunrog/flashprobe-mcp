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

pub use decode::{Decode, Line};
pub use source::ByteSource;

use regex::Regex;
use std::time::{Duration, Instant};

/// Why a capture stopped. Display strings are preserved verbatim from the
/// original implementation so rendered output is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Matched,
    Timeout,
    Idle,
    Cap,
    ReadError,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::Matched => "stop pattern matched",
            StopReason::Timeout => "timeout reached",
            StopReason::Idle => "idle timeout (no new data)",
            StopReason::Cap => "output cap reached",
            StopReason::ReadError => "serial read error",
        }
    }
}

/// Bounds and stop conditions for a capture. (defmt-only conditions like
/// `stop_on_level` are added in a later milestone.)
pub struct CaptureOpts {
    pub timeout: Duration,
    pub idle: Duration,
    pub stop: Option<Regex>,
    pub flush: bool,
    pub max_bytes: usize,
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
                    let is_match = !matched && stop.is_some_and(|re| re.is_match(&line.text));
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
                    return Err(format!("Serial read error: {e}"));
                }
                break (StopReason::ReadError, matched, false);
            }
        }
    };

    let pending = decoder.pending().unwrap_or("").to_string();
    Ok(CaptureResult {
        lines,
        pending,
        raw_bytes,
        stop_reason: outcome.0,
        matched: outcome.1,
        truncated: outcome.2,
    })
}
