//! Rendering a [`CaptureResult`] into the displayed output block. Two paths:
//! `render_text` (ANSI/boot-noise stripping + match focusing, ESP serial) and
//! `render_defmt` (structured level/module filters + suppressed counts).
//! `render_block` picks one based on whether defmt stats are present.

use crate::capture::filter::process_capture;
use crate::capture::{CaptureResult, DefmtStats, Level, Line, raw_text};
use regex::Regex;
use std::collections::BTreeMap;

/// Shared render/filter options across both modes. Text-mode fields
/// (`strip_*`) and defmt-mode fields (`min_level`, `module`) are each ignored by
/// the other path.
pub struct RenderOpts<'a> {
    /// Whether this capture reset the target before reading, as `rerun` and
    /// `flash_monitor` do. Changes what an empty capture means, and so what
    /// advice is worth giving for one.
    pub captured_from_reset: bool,
    pub strip_boot_noise: bool,
    pub strip_ansi: bool,
    pub stop_re: Option<&'a Regex>,
    pub context: Option<usize>,
    pub grep: Option<&'a Regex>,
    pub min_level: Option<Level>,
    pub module: Option<&'a Regex>,
}

/// The last non-empty (trimmed) line of `text`, or "(no output)" if there is none.
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

/// Render a capture, choosing the text or defmt path. `defmt` is `Some` (with
/// decode stats) when the capture was decoded as defmt.
/// What to show when a capture produced no lines.
///
/// "Nothing arrived" and "something arrived but was filtered out" look the same
/// in the output and have completely different causes, so they are worded
/// differently. The silent case is the one that misleads: a target that prints
/// at boot and then idles produces nothing at all for a capture that did not
/// reset it, which reads as a broken tool rather than a quiet target. A capture
/// that did reset the target needs different advice, so the two are separated.
fn empty_body(raw_bytes: usize, filtered: bool, captured_from_reset: bool) -> String {
    if raw_bytes == 0 {
        let hint = if captured_from_reset {
            // The target was reset and still said nothing, so pointing at the
            // reset-based tools would be circular. What is left is a target that
            // does not run, or does not log where this backend is listening.
            concat!(
                "The target was reset and still sent nothing. Either it is not running (a ",
                "reset that leaves an ESP in download mode does this), or it logs somewhere ",
                "this backend is not listening \u{2014} firmware using RTT produces nothing on ",
                "the serial backend, and vice versa."
            )
        } else {
            concat!(
                "The target sent no bytes while this capture was open. If it prints at boot ",
                "and then goes quiet, use `rerun` or `flash_monitor`, which reset it and ",
                "capture from the start; a plain `monitor` only sees what is sent after it ",
                "attaches."
            )
        };
        format!("(nothing was received)\n\n{hint}")
    } else if filtered {
        "(bytes arrived, but every line was removed by the filters)".to_string()
    } else {
        "(no application output\u{2014}only boot/ROM noise was captured)".to_string()
    }
}

pub fn render_block(
    header_line: &str,
    result: &CaptureResult,
    defmt: Option<DefmtStats>,
    opts: &RenderOpts,
) -> String {
    match defmt {
        Some(stats) => render_defmt(header_line, result, stats, opts),
        None => render_text(header_line, result, opts),
    }
}

/// Text-mode rendering: reconstruct the raw stream, then strip ANSI / boot noise,
/// focus on the match, and apply `grep` (see [`process_capture`]).
fn render_text(header_line: &str, result: &CaptureResult, opts: &RenderOpts) -> String {
    let raw = raw_text(result);
    let processed = process_capture(
        &raw,
        opts.strip_boot_noise,
        opts.strip_ansi,
        opts.stop_re,
        result.matched,
        opts.context,
        opts.grep,
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
        header.push_str(
            "\n[truncated: output cap reached, capture stopped early. Reads arrive in \
             chunks, so the captured total overshoots the cap rather than landing on it]",
        );
    }

    let body = if processed.is_empty() {
        empty_body(result.raw_bytes, false, opts.captured_from_reset)
    } else {
        processed
    };
    format!("{header}\n\n```\n{body}\n```")
}

