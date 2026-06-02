//! Stop/show filtering applied to a capture: ANSI stripping, ESP boot-noise
//! stripping (text mode), match focusing, and line filtering. These all operate
//! on the rendered text of a capture, in the order `process_capture` applies them.

use crate::esp_noise::strip_boot_noise;
use regex::Regex;
use std::sync::LazyLock;

/// Matches a single ANSI escape sequence (CSI sequences such as color codes).
static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?=]*[ -/]*[@-~]").unwrap());

/// Remove ANSI escape sequences from `text`.
pub fn strip_ansi(text: &str) -> String {
    ANSI_RE.replace_all(text, "").into_owned()
}

/// Compile an optional user-supplied regex, mapping errors to a readable message.
pub fn compile_opt_regex(pattern: Option<&str>) -> Result<Option<Regex>, String> {
    pattern
        .map(|p| Regex::new(p).map_err(|e| format!("Invalid regex pattern: {e}")))
        .transpose()
}

/// Keep only the lines of `text` that match `filter`.
pub fn filter_lines(text: &str, filter: &Regex) -> String {
    text.lines()
        .filter(|l| filter.is_match(l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// When a stop pattern matched, focus the output on the match: keep up to and
/// including the line containing the last match (dropping any post-match reboot
/// junk), and at most `context` lines before it. Returns None if the (cleaned)
/// text no longer contains the pattern.
pub fn trim_to_match(text: &str, re: &Regex, context: Option<usize>) -> Option<String> {
    let m = re.find_iter(text).last()?;
    let match_start = m.start();

    let lines: Vec<&str> = text.split('\n').collect();
    let mut offset = 0usize;
    let mut match_line = 0usize;
    for (i, l) in lines.iter().enumerate() {
        let line_end = offset + l.len();
        if match_start <= line_end {
            match_line = i;
            break;
        }
        offset = line_end + 1; // account for the '\n'
    }

    let start_line = match context {
        Some(n) => match_line.saturating_sub(n),
        None => 0,
    };
    Some(lines[start_line..=match_line].join("\n"))
}

/// Apply ANSI stripping, ESP boot-noise stripping, match focusing, and line
/// filtering to a rendered capture, in that order.
#[allow(clippy::too_many_arguments)]
pub fn process_capture(
    raw: &str,
    strip_boot_noise_opt: bool,
    strip_ansi_opt: bool,
    stop_re: Option<&Regex>,
    matched: bool,
    context: Option<usize>,
    grep: Option<&Regex>,
) -> String {
    let mut text = if strip_ansi_opt {
        strip_ansi(raw)
    } else {
        raw.to_string()
    };
    if strip_boot_noise_opt {
        text = strip_boot_noise(&text);
    } else {
        text = text.trim_end().to_string();
    }
    if matched
        && let Some(re) = stop_re
        && let Some(focused) = trim_to_match(&text, re, context)
    {
        text = focused;
    }
    if let Some(f) = grep {
        text = filter_lines(&text, f);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    // A realistic capture: ROM baud-mismatch garbage merged into the first boot
    // line, ESP-IDF bootloader logs, then ANSI-colored application output.
    pub(crate) fn sample_capture() -> String {
        let mut s = String::new();
        s.push_str("\u{FFFD}\u{FFFD}xx \u{FFFD}x\u{FFFD}x xx\u{FFFD} \u{FFFD}x\u{FFFD} ");
        s.push_str("\u{FFFD}\u{FFFD}\u{FFFD}x\u{FFFD}\u{FFFD}x\u{FFFD}x\u{FFFD}");
        // garbage runs straight into the first readable boot line, no newline
        s.push_str("I (277) esp_image: segment 1: paddr=00097a60 size=04384h load\n");
        s.push_str("I (284) esp_image: segment 2: paddr=0009bdec size=0422ch load\n");
        s.push_str("I (585) boot: Loaded app from partition at offset 0x10000\n");
        s.push_str("I (585) boot: Disabling RNG early entropy source...\n");
        s.push_str("\x1b[32mINFO - Embassy initialized!\x1b[0m\n");
        s.push_str("\x1b[32mINFO - BLE initialized!\x1b[0m\n");
        s.push_str("\x1b[32mINFO - RESULT PASS (100/100)\x1b[0m\n");
        s
    }

    #[test]
    fn strips_ansi() {
        let s = "\x1b[32mhello\x1b[0m world\x1b[1;31m!\x1b[0m";
        assert_eq!(strip_ansi(s), "hello world!");
    }

    #[test]
    fn strip_boot_noise_keeps_only_app_output() {
        let raw = strip_ansi(&sample_capture());
        let cleaned = strip_boot_noise(&raw);
        let expected = "INFO - Embassy initialized!\n\
             INFO - BLE initialized!\n\
             INFO - RESULT PASS (100/100)";
        assert_eq!(cleaned, expected);
    }

    #[test]
    fn process_capture_full_pipeline() {
        let raw = sample_capture();
        let out = process_capture(&raw, true, true, None, false, None, None);
        assert!(out.starts_with("INFO - Embassy initialized!"));
        assert!(!out.contains("esp_image"));
        assert!(!out.contains('\u{FFFD}'));
        assert!(!out.contains('\x1b'));
        assert!(out.ends_with("RESULT PASS (100/100)"));
    }

    #[test]
    fn trim_to_match_focuses_on_match_line() {
        let text = "line a\nline b\nRESULT PASS\ntrailing reboot junk\nmore junk";
        let re = Regex::new("RESULT (PASS|FAIL)").unwrap();
        // no context: everything up to and including the match line
        let out = trim_to_match(text, &re, None).unwrap();
        assert_eq!(out, "line a\nline b\nRESULT PASS");
        // with 1 context line: just the prior line + match line
        let out = trim_to_match(text, &re, Some(1)).unwrap();
        assert_eq!(out, "line b\nRESULT PASS");
        // context larger than available is clamped
        let out = trim_to_match(text, &re, Some(10)).unwrap();
        assert_eq!(out, "line a\nline b\nRESULT PASS");
    }

    #[test]
    fn process_capture_with_match_drops_trailing_junk() {
        let mut raw = sample_capture();
        raw.push_str("rst:0x1 (POWERON_RESET)\n\u{FFFD}\u{FFFD}garbage reboot\n");
        let re = Regex::new("RESULT (PASS|FAIL)").unwrap();
        let out = process_capture(&raw, true, true, Some(&re), true, Some(0), None);
        assert_eq!(out, "INFO - RESULT PASS (100/100)");
    }

    #[test]
    fn filter_keeps_only_matching_lines() {
        let text = "INFO - a\nERROR - boom\nINFO - b\nWARN - hmm";
        let re = Regex::new("ERROR|WARN").unwrap();
        assert_eq!(filter_lines(text, &re), "ERROR - boom\nWARN - hmm");
    }

    #[test]
    fn process_capture_applies_filter() {
        let raw = sample_capture();
        let re = Regex::new("RESULT").unwrap();
        let out = process_capture(&raw, true, true, None, false, None, Some(&re));
        assert_eq!(out, "INFO - RESULT PASS (100/100)");
    }
}
