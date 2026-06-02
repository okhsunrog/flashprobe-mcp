//! Capture tools: `monitor` (attach only), `flash_monitor` (flash then capture
//! from boot), and `rerun` (reset + capture, optionally N times). Each resolves
//! a backend (espflash serial or probe-rs RTT), a decode mode (text or defmt,
//! from the ELF), then runs the shared capture pipeline. Grouped into the
//! `capture_router`.

use crate::backend::espflash::{SerialSource, connect_to_device, detect_serial_port, flash_file};
use crate::backend::{BackendKind, parse_backend};
use crate::capture::decode::load_defmt_table;
use crate::detect::Detector;
use crate::capture::filter::{compile_opt_regex, process_capture};
use crate::capture::render::{RenderOpts, last_nonempty_line, render_block, truncate_line};
use crate::capture::{
    ByteSource, CaptureOpts, CaptureResult, DecodeMode, DefmtFraming, DefmtStats, Level, capture,
    raw_text,
};
use crate::inputs::*;
use crate::server::Server;
use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use std::time::Duration;

#[cfg(feature = "probe-rs")]
use crate::backend::probers;

#[tool_router(router = capture_router, vis = "pub(crate)")]
impl Server {
    #[tool(
        description = "Read output from a device for a bounded window. Backend (REQUIRED): \"probe-rs\" (RTT) or \"espflash\" (UART). The ELF auto-detects from the project for defmt decode (structured level/module; or pass `elf`); else plain text. Stops on: regex `stop`, `stop_on_level` (defmt), idle_ms, max timeout, or byte cap. Text mode strips boot noise + ANSI and focuses on the `stop` match."
    )]
    async fn monitor(
        &self,
        Parameters(input): Parameters<MonitorInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let stop_re = compile_opt_regex(input.stop.as_deref())?;
            let grep_re = compile_opt_regex(input.grep.as_deref())?;
            let module_re = compile_opt_regex(input.module.as_deref())?;
            let min_level = parse_level_opt(input.level.as_deref())?;
            let opts = CaptureOpts {
                timeout: Duration::from_secs_f64(input.timeout_s),
                idle: Duration::from_millis(input.idle_ms),
                stop: stop_re.clone(),
                stop_on_level: parse_level_opt(input.stop_on_level.as_deref())?,
                flush: input.flush,
                max_bytes: input.max_bytes,
            };
            let mut det = Detector::new(input.project_dir.as_deref(), input.bin.as_deref());

            let (mut source, header, framing): (Box<dyn ByteSource>, String, DefmtFraming) =
                match parse_backend(input.backend.as_deref())? {
                    BackendKind::Espflash => {
                        let port = detect_serial_port(input.port.as_deref())?;
                        let src = SerialSource::open(&port, input.baud)?;
                        (
                            Box::new(src),
                            format!("Port: {port} @ {} baud", input.baud),
                            DefmtFraming::EspPrintln,
                        )
                    }
                    #[cfg(feature = "probe-rs")]
                    BackendKind::ProbeRs => {
                        let chip = det.chip(input.chip.as_deref())?;
                        let session = probers::open_session(&chip, input.probe.as_deref())?;
                        (
                            Box::new(probers::RttSource::attach(session)?),
                            format!("Probe: {chip} via RTT"),
                            DefmtFraming::Raw,
                        )
                    }
                };

            // Auto-detect the ELF for defmt decode (text mode if none found).
            let elf = det.elf_opt(input.elf.as_deref());
            let defmt = load_optional_table(elf.as_deref())?;
            let mode = decode_mode(&defmt, framing);
            let (result, stats) = capture(source.as_mut(), &mode, &opts)?;

            let block = render_block(
                &header,
                &result,
                stats,
                &render_opts(&input.strip_boot_noise, input.strip_ansi, stop_re.as_ref(),
                    input.context, grep_re.as_ref(), min_level, module_re.as_ref()),
            );
            Ok(format!("## Serial Monitor Output\n\n{block}"))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(
        description = "Flash firmware, then immediately capture output to verify the boot. Backend (REQUIRED): \"probe-rs\" (flash + RTT) or \"espflash\" (flash + UART). File/chip auto-detect from the project; the flashed ELF is the defmt source. Captures from boot. Stops on: regex `stop`, `stop_on_level` (defmt), idle_ms, max timeout, or byte cap."
    )]
    async fn flash_monitor(
        &self,
        Parameters(input): Parameters<FlashMonitorInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let stop_re = compile_opt_regex(input.stop.as_deref())?;
            let grep_re = compile_opt_regex(input.grep.as_deref())?;
            let module_re = compile_opt_regex(input.module.as_deref())?;
            let min_level = parse_level_opt(input.level.as_deref())?;
            let opts = CaptureOpts {
                timeout: Duration::from_secs_f64(input.timeout_s),
                idle: Duration::from_millis(input.idle_ms),
                stop: stop_re.clone(),
                stop_on_level: parse_level_opt(input.stop_on_level.as_deref())?,
                flush: false, // do not flush: we want the boot output
                max_bytes: input.max_bytes,
            };
            let mut det = Detector::new(input.project_dir.as_deref(), input.bin.as_deref());
            // The file to flash: explicit, else the detected build artifact.
            let file_path = det.elf(input.file_path.as_deref())?;

            let (flash_msg, mut source, header, framing): (
                String,
                Box<dyn ByteSource>,
                String,
                DefmtFraming,
            ) = match parse_backend(input.backend.as_deref())? {
                BackendKind::Espflash => {
                    let port = detect_serial_port(input.port.as_deref())?;
                    let file_data = std::fs::read(&file_path)
                        .map_err(|e| format!("Failed to read file '{file_path}': {e}"))?;
                    let mut flasher = connect_to_device(&port, input.flash_baud, true)?;
                    let msg = flash_file(
                        &mut flasher,
                        &file_data,
                        input.flash_address,
                        input.partition_table.as_deref(),
                        input.bootloader.as_deref(),
                    )?;
                    // flash_file already reset the chip into the app; drop the
                    // flasher only to release the serial port for the monitor.
                    drop(flasher);
                    std::thread::sleep(Duration::from_millis(100));
                    let src = SerialSource::open(&port, input.monitor_baud)?;
                    (
                        msg,
                        Box::new(src),
                        format!("Port: {port} @ {} baud", input.monitor_baud),
                        DefmtFraming::EspPrintln,
                    )
                }
                #[cfg(feature = "probe-rs")]
                BackendKind::ProbeRs => {
                    let chip = det.chip(input.chip.as_deref())?;
                    let mut session = probers::open_session(&chip, input.probe.as_deref())?;
                    let msg = probers::download(&mut session, &file_path, &chip)?;
                    // Reset + attach RTT so capture starts at the run's beginning.
                    let src = probers::reset_and_attach_rtt(session, Some(&file_path))?;
                    (
                        msg,
                        Box::new(src),
                        format!("Probe: {chip} via RTT"),
                        DefmtFraming::Raw,
                    )
                }
            };

            // The flashed ELF is the defmt source (unless a raw bin was flashed).
            let elf_path = input
                .elf
                .clone()
                .or_else(|| (input.flash_address.is_none()).then(|| file_path.clone()));
            let defmt = load_optional_table(elf_path.as_deref())?;
            let mode = decode_mode(&defmt, framing);
            let (result, stats) = capture(source.as_mut(), &mode, &opts)?;

            let block = render_block(
                &header,
                &result,
                stats,
                &render_opts(&input.strip_boot_noise, input.strip_ansi, stop_re.as_ref(),
                    input.context, grep_re.as_ref(), min_level, module_re.as_ref()),
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
        description = "Re-run the firmware already on the device: reset (DTR/RTS for espflash, core reset for probe-rs), then capture the fresh boot. Backend (REQUIRED): \"probe-rs\" (RTT) or \"espflash\" (UART). ELF/chip auto-detect from the project for defmt decode. One call instead of reset + monitor. Set repeat > 1 to run N cycles back-to-back for a compact per-run summary - useful for characterizing flaky/intermittent bugs."
    )]
    async fn rerun(
        &self,
        Parameters(input): Parameters<RerunInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let stop_re = compile_opt_regex(input.stop.as_deref())?;
            let grep_re = compile_opt_regex(input.grep.as_deref())?;
            let module_re = compile_opt_regex(input.module.as_deref())?;
            let min_level = parse_level_opt(input.level.as_deref())?;
            let stop_on_level = parse_level_opt(input.stop_on_level.as_deref())?;
            let repeat = input.repeat.clamp(1, 50);
            let mut det = Detector::new(input.project_dir.as_deref(), input.bin.as_deref());

            // The resolved connection, reused across repeat cycles.
            enum Conn {
                Serial(String),
                #[cfg(feature = "probe-rs")]
                Jtag(String),
            }

            let (header, framing, conn) = match parse_backend(input.backend.as_deref())? {
                BackendKind::Espflash => {
                    let port = detect_serial_port(input.port.as_deref())?;
                    let header = format!("Port: {port} @ {} baud", input.baud);
                    (header, DefmtFraming::EspPrintln, Conn::Serial(port))
                }
                #[cfg(feature = "probe-rs")]
                BackendKind::ProbeRs => {
                    let chip = det.chip(input.chip.as_deref())?;
                    let header = format!("Probe: {chip} via RTT");
                    (header, DefmtFraming::Raw, Conn::Jtag(chip))
                }
            };

            let elf = det.elf_opt(input.elf.as_deref());
            let defmt = load_optional_table(elf.as_deref())?;

            // One reset + flush + capture on the selected backend / decode mode.
            let one_cycle = || -> Result<(CaptureResult, Option<DefmtStats>), String> {
                let opts = CaptureOpts {
                    timeout: Duration::from_secs_f64(input.timeout_s),
                    idle: Duration::from_millis(input.idle_ms),
                    stop: stop_re.clone(),
                    stop_on_level,
                    flush: true, // start each capture clean
                    max_bytes: input.max_bytes,
                };
                let mode = decode_mode(&defmt, framing);
                let mut source: Box<dyn ByteSource> = match &conn {
                    Conn::Serial(port) => {
                        // No stub, matches reset_device / espflash CLI.
                        let mut flasher = connect_to_device(port, 115_200, false)?;
                        flasher
                            .connection()
                            .reset()
                            .map_err(|e| format!("Failed to reset device: {e}"))?;
                        drop(flasher);
                        std::thread::sleep(Duration::from_millis(100));
                        Box::new(SerialSource::open(port, input.baud)?)
                    }
                    #[cfg(feature = "probe-rs")]
                    Conn::Jtag(chip) => {
                        let session = probers::open_session(chip, input.probe.as_deref())?;
                        // Reset + attach RTT so each cycle captures from the start.
                        Box::new(probers::reset_and_attach_rtt(session, elf.as_deref())?)
                    }
                };
                capture(source.as_mut(), &mode, &opts)
            };

            if repeat == 1 {
                let (result, stats) = one_cycle()?;
                let block = render_block(
                    &header,
                    &result,
                    stats,
                    &render_opts(&input.strip_boot_noise, input.strip_ansi, stop_re.as_ref(),
                        input.context, grep_re.as_ref(), min_level, module_re.as_ref()),
                );
                return Ok(format!("## Rerun (reset + monitor)\n\n{block}"));
            }

            // repeat > 1: compact summary, one line per run.
            let mut matched_count = 0usize;
            let mut rows = String::new();
            for i in 1..=repeat {
                let (mr, stats) = one_cycle()?;
                if mr.matched {
                    matched_count += 1;
                }
                let summary = run_summary(
                    &mr,
                    stats.is_some(),
                    stop_re.as_ref(),
                    input.strip_boot_noise,
                    input.strip_ansi,
                    grep_re.as_ref(),
                );
                let tag = if mr.matched { "match" } else { mr.stop_reason.as_str() };
                rows.push_str(&format!("{i:>2}. [{tag}] {}\n", truncate_line(&summary, 200)));
            }

            let header = format!(
                "## Rerun \u{00d7}{repeat} (reset + monitor)\n\n{header}\n\
                 stop matched in {matched_count}/{repeat} runs"
            );
            Ok(format!("{header}\n\n```\n{rows}```"))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }
}