/// defmt-mode rendering: focus on the stop match (± context), then apply the
/// structured `min_level` / `module` filters and `grep`, reporting how many
/// frames each level hid so the agent can loosen `level` if it wants more.
fn render_defmt(
    header_line: &str,
    result: &CaptureResult,
    stats: DefmtStats,
    opts: &RenderOpts,
) -> String {
    // 1. Focus to the last line matching `stop`, with `context` lines before it.
    let focused: Vec<&Line> = match (result.matched, opts.stop_re) {
        (true, Some(re)) => match result.lines.iter().rposition(|l| re.is_match(&l.text)) {
            Some(mi) => {
                let start = opts.context.map_or(0, |c| mi.saturating_sub(c));
                result.lines[start..=mi].iter().collect()
            }
            None => result.lines.iter().collect(),
        },
        _ => result.lines.iter().collect(),
    };

    // 2. Apply structured + grep filters, counting frames hidden purely by level.
    let mut hidden_by_level: BTreeMap<Level, usize> = BTreeMap::new();
    let mut shown: Vec<&str> = Vec::new();
    for l in focused {
        if let (Some(min), Some(lv)) = (opts.min_level, l.level)
            && lv < min
        {
            *hidden_by_level.entry(lv).or_default() += 1;
            continue;
        }
        if let Some(mre) = opts.module {
            // A module filter only keeps frames whose module is known and matches.
            if !l.module.as_deref().is_some_and(|m| mre.is_match(m)) {
                continue;
            }
        }
        if let Some(g) = opts.grep
            && !g.is_match(&l.text)
        {
            continue;
        }
        shown.push(&l.text);
    }

    let mut header = format!(
        "{}\nStopped: {}\nmode: defmt\nCaptured {} raw bytes, {} frames decoded",
        header_line,
        result.stop_reason.as_str(),
        result.raw_bytes,
        stats.decoded
    );
    if stats.malformed > 0 {
        header.push_str(&format!(", {} malformed", stats.malformed));
    }
    if result.truncated {
        header.push_str(
            "\n[truncated: output cap reached, capture stopped early. Reads arrive in \
             chunks, so the captured total overshoots the cap rather than landing on it]",
        );
    }
    if !hidden_by_level.is_empty() {
        // Highest level first: "hidden by level: 412 debug, 30 trace".
        let parts: Vec<String> = hidden_by_level
            .iter()
            .rev()
            .map(|(lv, n)| format!("{n} {}", lv.as_str().to_lowercase()))
            .collect();
        header.push_str(&format!("\nhidden by level: {}", parts.join(", ")));
    }
    if stats.decoded == 0 && result.raw_bytes > 0 {
        header.push_str(
            "\n[warning: 0 defmt frames decoded from a non-empty stream \u{2014} the ELF likely \
             does not match the running firmware]",
        );
    }

    let body = if shown.is_empty() {
        // Anything decoded or hidden means bytes did arrive and the filters are
        // what emptied the output.
        let filtered = stats.decoded > 0 || !hidden_by_level.is_empty();
        empty_body(result.raw_bytes, filtered, opts.captured_from_reset)
    } else {
        shown.join("\n")
    };
    format!("{header}\n\n```\n{body}\n```")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::StopReason;

    #[test]
    fn empty_body_tells_silence_apart_from_filtering() {
        // Nothing arrived: the filters are not the reason, and the hint points
        // at the case that actually causes this.
        // Nothing arrived and nothing reset the target: suggest the tools that do.
        let silent = empty_body(0, false, false);
        assert!(silent.contains("nothing was received"), "{silent}");
        assert!(silent.contains("rerun"), "{silent}");
        assert!(!silent.contains("filters"), "{silent}");

        // Nothing arrived even though the target was reset. Suggesting `rerun`
        // here would be circular, since `rerun` is what produced this.
        let after_reset = empty_body(0, false, true);
        assert!(after_reset.contains("nothing was received"), "{after_reset}");
        assert!(after_reset.contains("reset and still sent nothing"), "{after_reset}");
        assert!(!after_reset.contains("use `rerun`"), "{after_reset}");

        // Bytes arrived and the filters emptied the output.
        let filtered = empty_body(500, true, false);
        assert!(filtered.contains("filters"), "{filtered}");
        assert!(!filtered.contains("nothing was received"), "{filtered}");

        // Bytes arrived, nothing was filtered, but cleaning left nothing.
        let noise = empty_body(500, false, false);
        assert!(noise.contains("boot/ROM noise"), "{noise}");
    }

    #[test]
    fn last_nonempty_and_truncate() {
        assert_eq!(last_nonempty_line("a\nb\n\n  \n"), "b");
        assert_eq!(last_nonempty_line("   \n"), "(no output)");
        assert_eq!(truncate_line("short", 200), "short");
        assert_eq!(truncate_line("abcdef", 3), "abc\u{2026} [+3 chars]");
    }

    fn line(text: &str, level: Option<Level>, module: Option<&str>) -> Line {
        Line {
            text: text.to_string(),
            level,
            module: module.map(String::from),
        }
    }

    fn result(lines: Vec<Line>, matched: bool, reason: StopReason) -> CaptureResult {
        CaptureResult {
            lines,
            pending: String::new(),
            raw_bytes: 100,
            stop_reason: reason,
            matched,
            truncated: false,
        }
    }

    fn defmt_opts<'a>(
        stop_re: Option<&'a Regex>,
        context: Option<usize>,
        min_level: Option<Level>,
        module: Option<&'a Regex>,
    ) -> RenderOpts<'a> {
        RenderOpts {
            captured_from_reset: false,
            strip_boot_noise: true,
            strip_ansi: true,
            stop_re,
            context,
            grep: None,
            min_level,
            module,
        }
    }

    #[test]
    fn defmt_level_filter_reports_suppressed_count() {
        let r = result(
            vec![
                line("INFO a", Some(Level::Info), Some("app::foo")),
                line("DEBUG b", Some(Level::Debug), Some("app::bar")),
                line("ERROR c", Some(Level::Error), Some("app::foo")),
            ],
            false,
            StopReason::Idle,
        );
        let opts = defmt_opts(None, None, Some(Level::Info), None);
        let out = render_block(
            "Probe: x",
            &r,
            Some(DefmtStats {
                decoded: 3,
                malformed: 0,
            }),
            &opts,
        );
        assert!(out.contains("mode: defmt"));
        assert!(out.contains("INFO a") && out.contains("ERROR c"));
        assert!(!out.contains("DEBUG b"));
        assert!(out.contains("hidden by level: 1 debug"));
    }

    #[test]
    fn defmt_module_filter_keeps_only_matching() {
        let re = Regex::new("app::foo").unwrap();
        let r = result(
            vec![
                line("INFO a", Some(Level::Info), Some("app::foo")),
                line("INFO b", Some(Level::Info), Some("app::bar")),
                line("INFO c", Some(Level::Info), None),
            ],
            false,
            StopReason::Idle,
        );
        let opts = defmt_opts(None, None, None, Some(&re));
        let out = render_block(
            "Probe: x",
            &r,
            Some(DefmtStats {
                decoded: 3,
                malformed: 0,
            }),
            &opts,
        );
        assert!(out.contains("INFO a"));
        assert!(!out.contains("INFO b") && !out.contains("INFO c"));
    }

    #[test]
    fn defmt_stop_focus_with_context() {
        let re = Regex::new("boom").unwrap();
        let r = result(
            vec![
                line("INFO 1", Some(Level::Info), None),
                line("INFO 2", Some(Level::Info), None),
                line("ERROR boom", Some(Level::Error), None),
                line("INFO after", Some(Level::Info), None),
            ],
            true,
            StopReason::Matched,
        );
        let opts = defmt_opts(Some(&re), Some(1), None, None);
        let out = render_block(
            "Probe: x",
            &r,
            Some(DefmtStats {
                decoded: 4,
                malformed: 0,
            }),
            &opts,
        );
        assert!(out.contains("INFO 2") && out.contains("ERROR boom"));
        assert!(!out.contains("INFO 1") && !out.contains("after"));
    }

    #[test]
    fn defmt_warns_on_zero_frames() {
        let r = result(vec![], false, StopReason::Timeout);
        let opts = defmt_opts(None, None, None, None);
        let out = render_block(
            "Probe: x",
            &r,
            Some(DefmtStats {
                decoded: 0,
                malformed: 0,
            }),
            &opts,
        );
        assert!(out.contains("0 defmt frames decoded"));
        assert!(out.contains("does not match the running firmware"));
    }
}
