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
        header.push_str("\n[truncated: output cap reached, capture stopped early]");
    }

    let body = if processed.is_empty() {
        "(no application output\u{2014}only boot/ROM noise was captured)".to_string()
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
        header.push_str("\n[truncated: output cap reached, capture stopped early]");
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
        "(no frames matched the filters)".to_string()
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
        let out = render_block("Probe: x", &r, Some(DefmtStats { decoded: 3, malformed: 0 }), &opts);
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
        let out = render_block("Probe: x", &r, Some(DefmtStats { decoded: 3, malformed: 0 }), &opts);
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
        let out = render_block("Probe: x", &r, Some(DefmtStats { decoded: 4, malformed: 0 }), &opts);
        assert!(out.contains("INFO 2") && out.contains("ERROR boom"));
        assert!(!out.contains("INFO 1") && !out.contains("after"));
    }

    #[test]
    fn defmt_warns_on_zero_frames() {
        let r = result(vec![], false, StopReason::Timeout);
        let opts = defmt_opts(None, None, None, None);
        let out = render_block("Probe: x", &r, Some(DefmtStats { decoded: 0, malformed: 0 }), &opts);
        assert!(out.contains("0 defmt frames decoded"));
        assert!(out.contains("does not match the running firmware"));
    }
}
