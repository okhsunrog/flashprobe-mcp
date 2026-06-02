//! Capture tools: `monitor` (attach only), `flash_monitor` (flash then capture
//! from boot), and `rerun` (reset + capture, optionally N times). Each dispatches
//! to the espflash (serial) or probe-rs (RTT) backend, then runs the shared
//! capture pipeline. Grouped into the `capture_router`.

use crate::backend::espflash::{SerialSource, connect_to_device, flash_file};
use crate::backend::{BackendKind, parse_backend};
use crate::capture::filter::{compile_opt_regex, process_capture};
use crate::capture::render::{last_nonempty_line, render_capture, truncate_line};
use crate::capture::{ByteSource, CaptureOpts, CaptureResult, decode::TextDecoder, raw_text, run_capture};
use crate::inputs::*;
use crate::server::EspflashServer;
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
impl EspflashServer {
    #[tool(
        description = "Read serial/RTT output from a device for a bounded duration. Backend: espflash (serial, default) or probe-rs (RTT; pass `chip`). By default the buffer is flushed first, ROM/bootloader boot noise and ANSI color codes are stripped, and on a `stop` match the output is focused on the matched line. Stops on: max timeout, regex `stop` match, idle timeout (no new data), or output cap."
    )]
    async fn monitor(
        &self,
        Parameters(input): Parameters<MonitorInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let stop_re = compile_opt_regex(input.stop.as_deref())?;
            let grep_re = compile_opt_regex(input.grep.as_deref())?;
            let opts = CaptureOpts {
                timeout: Duration::from_secs_f64(input.timeout_s),
                idle: Duration::from_millis(input.idle_ms),
                stop: stop_re.clone(),
                flush: input.flush,
                max_bytes: input.max_bytes,
            };

            let (mut source, header): (Box<dyn ByteSource>, String) =
                match parse_backend(input.backend.as_deref())? {
                    BackendKind::Espflash => {
                        let port = require_port(input.port.as_deref())?;
                        let src = SerialSource::open(port, input.baud)?;
                        (Box::new(src), format!("Port: {port} @ {} baud", input.baud))
                    }
                    #[cfg(feature = "probe-rs")]
                    BackendKind::ProbeRs => {
                        let chip = require_chip(input.chip.as_deref())?;
                        let session = probers::open_session(chip, input.probe.as_deref())?;
                        let src = probers::RttSource::attach(session)?;
                        (Box::new(src), format!("Probe: {chip} via RTT"))
                    }
                };

            let mut decoder = TextDecoder::new();
            let result = run_capture(source.as_mut(), &mut decoder, &opts)?;

            let block = render_capture(
                &header,
                &result,
                input.strip_boot_noise,
                input.strip_ansi,
                stop_re.as_ref(),
                input.context,
                grep_re.as_ref(),
            );
            Ok(format!("## Serial Monitor Output\n\n{block}"))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(
        description = "Flash firmware to a device, then immediately capture output to verify the boot. Backend: espflash (serial, default) or probe-rs (JTAG/SWD flash + RTT; pass `chip`). Captures from boot (no flush); ROM/bootloader noise and ANSI codes are stripped by default, and on a `stop` match the output is focused on the matched line. Stops on: max timeout, regex match, idle timeout, or output cap."
    )]
    async fn flash_monitor(
        &self,
        Parameters(input): Parameters<FlashMonitorInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let stop_re = compile_opt_regex(input.stop.as_deref())?;
            let grep_re = compile_opt_regex(input.grep.as_deref())?;
            let opts = CaptureOpts {
                timeout: Duration::from_secs_f64(input.timeout_s),
                idle: Duration::from_millis(input.idle_ms),
                stop: stop_re.clone(),
                flush: false, // do not flush: we want the boot output
                max_bytes: input.max_bytes,
            };

            let (flash_msg, mut source, header): (String, Box<dyn ByteSource>, String) =
                match parse_backend(input.backend.as_deref())? {
                    BackendKind::Espflash => {
                        let port = require_port(input.port.as_deref())?;
                        let file_data = std::fs::read(&input.file_path)
                            .map_err(|e| format!("Failed to read file '{}': {e}", input.file_path))?;

                        let mut flasher = connect_to_device(port, input.flash_baud, true)?;
                        let msg = flash_file(
                            &mut flasher,
                            &file_data,
                            input.flash_address,
                            input.partition_table.as_deref(),
                            input.bootloader.as_deref(),
                        )?;
                        // Drop flasher to release the port (hard reset happens here),
                        // then let the device start booting before we open the monitor.
                        drop(flasher);
                        std::thread::sleep(Duration::from_millis(100));

                        let src = SerialSource::open(port, input.monitor_baud)?;
                        (
                            msg,
                            Box::new(src),
                            format!("Port: {port} @ {} baud", input.monitor_baud),
                        )
                    }
                    #[cfg(feature = "probe-rs")]
                    BackendKind::ProbeRs => {
                        let chip = require_chip(input.chip.as_deref())?;
                        let mut session = probers::open_session(chip, input.probe.as_deref())?;
                        let msg = probers::flash(&mut session, &input.file_path, chip)?;
                        let src = probers::RttSource::attach(session)?;
                        (msg, Box::new(src), format!("Probe: {chip} via RTT"))
                    }
                };

            let mut decoder = TextDecoder::new();
            let result = run_capture(source.as_mut(), &mut decoder, &opts)?;

            let block = render_capture(
                &header,
                &result,
                input.strip_boot_noise,
                input.strip_ansi,
                stop_re.as_ref(),
                input.context,
                grep_re.as_ref(),
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
        description = "Re-run the firmware already on the device: reset (DTR/RTS for espflash, core reset for probe-rs), then capture the fresh boot. Backend: espflash (serial, default) or probe-rs (RTT; pass `chip`). One call instead of reset + monitor; ideal for iterating without reflashing. Set repeat > 1 to run N cycles back-to-back and get a compact per-run summary (one matched line per run + a match count) - useful for characterizing flaky/intermittent bugs. ROM/bootloader noise and ANSI codes are stripped by default, and on a `stop` match the output is focused on the matched line."
    )]
    async fn rerun(
        &self,
        Parameters(input): Parameters<RerunInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let stop_re = compile_opt_regex(input.stop.as_deref())?;
            let grep_re = compile_opt_regex(input.grep.as_deref())?;
            let repeat = input.repeat.clamp(1, 50);
            let backend = parse_backend(input.backend.as_deref())?;

            let header = match &backend {
                BackendKind::Espflash => {
                    let port = require_port(input.port.as_deref())?;
                    format!("Port: {port} @ {} baud", input.baud)
                }
                #[cfg(feature = "probe-rs")]
                BackendKind::ProbeRs => {
                    let chip = require_chip(input.chip.as_deref())?;
                    format!("Probe: {chip} via RTT")
                }
            };

            // One reset + flush + capture, on the selected backend.
            let one_cycle = || -> Result<CaptureResult, String> {
                let opts = CaptureOpts {
                    timeout: Duration::from_secs_f64(input.timeout_s),
                    idle: Duration::from_millis(input.idle_ms),
                    stop: stop_re.clone(),
                    flush: true, // start each capture clean
                    max_bytes: input.max_bytes,
                };
                let mut decoder = TextDecoder::new();
                match &backend {
                    BackendKind::Espflash => {
                        let port = require_port(input.port.as_deref())?;
                        // No stub, matches reset_device / espflash CLI.
                        let mut flasher = connect_to_device(port, 115_200, false)?;
                        flasher
                            .connection()
                            .reset()
                            .map_err(|e| format!("Failed to reset device: {e}"))?;
                        drop(flasher);
                        // Let the ROM bootloader start before we open the monitor.
                        std::thread::sleep(Duration::from_millis(100));
                        let mut src = SerialSource::open(port, input.baud)?;
                        run_capture(&mut src, &mut decoder, &opts)
                    }
                    #[cfg(feature = "probe-rs")]
                    BackendKind::ProbeRs => {
                        let chip = require_chip(input.chip.as_deref())?;
                        let mut session = probers::open_session(chip, input.probe.as_deref())?;
                        probers::reset(&mut session)?;
                        let mut src = probers::RttSource::attach(session)?;
                        run_capture(&mut src, &mut decoder, &opts)
                    }
                }
            };

            if repeat == 1 {
                let result = one_cycle()?;
                let block = render_capture(
                    &header,
                    &result,
                    input.strip_boot_noise,
                    input.strip_ansi,
                    stop_re.as_ref(),
                    input.context,
                    grep_re.as_ref(),
                );
                return Ok(format!("## Rerun (reset + monitor)\n\n{block}"));
            }

            // repeat > 1: compact summary, one line per run.
            let mut matched_count = 0usize;
            let mut rows = String::new();
            for i in 1..=repeat {
                let mr = one_cycle()?;
                if mr.matched {
                    matched_count += 1;
                }
                let raw = raw_text(&mr);
                let summary = if mr.matched && stop_re.is_some() {
                    // Focus to just the matched line (ignore grep for the summary).
                    process_capture(
                        &raw,
                        input.strip_boot_noise,
                        input.strip_ansi,
                        stop_re.as_ref(),
                        true,
                        Some(0),
                        None,
                    )
                } else {
                    let clean = process_capture(
                        &raw,
                        input.strip_boot_noise,
                        input.strip_ansi,
                        None,
                        false,
                        None,
                        grep_re.as_ref(),
                    );
                    last_nonempty_line(&clean)
                };
                let tag = if mr.matched {
                    "match"
                } else {
                    mr.stop_reason.as_str()
                };
                rows.push_str(&format!("{i:>2}. [{tag}] {}\n", truncate_line(&summary, 200)));
            }

            let header = format!(
                "## Rerun \u{00d7}{repeat} (reset + monitor)\n\n\
                 {header}\n\
                 stop matched in {}/{} runs",
                matched_count, repeat
            );
            Ok(format!("{header}\n\n```\n{rows}```"))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }
}

/// The espflash backend needs a serial port.
fn require_port(port: Option<&str>) -> Result<&str, String> {
    port.ok_or_else(|| "backend=espflash requires `port`".to_string())
}

/// The probe-rs backend needs a chip/target name.
#[cfg(feature = "probe-rs")]
fn require_chip(chip: Option<&str>) -> Result<&str, String> {
    chip.ok_or_else(|| "backend=probe-rs requires `chip` (e.g. \"esp32c3\")".to_string())
}