/// Parse an optional level name (`info`, `error`, …).
fn parse_level_opt(s: Option<&str>) -> Result<Option<Level>, String> {
    s.map(Level::parse).transpose()
}

/// Load a defmt table from an optional ELF path (None → text mode).
type DefmtTable = (defmt_decoder::Table, Vec<u8>);
fn load_optional_table(elf: Option<&str>) -> Result<Option<DefmtTable>, String> {
    match elf {
        Some(path) => load_defmt_table(path),
        None => Ok(None),
    }
}

/// Pick the decode mode from a (maybe) loaded defmt table. `framing` selects the
/// wire format and comes from the backend (serial → esp-println marker framing,
/// RTT → raw rzCOBS).
fn decode_mode(defmt: &Option<DefmtTable>, framing: DefmtFraming) -> DecodeMode<'_> {
    match defmt {
        Some((table, elf)) => DecodeMode::Defmt {
            table,
            elf,
            framing,
        },
        None => DecodeMode::Text,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_opts<'a>(
    strip_boot_noise: &bool,
    strip_ansi: bool,
    stop_re: Option<&'a regex::Regex>,
    context: Option<usize>,
    grep: Option<&'a regex::Regex>,
    min_level: Option<Level>,
    module: Option<&'a regex::Regex>,
) -> RenderOpts<'a> {
    RenderOpts {
        strip_boot_noise: *strip_boot_noise,
        strip_ansi,
        stop_re,
        context,
        grep,
        min_level,
        module,
    }
}

/// One-line summary of a capture for the repeat>1 table. In defmt mode it reads
/// the decoded lines directly; in text mode it runs the cleaning pipeline.
fn run_summary(
    mr: &CaptureResult,
    is_defmt: bool,
    stop_re: Option<&regex::Regex>,
    strip_boot_noise: bool,
    strip_ansi: bool,
    grep: Option<&regex::Regex>,
) -> String {
    if is_defmt {
        if mr.matched && let Some(re) = stop_re {
            return mr
                .lines
                .iter()
                .rev()
                .find(|l| re.is_match(&l.text))
                .map(|l| l.text.clone())
                .unwrap_or_default();
        }
        return mr
            .lines
            .iter()
            .rev()
            .map(|l| l.text.trim())
            .find(|t| !t.is_empty())
            .unwrap_or("(no output)")
            .to_string();
    }

    let raw = raw_text(mr);
    if mr.matched && stop_re.is_some() {
        process_capture(&raw, strip_boot_noise, strip_ansi, stop_re, true, Some(0), None)
    } else {
        let clean = process_capture(&raw, strip_boot_noise, strip_ansi, None, false, None, grep);
        last_nonempty_line(&clean)
    }
}
