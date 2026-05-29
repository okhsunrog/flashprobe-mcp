//! MCP Server for espflash - Flash, erase, and manage ESP devices over serial

use anyhow::Result;
use espflash::{
    connection::{Connection, ResetAfterOperation, ResetBeforeOperation},
    flasher::{FlashData, FlashSettings, Flasher},
    image_format::{ImageFormat, idf::IdfBootloaderFormat},
    target::DefaultProgressCallback,
};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::tool::ToolRouter,
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    schemars, tool, tool_handler, tool_router,
};
use regex::Regex;
use serde::Deserialize;
use serialport::{ClearBuffer, SerialPortType};
use std::io::Read as _;
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tracing::info;
use tracing_subscriber::EnvFilter;

// --- Input types ---

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListPortsInput {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ChipInfoInput {
    /// Serial port path (e.g., "/dev/ttyUSB0" or "/dev/ttyACM0")
    port: String,
    /// Baud rate for communication (default: 460800)
    #[serde(default = "default_baud")]
    baud: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FlashInput {
    /// Serial port path
    port: String,
    /// Path to the ELF or binary file to flash
    file_path: String,
    /// Baud rate for flashing (default: 460800)
    #[serde(default = "default_baud")]
    baud: u32,
    /// Flash address for raw binary files (hex or decimal). If omitted, the file
    /// is treated as an ELF and processed through the IDF bootloader format.
    flash_address: Option<u32>,
    /// Path to a custom partition table CSV or binary file
    partition_table: Option<String>,
    /// Path to a custom bootloader binary file
    bootloader: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EraseFlashInput {
    /// Serial port path
    port: String,
    /// Baud rate (default: 460800)
    #[serde(default = "default_baud")]
    baud: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EraseRegionInput {
    /// Serial port path
    port: String,
    /// Start address to erase (must be 4096-byte aligned)
    address: u32,
    /// Number of bytes to erase (must be 4096-byte aligned)
    size: u32,
    /// Baud rate (default: 460800)
    #[serde(default = "default_baud")]
    baud: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReadFlashInput {
    /// Serial port path
    port: String,
    /// Start address to read from
    address: u32,
    /// Number of bytes to read
    size: u32,
    /// Path to save the output file
    output_path: String,
    /// Baud rate (default: 460800)
    #[serde(default = "default_baud")]
    baud: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ResetDeviceInput {
    /// Serial port path
    port: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ChecksumMd5Input {
    /// Serial port path
    port: String,
    /// Start address of the flash region
    address: u32,
    /// Size of the flash region in bytes
    size: u32,
    /// Baud rate (default: 460800)
    #[serde(default = "default_baud")]
    baud: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MonitorInput {
    /// Serial port path
    port: String,
    /// Baud rate for serial monitoring (default: 115200, which is the ESP-IDF default)
    #[serde(default = "default_monitor_baud")]
    baud: u32,
    /// Maximum time to monitor in seconds (default: 5)
    #[serde(default = "default_timeout_secs")]
    timeout_secs: f64,
    /// Stop monitoring when this (unanchored) regex matches anywhere in the output.
    /// It is a substring regex, not anchored — `RESULT` matches mid-line. Alternation
    /// works: `RESULT (PASS|FAIL)`, `panic|abort|Guru Meditation`, `Ready|app_main`.
    stop_pattern: Option<String>,
    /// Stop after no new data is received for this many milliseconds (default: 4000).
    /// Raise it (6000-10000) for programs that think for a while between prints;
    /// lower it (1500-2000) for firmware that boots and prints immediately.
    #[serde(default = "default_idle_timeout_ms")]
    idle_timeout_ms: u64,
    /// Drop the ROM baud-mismatch garbage and ESP-IDF bootloader log lines that
    /// precede the application output (default: true). Set false to get raw bytes.
    #[serde(default = "default_true")]
    strip_boot_noise: bool,
    /// Strip ANSI escape / color sequences from the output (default: true). These
    /// render as color in a terminal but are pure noise tokens for an LLM.
    #[serde(default = "default_true")]
    strip_ansi: bool,
    /// When stop_pattern matches, return only the matched line plus this many lines
    /// before it (and nothing after). Omit to keep all output up to the matched line.
    context_lines: Option<usize>,
    /// Discard any bytes buffered before monitoring starts (default: true). Prevents
    /// catching the tail of a previous run. Set false to keep already-buffered output.
    #[serde(default = "default_true")]
    flush: bool,
    /// Cap on captured bytes; stops early and marks output truncated if exceeded
    /// (default: 65536). Guards against reboot-loop floods filling the context.
    #[serde(default = "default_max_bytes")]
    max_bytes: usize,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FlashMonitorInput {
    /// Serial port path
    port: String,
    /// Path to the ELF or binary file to flash
    file_path: String,
    /// Baud rate for flashing (default: 460800)
    #[serde(default = "default_baud")]
    flash_baud: u32,
    /// Baud rate for serial monitoring after flash (default: 115200)
    #[serde(default = "default_monitor_baud")]
    monitor_baud: u32,
    /// Flash address for raw binary files. If omitted, ELF format is assumed.
    flash_address: Option<u32>,
    /// Path to a custom partition table CSV or binary file
    partition_table: Option<String>,
    /// Path to a custom bootloader binary file
    bootloader: Option<String>,
    /// Maximum time to monitor after flash in seconds (default: 5)
    #[serde(default = "default_timeout_secs")]
    timeout_secs: f64,
    /// Stop monitoring when this (unanchored) regex matches anywhere in the output.
    /// Substring regex with alternation: `RESULT (PASS|FAIL)`, `panic|Guru Meditation`.
    stop_pattern: Option<String>,
    /// Stop after no new data is received for this many milliseconds (default: 4000).
    #[serde(default = "default_idle_timeout_ms")]
    idle_timeout_ms: u64,
    /// Drop ROM garbage and ESP-IDF bootloader log lines before app output (default: true).
    #[serde(default = "default_true")]
    strip_boot_noise: bool,
    /// Strip ANSI escape / color sequences from the output (default: true).
    #[serde(default = "default_true")]
    strip_ansi: bool,
    /// When stop_pattern matches, return only the matched line plus this many lines before it.
    context_lines: Option<usize>,
    /// Cap on captured bytes; stops early and marks output truncated (default: 65536).
    #[serde(default = "default_max_bytes")]
    max_bytes: usize,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RerunInput {
    /// Serial port path
    port: String,
    /// Baud rate for serial monitoring (default: 115200)
    #[serde(default = "default_monitor_baud")]
    baud: u32,
    /// Maximum time to monitor in seconds (default: 5)
    #[serde(default = "default_timeout_secs")]
    timeout_secs: f64,
    /// Stop monitoring when this (unanchored) regex matches anywhere in the output.
    /// Substring regex with alternation: `RESULT (PASS|FAIL)`, `panic|Guru Meditation`.
    stop_pattern: Option<String>,
    /// Stop after no new data is received for this many milliseconds (default: 4000).
    #[serde(default = "default_idle_timeout_ms")]
    idle_timeout_ms: u64,
    /// Drop ROM garbage and ESP-IDF bootloader log lines before app output (default: true).
    #[serde(default = "default_true")]
    strip_boot_noise: bool,
    /// Strip ANSI escape / color sequences from the output (default: true).
    #[serde(default = "default_true")]
    strip_ansi: bool,
    /// When stop_pattern matches, return only the matched line plus this many lines before it.
    context_lines: Option<usize>,
    /// Cap on captured bytes; stops early and marks output truncated (default: 65536).
    #[serde(default = "default_max_bytes")]
    max_bytes: usize,
}

fn default_baud() -> u32 {
    460800
}

fn default_monitor_baud() -> u32 {
    115200
}

fn default_timeout_secs() -> f64 {
    5.0
}

fn default_idle_timeout_ms() -> u64 {
    4000
}

fn default_true() -> bool {
    true
}

fn default_max_bytes() -> usize {
    65536
}

// --- Connection helper ---

fn connect_to_device(port_name: &str, baud: u32, use_stub: bool) -> Result<Flasher, String> {
    let ports = serialport::available_ports()
        .map_err(|e| format!("Failed to enumerate serial ports: {e}"))?;

    let port_info = ports.iter().find(|p| p.port_name == port_name);

    let usb_info = match port_info.map(|p| &p.port_type) {
        Some(SerialPortType::UsbPort(info)) => info.clone(),
        _ => serialport::UsbPortInfo {
            vid: 0,
            pid: 0,
            serial_number: None,
            manufacturer: None,
            product: None,
        },
    };

    let serial = serialport::new(port_name, 115_200)
        .open_native()
        .map_err(|e| format!("Failed to open serial port '{port_name}': {e}"))?;

    let connection = Connection::new(
        serial,
        usb_info,
        ResetAfterOperation::HardReset,
        ResetBeforeOperation::DefaultReset,
        115_200,
    );

    Flasher::connect(
        connection,
        use_stub,
        true, // verify
        true, // skip unchanged
        None, // auto-detect chip
        if baud > 115_200 { Some(baud) } else { None },
    )
    .map_err(|e| format!("Failed to connect to device: {e}"))
}

// --- Output cleaning helpers ---

/// Matches a single ANSI escape sequence (CSI sequences such as color codes).
static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?=]*[ -/]*[@-~]").unwrap());

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

/// Remove ANSI escape sequences from `text`.
fn strip_ansi(text: &str) -> String {
    ANSI_RE.replace_all(text, "").into_owned()
}

/// The ROM bootlog is emitted at 74880 baud; read at 115200 it decodes to a blob
/// of U+FFFD replacement chars merged directly into the first readable boot line
/// (e.g. `..x\u{FFFD}x\u{FFFD}I (277) esp_image:`). Cut everything up to and
/// including the last replacement char so the readable tail can be classified.
fn strip_garbled_prefix(line: &str) -> &str {
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
fn is_boot_noise_line(line: &str) -> bool {
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
fn strip_boot_noise(raw: &str) -> String {
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

/// When a stop pattern matched, focus the output on the match: keep up to and
/// including the line containing the last match (dropping any post-match reboot
/// junk), and at most `context_lines` lines before it. Returns None if the
/// (cleaned) text no longer contains the pattern.
fn trim_to_match(text: &str, re: &Regex, context_lines: Option<usize>) -> Option<String> {
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

    let start_line = match context_lines {
        Some(n) => match_line.saturating_sub(n),
        None => 0,
    };
    Some(lines[start_line..=match_line].join("\n"))
}

/// Apply boot-noise stripping, ANSI stripping, and match focusing to a raw capture.
fn process_capture(
    raw: &str,
    strip_boot_noise_opt: bool,
    strip_ansi_opt: bool,
    stop_re: Option<&Regex>,
    matched: bool,
    context_lines: Option<usize>,
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
        && let Some(focused) = trim_to_match(&text, re, context_lines)
    {
        text = focused;
    }
    text
}

// --- Serial monitor helper ---

struct MonitorResult {
    output: String,
    stop_reason: &'static str,
    matched: bool,
    truncated: bool,
}

#[allow(clippy::too_many_arguments)]
fn read_serial_output(
    port_name: &str,
    baud: u32,
    timeout: Duration,
    idle_timeout: Duration,
    stop_pattern: Option<&Regex>,
    flush: bool,
    max_bytes: usize,
) -> Result<MonitorResult, String> {
    let mut port = serialport::new(port_name, baud)
        .timeout(Duration::from_millis(100))
        .open()
        .map_err(|e| format!("Failed to open serial port '{port_name}': {e}"))?;

    if flush {
        // Discard anything already buffered (e.g. the tail of a previous run).
        let _ = port.clear(ClearBuffer::Input);
    }

    let mut output = String::new();
    let mut buf = [0u8; 4096];
    let start = Instant::now();
    let mut last_data = Instant::now();

    // After a stop_pattern match, linger briefly so the rest of the matched line
    // arrives (the line is often not newline-terminated at the instant of match).
    let grace = Duration::from_millis(250);
    let mut match_grace_until: Option<Instant> = None;
    let mut match_end: Option<usize> = None;

    loop {
        if let Some(end) = match_end {
            // Stop as soon as the matched line is complete, or the grace elapses.
            if output[end..].contains('\n') {
                return Ok(MonitorResult {
                    output,
                    stop_reason: "stop pattern matched",
                    matched: true,
                    truncated: false,
                });
            }
            if match_grace_until.is_some_and(|t| Instant::now() >= t) {
                return Ok(MonitorResult {
                    output,
                    stop_reason: "stop pattern matched",
                    matched: true,
                    truncated: false,
                });
            }
        }

        if start.elapsed() >= timeout {
            return Ok(MonitorResult {
                output,
                stop_reason: "timeout reached",
                matched: match_end.is_some(),
                truncated: false,
            });
        }

        if match_end.is_none() && last_data.elapsed() >= idle_timeout && !output.is_empty() {
            return Ok(MonitorResult {
                output,
                stop_reason: "idle timeout (no new data)",
                matched: false,
                truncated: false,
            });
        }

        match port.read(&mut buf) {
            Ok(n) if n > 0 => {
                last_data = Instant::now();
                let chunk = String::from_utf8_lossy(&buf[..n]);
                output.push_str(&chunk);

                if output.len() >= max_bytes {
                    output.truncate(max_bytes);
                    return Ok(MonitorResult {
                        output,
                        stop_reason: "output cap reached",
                        matched: match_end.is_some(),
                        truncated: true,
                    });
                }

                if match_end.is_none()
                    && let Some(re) = stop_pattern
                    && let Some(m) = re.find(&output)
                {
                    match_end = Some(m.end());
                    match_grace_until = Some(Instant::now() + grace);
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                if output.is_empty() {
                    return Err(format!("Serial read error: {e}"));
                }
                return Ok(MonitorResult {
                    output,
                    stop_reason: "serial read error",
                    matched: match_end.is_some(),
                    truncated: false,
                });
            }
        }
    }
}

/// Format a captured monitor result into the displayed serial-output block.
fn render_capture(
    port: &str,
    baud: u32,
    raw: &MonitorResult,
    strip_boot_noise_opt: bool,
    strip_ansi_opt: bool,
    stop_re: Option<&Regex>,
    context_lines: Option<usize>,
) -> String {
    let processed = process_capture(
        &raw.output,
        strip_boot_noise_opt,
        strip_ansi_opt,
        stop_re,
        raw.matched,
        context_lines,
    );

    let mut header = format!(
        "Port: {} @ {} baud\nStopped: {}\nCaptured {} raw bytes",
        port,
        baud,
        raw.stop_reason,
        raw.output.len()
    );
    if processed.len() != raw.output.len() {
        header.push_str(&format!(" ({} shown after cleaning)", processed.len()));
    }
    if raw.truncated {
        header.push_str("\n[truncated: output cap reached, capture stopped early]");
    }

    let body = if processed.is_empty() {
        "(no application output\u{2014}only boot/ROM noise was captured)".to_string()
    } else {
        processed
    };

    format!("{header}\n\n```\n{body}\n```")
}

// --- MCP Server ---

#[derive(Clone)]
struct EspflashServer {
    tool_router: ToolRouter<Self>,
}

impl EspflashServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl EspflashServer {
    #[tool(description = "List available serial ports that could have ESP devices attached")]
    async fn list_ports(
        &self,
        rmcp::handler::server::wrapper::Parameters(_input): rmcp::handler::server::wrapper::Parameters<ListPortsInput>,
    ) -> Result<CallToolResult, McpError> {
        let ports = tokio::task::spawn_blocking(serialport::available_ports)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if ports.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No serial ports found.",
            )]));
        }

        let mut output = format!("Found {} serial port(s):\n\n", ports.len());
        for port in &ports {
            output.push_str(&format!("- {}", port.port_name));
            match &port.port_type {
                SerialPortType::UsbPort(info) => {
                    output.push_str(&format!(" [USB {:04x}:{:04x}", info.vid, info.pid));
                    if let Some(product) = &info.product {
                        output.push_str(&format!(" {product}"));
                    }
                    if let Some(manufacturer) = &info.manufacturer {
                        output.push_str(&format!(" ({manufacturer})"));
                    }
                    output.push(']');
                }
                SerialPortType::PciPort => output.push_str(" [PCI]"),
                _ => {}
            }
            output.push('\n');
        }

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        description = "Connect to an ESP device and retrieve chip information including type, revision, MAC address, flash size, and features"
    )]
    async fn chip_info(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<ChipInfoInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut flasher = connect_to_device(&input.port, input.baud, true)?;

            let info = flasher
                .device_info()
                .map_err(|e| format!("Failed to get device info: {e}"))?;

            let mut output = String::from("## Device Information\n\n");
            output.push_str(&format!("- Chip: {}\n", info.chip));
            if let Some((major, minor)) = info.revision {
                output.push_str(&format!("- Revision: v{major}.{minor}\n"));
            }
            output.push_str(&format!("- Crystal frequency: {}\n", info.crystal_frequency));
            output.push_str(&format!("- Flash size: {}\n", info.flash_size));
            if let Some(mac) = &info.mac_address {
                output.push_str(&format!("- MAC address: {mac}\n"));
            }
            if !info.features.is_empty() {
                output.push_str(&format!("- Features: {}\n", info.features.join(", ")));
            }

            Ok(output)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(
        description = "Flash an ELF or raw binary file to an ESP device. For ELF files, the IDF bootloader format is used automatically. For raw binaries, provide a flash_address."
    )]
    async fn flash(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<FlashInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let file_data = std::fs::read(&input.file_path)
                .map_err(|e| format!("Failed to read file '{}': {e}", input.file_path))?;

            let mut flasher = connect_to_device(&input.port, input.baud, true)?;

            if let Some(addr) = input.flash_address {
                // Raw binary mode
                flasher
                    .write_bin_to_flash(addr, &file_data, &mut DefaultProgressCallback)
                    .map_err(|e| format!("Failed to write binary to flash: {e}"))?;

                Ok(format!(
                    "Successfully flashed {} bytes to address 0x{:08x}",
                    file_data.len(),
                    addr
                ))
            } else {
                // ELF mode with IDF bootloader format
                let info = flasher
                    .device_info()
                    .map_err(|e| format!("Failed to get device info: {e}"))?;

                let flash_data = FlashData::new(
                    FlashSettings::default(),
                    0,    // min_chip_rev
                    None, // mmu_page_size (auto)
                    info.chip,
                    info.crystal_frequency,
                );

                let image = IdfBootloaderFormat::new(
                    &file_data,
                    &flash_data,
                    input.partition_table.as_deref().map(Path::new),
                    input.bootloader.as_deref().map(Path::new),
                    None, // partition_table_offset
                    None, // target_app_partition
                )
                .map_err(|e| format!("Failed to create flash image: {e}"))?;

                flasher
                    .load_image_to_flash(&mut DefaultProgressCallback, ImageFormat::EspIdf(image))
                    .map_err(|e| format!("Failed to flash image: {e}"))?;

                Ok(format!(
                    "Successfully flashed ELF ({} bytes) to {} device",
                    file_data.len(),
                    info.chip
                ))
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(
        description = "Erase the entire flash memory of the connected ESP device. WARNING: This is irreversible and will delete all data including firmware."
    )]
    async fn erase_flash(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<EraseFlashInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut flasher = connect_to_device(&input.port, input.baud, true)?;

            flasher
                .erase_flash()
                .map_err(|e| format!("Failed to erase flash: {e}"))?;

            Ok("Successfully erased entire flash memory.".to_string())
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(
        description = "Erase a specific region of flash memory. Both address and size must be 4096-byte (0x1000) aligned."
    )]
    async fn erase_region(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<EraseRegionInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            if input.address % 0x1000 != 0 {
                return Err(format!(
                    "Address 0x{:08x} is not 4096-byte aligned",
                    input.address
                ));
            }
            if input.size % 0x1000 != 0 {
                return Err(format!("Size 0x{:x} is not 4096-byte aligned", input.size));
            }

            let mut flasher = connect_to_device(&input.port, input.baud, true)?;

            flasher
                .erase_region(input.address, input.size)
                .map_err(|e| format!("Failed to erase region: {e}"))?;

            Ok(format!(
                "Successfully erased 0x{:x} bytes at address 0x{:08x}",
                input.size, input.address
            ))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(description = "Read flash memory contents and save to a file")]
    async fn read_flash(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<ReadFlashInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut flasher = connect_to_device(&input.port, input.baud, true)?;

            let output_path = std::path::PathBuf::from(&input.output_path);

            flasher
                .read_flash(
                    input.address,
                    input.size,
                    0x400, // block_size
                    32,    // max_in_flight
                    output_path.clone(),
                )
                .map_err(|e| format!("Failed to read flash: {e}"))?;

            let file_size = std::fs::metadata(&output_path).map(|m| m.len()).unwrap_or(0);

            Ok(format!(
                "Successfully read 0x{:x} bytes from address 0x{:08x} to '{}' ({file_size} bytes written)",
                input.size,
                input.address,
                output_path.display()
            ))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(description = "Reset the connected ESP device using DTR/RTS serial control lines")]
    async fn reset_device(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<ResetDeviceInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            // Connect without stub for reset (matches espflash CLI behavior)
            let mut flasher = connect_to_device(&input.port, 115_200, false)?;

            flasher
                .connection()
                .reset()
                .map_err(|e| format!("Failed to reset device: {e}"))?;

            Ok("Device reset successfully.".to_string())
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(description = "Compute the MD5 checksum of a flash memory region")]
    async fn checksum_md5(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<ChecksumMd5Input>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut flasher = connect_to_device(&input.port, input.baud, true)?;

            let checksum = flasher
                .checksum_md5(input.address, input.size)
                .map_err(|e| format!("Failed to compute checksum: {e}"))?;

            Ok(format!(
                "MD5 checksum of 0x{:x} bytes at 0x{:08x}: {:032x}",
                input.size, input.address, checksum
            ))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(
        description = "Read serial output from an ESP device for a bounded duration. By default the input buffer is flushed first (drops a previous run's tail), ROM/bootloader boot noise and ANSI color codes are stripped, and on a stop_pattern match the output is focused on the matched line. Stops on: max timeout, regex pattern match, idle timeout (no new data), or output cap."
    )]
    async fn monitor(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<MonitorInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let stop_re = input
                .stop_pattern
                .as_deref()
                .map(|p| Regex::new(p).map_err(|e| format!("Invalid regex pattern: {e}")))
                .transpose()?;

            let monitor = read_serial_output(
                &input.port,
                input.baud,
                Duration::from_secs_f64(input.timeout_secs),
                Duration::from_millis(input.idle_timeout_ms),
                stop_re.as_ref(),
                input.flush,
                input.max_bytes,
            )?;

            let block = render_capture(
                &input.port,
                input.baud,
                &monitor,
                input.strip_boot_noise,
                input.strip_ansi,
                stop_re.as_ref(),
                input.context_lines,
            );

            Ok(format!("## Serial Monitor Output\n\n{block}"))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(
        description = "Flash firmware to an ESP device, then immediately monitor serial output to verify the boot. Captures from boot (no flush) so it sees the full startup; ROM/bootloader noise and ANSI codes are stripped by default, and on a stop_pattern match the output is focused on the matched line. Stops monitoring on: max timeout, regex match, idle timeout, or output cap."
    )]
    async fn flash_monitor(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<FlashMonitorInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let file_data = std::fs::read(&input.file_path)
                .map_err(|e| format!("Failed to read file '{}': {e}", input.file_path))?;

            // --- Flash phase ---
            let mut flasher = connect_to_device(&input.port, input.flash_baud, true)?;

            let flash_msg = if let Some(addr) = input.flash_address {
                flasher
                    .write_bin_to_flash(addr, &file_data, &mut DefaultProgressCallback)
                    .map_err(|e| format!("Failed to write binary to flash: {e}"))?;
                format!("Flashed {} bytes to 0x{:08x}", file_data.len(), addr)
            } else {
                let info = flasher
                    .device_info()
                    .map_err(|e| format!("Failed to get device info: {e}"))?;

                let flash_data = FlashData::new(
                    FlashSettings::default(),
                    0,
                    None,
                    info.chip,
                    info.crystal_frequency,
                );

                let image = IdfBootloaderFormat::new(
                    &file_data,
                    &flash_data,
                    input.partition_table.as_deref().map(Path::new),
                    input.bootloader.as_deref().map(Path::new),
                    None,
                    None,
                )
                .map_err(|e| format!("Failed to create flash image: {e}"))?;

                flasher
                    .load_image_to_flash(&mut DefaultProgressCallback, ImageFormat::EspIdf(image))
                    .map_err(|e| format!("Failed to flash image: {e}"))?;

                format!("Flashed ELF ({} bytes) to {}", file_data.len(), info.chip)
            };

            // Drop flasher to release the serial port (hard reset happens here)
            drop(flasher);

            // Small delay to let the device start booting after reset
            std::thread::sleep(Duration::from_millis(100));

            // --- Monitor phase ---
            let stop_re = input
                .stop_pattern
                .as_deref()
                .map(|p| Regex::new(p).map_err(|e| format!("Invalid regex pattern: {e}")))
                .transpose()?;

            let monitor = read_serial_output(
                &input.port,
                input.monitor_baud,
                Duration::from_secs_f64(input.timeout_secs),
                Duration::from_millis(input.idle_timeout_ms),
                stop_re.as_ref(),
                false, // do not flush: we want the boot output
                input.max_bytes,
            )?;

            let block = render_capture(
                &input.port,
                input.monitor_baud,
                &monitor,
                input.strip_boot_noise,
                input.strip_ansi,
                stop_re.as_ref(),
                input.context_lines,
            );

            Ok(format!(
                "## Flash + Monitor\n\n{flash_msg}\n\n### Serial Output\n\n{block}"
            ))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(
        description = "Re-run the firmware already on the device: hardware reset (DTR/RTS), flush the buffer, then monitor the fresh boot. One call instead of reset_device + monitor; ideal for iterating on the same binary without reflashing. ROM/bootloader noise and ANSI codes are stripped by default, and on a stop_pattern match the output is focused on the matched line."
    )]
    async fn rerun(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<RerunInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let stop_re = input
                .stop_pattern
                .as_deref()
                .map(|p| Regex::new(p).map_err(|e| format!("Invalid regex pattern: {e}")))
                .transpose()?;

            // --- Reset phase (no stub, matches reset_device / espflash CLI) ---
            let mut flasher = connect_to_device(&input.port, 115_200, false)?;
            flasher
                .connection()
                .reset()
                .map_err(|e| format!("Failed to reset device: {e}"))?;
            drop(flasher);

            // Let the ROM bootloader start before we open the monitor.
            std::thread::sleep(Duration::from_millis(100));

            // --- Monitor phase (flush drops connect/download-mode noise) ---
            let monitor = read_serial_output(
                &input.port,
                input.baud,
                Duration::from_secs_f64(input.timeout_secs),
                Duration::from_millis(input.idle_timeout_ms),
                stop_re.as_ref(),
                true, // flush: start the capture clean
                input.max_bytes,
            )?;

            let block = render_capture(
                &input.port,
                input.baud,
                &monitor,
                input.strip_boot_noise,
                input.strip_ansi,
                stop_re.as_ref(),
                input.context_lines,
            );

            Ok(format!("## Rerun (reset + monitor)\n\n{block}"))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }
}

#[tool_handler]
impl ServerHandler for EspflashServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: env!("CARGO_PKG_NAME").to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            },
            instructions: Some(
                "ESP device flash tool. Connect to ESP32/ESP32-S2/S3/C2/C3/C5/C6/H2/P4 \
                 devices via serial port for flashing, erasing, and reading flash memory.\n\n\
                 ## Tools\n\
                 - list_ports: Discover available serial ports\n\
                 - chip_info: Get device type, revision, MAC, flash size\n\
                 - flash: Flash ELF or raw binary firmware\n\
                 - flash_monitor: Flash firmware then capture boot serial output\n\
                 - rerun: Reset the device and monitor the fresh boot (no reflash)\n\
                 - monitor: Read serial output with timeout/pattern stop\n\
                 - erase_flash: Erase entire flash (destructive)\n\
                 - erase_region: Erase specific flash region (destructive)\n\
                 - read_flash: Read flash contents to file\n\
                 - reset_device: Hardware reset the device\n\
                 - checksum_md5: Compute MD5 of flash region\n\n\
                 All tools that communicate with a device require a serial port path. \
                 Use list_ports first to discover available ports.\n\n\
                 ## Monitoring\n\
                 monitor, flash_monitor, and rerun stop when: max timeout reached, \
                 a regex stop_pattern matches (substring/unanchored, alternation OK), \
                 idle_timeout_ms passes with no new data, or the byte cap is reached. \
                 By default they strip ROM baud-mismatch garbage + ESP-IDF bootloader \
                 log lines (strip_boot_noise) and ANSI color codes (strip_ansi), and on \
                 a stop_pattern match they focus output on the matched line (set \
                 context_lines for N preceding lines). Defaults: 5s timeout, 4s idle.\n\n\
                 For iterating on the same firmware, prefer rerun over reset_device + \
                 monitor. flash_monitor captures from boot (it does not flush)."
                    .into(),
            ),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    info!("Starting espflash MCP server");

    let server = EspflashServer::new();
    let transport = rmcp::transport::io::stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A realistic capture: ROM baud-mismatch garbage merged into the first boot
    // line, ESP-IDF bootloader logs, then ANSI-colored application output.
    fn sample_capture() -> String {
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
        let out = process_capture(&raw, true, true, None, false, None);
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
        let out = process_capture(&raw, true, true, Some(&re), true, Some(0));
        assert_eq!(out, "INFO - RESULT PASS (100/100)");
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
