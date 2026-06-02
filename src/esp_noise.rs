//! ESP-ROM / ESP-IDF bootloader text scrubbing.
//!
//! This is text-mode only and ESP-specific: it strips the ROM baud-mismatch
//! garbage and the second-stage bootloader log lines that precede application
//! output on the UART. In defmt mode the framing handles boot noise for free,
//! so none of this runs.

use regex::Regex;
use std::sync::LazyLock;

/// Matches ESP ROM and second-stage (ESP-IDF) bootloader log lines. Anchored at
/// the start of an already-trimmed line. App output (esp-hal / esp-println) does
/// not use these prefixes.
static BOOT_NOISE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        ^ets\s
      | ^rst:0x
      | ^configsip:
      | ^clk_drv:
      | ^mode:[A-Za-z]
      | ^load:0x
      | ^entry\s0x
      | ^SPIWP:
      | ^ho\s\d+\stail
      | ^csum
      | ^Saved\sPC
      | ^waiting\sfor\sdownload
      | ^[IWED]\s\(\d+\)\s
        (boot|esp_image|boot_comm|cpu_start|heap_init|spi_flash|flash_parts
        |partition|partition_table|main_task|app_init|app_start|esp_psram
        |psram|octal_psram|quad_psram|system_api|register_frame|esp_core_dump
        |efuse|sleep|clk|memprot|mmu):
    ",
    )
    .unwrap()
});

/// The ROM bootlog is emitted at 74880 baud; read at 115200 it decodes to a blob
/// of U+FFFD replacement chars merged directly into the first readable boot line
/// (e.g. `..x\u{FFFD}x\u{FFFD}I (277) esp_image:`). Cut everything up to and
/// including the last replacement char so the readable tail can be classified.
pub fn strip_garbled_prefix(line: &str) -> &str {
    match line.rfind('\u{FFFD}') {
        Some(pos) => {
            let after = &line[pos + '\u{FFFD}'.len_utf8()..];
            after.trim_start_matches(|c: char| c.is_control() && c != '\t')
        }
        None => line,
    }
}

/// True if a line is an ESP ROM / bootloader log line (after any garbled prefix is
/// removed). Empty lines are NOT treated as noise: blanks are neutral so a trailing
/// blank never gets mistaken for the last line of the boot block.
pub fn is_boot_noise_line(line: &str) -> bool {
    let cleaned = strip_garbled_prefix(line).trim();
    if cleaned.is_empty() {
        return false;
    }
    BOOT_NOISE_RE.is_match(cleaned)
}

/// Largest number of consecutive non-boot lines tolerated inside the boot block.
/// A capture can start mid-line with a stray fragment (e.g. `16`, the tail of a
/// flushed `len:15916`), so a few non-recognized lines between real boot lines are
/// treated as part of the boot block. Once this many non-boot lines appear in a
/// row, application output has clearly started and the block ends.
const BOOT_BLOCK_GAP: usize = 2;

/// Drop the ROM garbage and ESP-IDF bootloader log lines, returning the output
/// from the first application line onward. The boot block is everything up to and
/// including the last bootloader line (tolerating short fragments between boot
/// lines); application output is then kept verbatim. If no boot line is found the
/// input is returned unchanged (only the leading garbled prefix is cleaned).
pub fn strip_boot_noise(raw: &str) -> String {
    let lines: Vec<&str> = raw.split('\n').collect();

    // Find the last line of the boot block.
    let mut last_boot: Option<usize> = None;
    let mut gap = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if is_boot_noise_line(line) {
            last_boot = Some(i);
            gap = 0;
        } else {
            gap += 1;
            // Only stop once we are past the boot block and into sustained output.
            if last_boot.is_some() && gap > BOOT_BLOCK_GAP {
                break;
            }
        }
    }

    let start = match last_boot {
        Some(k) => k + 1,
        None => 0,
    };
    if start >= lines.len() {
        return String::new();
    }

    // The first kept line may carry a garbled prefix merged from the ROM blob.
    let mut out = String::from(strip_garbled_prefix(lines[start]));
    for l in &lines[start + 1..] {
        out.push('\n');
        out.push_str(l);
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbled_prefix_is_cut_to_last_replacement_char() {
        let line = "x\u{FFFD}x\u{FFFD}I (277) esp_image: segment 1";
        assert_eq!(strip_garbled_prefix(line), "I (277) esp_image: segment 1");
        // a clean line is untouched
        assert_eq!(strip_garbled_prefix("INFO - hi"), "INFO - hi");
    }

    #[test]
    fn boot_noise_lines_detected() {
        assert!(is_boot_noise_line("ets Jul 29 2019 12:21:46"));
        assert!(is_boot_noise_line(
            "rst:0x1 (POWERON_RESET),boot:0x13 (SPI_FAST_FLASH_BOOT)"
        ));
        assert!(is_boot_noise_line("mode:DIO, clock div:2"));
        assert!(is_boot_noise_line("load:0x3fff0030,len:6384"));
        assert!(is_boot_noise_line("entry 0x40080644"));
        assert!(is_boot_noise_line("I (585) boot: Loaded app from partition"));
        assert!(is_boot_noise_line(
            "x\u{FFFD}x\u{FFFD}I (277) esp_image: segment 1"
        ));
        // application output is NOT boot noise
        assert!(!is_boot_noise_line("INFO - Embassy initialized!"));
        assert!(!is_boot_noise_line("RESULT PASS (100/100)"));
    }

    #[test]
    fn strip_boot_noise_handles_leading_fragment() {
        // A flushed capture can start mid-line with a stray fragment that is not a
        // recognized boot pattern; it must still be dropped along with the boot block.
        let raw = "16\n\
             load:0x40080400,len:3920\n\
             entry 0x40080644\n\
             I (27) boot: ESP-IDF\n\
             I (140) boot: Loaded app from partition at offset 0x10000\n\
             I (140) boot: Disabling RNG early entropy source...\n\
             i2s_test: RESULT FAIL (1/100 dropped)\n\
             \u{20}\u{20}transfer 99 (marker 0x2063) not received\n";
        let cleaned = strip_boot_noise(raw);
        assert_eq!(
            cleaned,
            "i2s_test: RESULT FAIL (1/100 dropped)\n  transfer 99 (marker 0x2063) not received"
        );
    }

    #[test]
    fn all_noise_yields_empty() {
        let raw = "ets Jul 29\nrst:0x1 (POWERON_RESET)\nentry 0x40080644\n";
        assert_eq!(strip_boot_noise(raw), "");
    }
}
