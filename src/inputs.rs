//! Tool input types. Capture tools share a renamed field set (`stop`,
//! `timeout_s`, `idle_ms`, `grep`, `context`); device tools are unchanged.

use rmcp::schemars;
use serde::Deserialize;

// --- defaults ---

pub fn default_baud() -> u32 {
    460800
}
pub fn default_monitor_baud() -> u32 {
    115200
}
pub fn default_timeout_secs() -> f64 {
    5.0
}
pub fn default_idle_ms() -> u64 {
    4000
}
pub fn default_true() -> bool {
    true
}
pub fn default_max_bytes() -> usize {
    65536
}
pub fn default_repeat() -> usize {
    1
}

// --- device tools ---

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListPortsInput {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChipInfoInput {
    /// Serial port path (e.g., "/dev/ttyUSB0" or "/dev/ttyACM0")
    pub port: String,
    /// Baud rate for communication (default: 460800)
    #[serde(default = "default_baud")]
    pub baud: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FlashInput {
    /// Serial port path
    pub port: String,
    /// Path to the ELF or binary file to flash
    pub file_path: String,
    /// Baud rate for flashing (default: 460800)
    #[serde(default = "default_baud")]
    pub baud: u32,
    /// Flash address for raw binary files (hex or decimal). If omitted, the file
    /// is treated as an ELF and processed through the IDF bootloader format.
    pub flash_address: Option<u32>,
    /// Path to a custom partition table CSV or binary file
    pub partition_table: Option<String>,
    /// Path to a custom bootloader binary file
    pub bootloader: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EraseFlashInput {
    /// Serial port path
    pub port: String,
    /// Baud rate (default: 460800)
    #[serde(default = "default_baud")]
    pub baud: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EraseRegionInput {
    /// Serial port path
    pub port: String,
    /// Start address to erase (must be 4096-byte aligned)
    pub address: u32,
    /// Number of bytes to erase (must be 4096-byte aligned)
    pub size: u32,
    /// Baud rate (default: 460800)
    #[serde(default = "default_baud")]
    pub baud: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadFlashInput {
    /// Serial port path
    pub port: String,
    /// Start address to read from
    pub address: u32,
    /// Number of bytes to read
    pub size: u32,
    /// Path to save the output file
    pub output_path: String,
    /// Baud rate (default: 460800)
    #[serde(default = "default_baud")]
    pub baud: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ResetDeviceInput {
    /// Serial port path
    pub port: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChecksumMd5Input {
    /// Serial port path
    pub port: String,
    /// Start address of the flash region
    pub address: u32,
    /// Size of the flash region in bytes
    pub size: u32,
    /// Baud rate (default: 460800)
    #[serde(default = "default_baud")]
    pub baud: u32,
}

// --- capture tools ---

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MonitorInput {
    /// Serial port path
    pub port: String,
    /// Baud rate for serial monitoring (default: 115200, the ESP-IDF default)
    #[serde(default = "default_monitor_baud")]
    pub baud: u32,
    /// Maximum time to monitor in seconds (default: 5)
    #[serde(default = "default_timeout_secs")]
    pub timeout_s: f64,
    /// Stop capturing when this (unanchored) regex matches a rendered line. It is
    /// a substring regex, not anchored — `RESULT` matches mid-line. Plain text is
    /// a valid pattern. Alternation works: `RESULT (PASS|FAIL)`, `panic|abort`.
    pub stop: Option<String>,
    /// Stop after no new data is received for this many milliseconds (default:
    /// 4000). Raise it (6000-10000) for programs that think for a while between
    /// prints; lower it (1500-2000) for firmware that boots and prints immediately.
    #[serde(default = "default_idle_ms")]
    pub idle_ms: u64,
    /// Drop the ROM baud-mismatch garbage and ESP-IDF bootloader log lines that
    /// precede the application output (default: true). Set false to get raw bytes.
    #[serde(default = "default_true")]
    pub strip_boot_noise: bool,
    /// Strip ANSI escape / color sequences from the output (default: true). These
    /// render as color in a terminal but are pure noise tokens for an LLM.
    #[serde(default = "default_true")]
    pub strip_ansi: bool,
    /// When `stop` matches, return only the matched line plus this many lines
    /// before it (and nothing after). Omit to keep all output up to the match.
    pub context: Option<usize>,
    /// Keep only lines matching this (unanchored) regex; everything else is
    /// dropped. Applied after cleaning. Useful for full-log captures with no
    /// `stop`, e.g. `grep: "ERROR|WARN"`.
    pub grep: Option<String>,
    /// Discard any bytes buffered before monitoring starts (default: true).
    /// Prevents catching the tail of a previous run. Set false to keep buffered output.
    #[serde(default = "default_true")]
    pub flush: bool,
    /// Cap on captured bytes; stops early and marks output truncated if exceeded
    /// (default: 65536). Guards against reboot-loop floods filling the context.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FlashMonitorInput {
    /// Serial port path
    pub port: String,
    /// Path to the ELF or binary file to flash
    pub file_path: String,
    /// Baud rate for flashing (default: 460800)
    #[serde(default = "default_baud")]
    pub flash_baud: u32,
    /// Baud rate for serial monitoring after flash (default: 115200)
    #[serde(default = "default_monitor_baud")]
    pub monitor_baud: u32,
    /// Flash address for raw binary files. If omitted, ELF format is assumed.
    pub flash_address: Option<u32>,
    /// Path to a custom partition table CSV or binary file
    pub partition_table: Option<String>,
    /// Path to a custom bootloader binary file
    pub bootloader: Option<String>,
    /// Maximum time to monitor after flash in seconds (default: 5)
    #[serde(default = "default_timeout_secs")]
    pub timeout_s: f64,
    /// Stop capturing when this (unanchored) regex matches a rendered line.
    /// Substring regex with alternation: `RESULT (PASS|FAIL)`, `panic|Guru Meditation`.
    pub stop: Option<String>,
    /// Stop after no new data is received for this many milliseconds (default: 4000).
    #[serde(default = "default_idle_ms")]
    pub idle_ms: u64,
    /// Drop ROM garbage and ESP-IDF bootloader log lines before app output (default: true).
    #[serde(default = "default_true")]
    pub strip_boot_noise: bool,
    /// Strip ANSI escape / color sequences from the output (default: true).
    #[serde(default = "default_true")]
    pub strip_ansi: bool,
    /// When `stop` matches, return only the matched line plus this many lines before it.
    pub context: Option<usize>,
    /// Keep only lines matching this (unanchored) regex; applied after cleaning.
    pub grep: Option<String>,
    /// Cap on captured bytes; stops early and marks output truncated (default: 65536).
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RerunInput {
    /// Serial port path
    pub port: String,
    /// Baud rate for serial monitoring (default: 115200)
    #[serde(default = "default_monitor_baud")]
    pub baud: u32,
    /// Maximum time to monitor in seconds (default: 5)
    #[serde(default = "default_timeout_secs")]
    pub timeout_s: f64,
    /// Stop capturing when this (unanchored) regex matches a rendered line.
    /// Substring regex with alternation: `RESULT (PASS|FAIL)`, `panic|Guru Meditation`.
    pub stop: Option<String>,
    /// Stop after no new data is received for this many milliseconds (default: 4000).
    #[serde(default = "default_idle_ms")]
    pub idle_ms: u64,
    /// Drop ROM garbage and ESP-IDF bootloader log lines before app output (default: true).
    #[serde(default = "default_true")]
    pub strip_boot_noise: bool,
    /// Strip ANSI escape / color sequences from the output (default: true).
    #[serde(default = "default_true")]
    pub strip_ansi: bool,
    /// When `stop` matches, return only the matched line plus this many lines before it.
    pub context: Option<usize>,
    /// Keep only lines matching this (unanchored) regex; applied after cleaning.
    pub grep: Option<String>,
    /// Cap on captured bytes; stops early and marks output truncated (default: 65536).
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
    /// Number of reset+monitor cycles to run back-to-back (default: 1, max: 50).
    /// With repeat > 1 the result is compact: one line per run (the matched line if
    /// `stop` is set, else the last line) plus a summary counting how many runs
    /// matched. Ideal for characterizing intermittent/flaky bugs in one call.
    #[serde(default = "default_repeat")]
    pub repeat: usize,
}
