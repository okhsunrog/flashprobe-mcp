//! Rendering a [`CaptureResult`] into the displayed serial-output block, and the
//! small helpers used by the compact per-run summary.

use crate::capture::{CaptureResult, raw_text};
use crate::capture::filter::process_capture;
use regex::Regex;

/// The last non-empty (trimmed) line of `text`, or "(no output)" if there is none.
/// Used for compact per-run summaries.
pub fn last_nonempty_line(text: &str) -> String {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no output)")
        .to_string()
}

/// Char-safe truncation for compact one-line summaries.
pub fn truncate_line(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\u{2026} [+{} chars]", s.chars().count() - max)
}

/// Format a captured result into the displayed serial-output block. `header_line`
/// labels the source (e.g. `Port: /dev/ttyUSB0 @ 115200 baud` for serial, or
/// `Probe: esp32c3 via RTT` for probe-rs).
#[allow(clippy::too_many_arguments)]
pub fn render_capture(
    header_line: &str,
    result: &CaptureResult,
    strip_boot_noise_opt: bool,
    strip_ansi_opt: bool,
    stop_re: Option<&Regex>,
    context: Option<usize>,
    grep: Option<&Regex>,
) -> String {
    let raw = raw_text(result);
    let processed = process_capture(
        &raw,
        strip_boot_noise_opt,
        strip_ansi_opt,
        stop_re,
        result.matched,
        context,
        grep,
    );

    let mut header = format!(
        "{}\nStopped: {}\nCaptured {} raw bytes",
        header_line,
        result.stop_reason.as_str(),
        result.raw_bytes
    );
    if processed.len() != raw.len() {
        header.push_str(&format!(" ({} shown after cleaning)", processed.len()));
    }
    if result.truncated {
        header.push_str("\n[truncated: output cap reached, capture stopped early]");
    }

    let body = if processed.is_empty() {
        "(no application output\u{2014}only boot/ROM noise was captured)".to_string()
    } else {
        processed
    };

    format!("{header}\n\n```\n{body}\n```")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_nonempty_and_truncate() {
        assert_eq!(last_nonempty_line("a\nb\n\n  \n"), "b");
        assert_eq!(last_nonempty_line("   \n"), "(no output)");
        assert_eq!(truncate_line("short", 200), "short");
        assert_eq!(truncate_line("abcdef", 3), "abc\u{2026} [+3 chars]");
    }
}
