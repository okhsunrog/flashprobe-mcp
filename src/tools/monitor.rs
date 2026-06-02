//! Capture tools: `monitor` (attach only), `flash_monitor` (flash then capture
//! from boot), and `rerun` (reset + capture, optionally N times). All share the
//! capture pipeline over a [`SerialSource`]. Grouped into the `capture_router`.

use crate::backend::espflash::{SerialSource, connect_to_device, flash_file};
use crate::capture::filter::{compile_opt_regex, process_capture};
use crate::capture::render::{last_nonempty_line, render_capture, truncate_line};
use crate::capture::{CaptureOpts, CaptureResult, decode::TextDecoder, raw_text, run_capture};
use crate::inputs::*;
use crate::server::EspflashServer;
use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use std::time::Duration;

#[tool_router(router = capture_router, vis = "pub(crate)")]
impl EspflashServer {
    #[tool(
        description = "Read serial output from an ESP device for a bounded duration. By default the input buffer is flushed first (drops a previous run's tail), ROM/bootloader boot noise and ANSI color codes are stripped, and on a `stop` match the output is focused on the matched line. Stops on: max timeout, regex `stop` match, idle timeout (no new data), or output cap."
    )]
    async fn monitor(
        &self,
        Parameters(input): Parameters<MonitorInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let stop_re = compile_opt_regex(input.stop.as_deref())?;
            let grep_re = compile_opt_regex(input.grep.as_deref())?;

            let mut source = SerialSource::open(&input.port, input.baud)?;
            let mut decoder = TextDecoder::new();
            let opts = CaptureOpts {
                timeout: Duration::from_secs_f64(input.timeout_s),
                idle: Duration::from_millis(input.idle_ms),
                stop: stop_re.clone(),
                flush: input.flush,
                max_bytes: input.max_bytes,
            };
            let result = run_capture(&mut source, &mut decoder, &opts)?;

            let block = render_capture(
                &input.port,
                input.baud,
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
        description = "Flash firmware to an ESP device, then immediately capture serial output to verify the boot. Captures from boot (no flush) so it sees the full startup; ROM/bootloader noise and ANSI codes are stripped by default, and on a `stop` match the output is focused on the matched line. Stops capturing on: max timeout, regex match, idle timeout, or output cap."
    )]
    async fn flash_monitor(
        &self,
        Parameters(input): Parameters<FlashMonitorInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let file_data = std::fs::read(&input.file_path)
                .map_err(|e| format!("Failed to read file '{}': {e}", input.file_path))?;

            // --- Flash phase ---
            let mut flasher = connect_to_device(&input.port, input.flash_baud, true)?;
            let flash_msg = flash_file(
                &mut flasher,
                &file_data,
                input.flash_address,
                input.partition_table.as_deref(),
                input.bootloader.as_deref(),
            )?;

            // Drop flasher to release the serial port (hard reset happens here)
            drop(flasher);
            // Small delay to let the device start booting after reset
            std::thread::sleep(Duration::from_millis(100));

            // --- Monitor phase ---
            let stop_re = compile_opt_regex(input.stop.as_deref())?;
            let grep_re = compile_opt_regex(input.grep.as_deref())?;

            let mut source = SerialSource::open(&input.port, input.monitor_baud)?;
            let mut decoder = TextDecoder::new();
            let opts = CaptureOpts {
                timeout: Duration::from_secs_f64(input.timeout_s),
                idle: Duration::from_millis(input.idle_ms),
                stop: stop_re.clone(),
                flush: false, // do not flush: we want the boot output
                max_bytes: input.max_bytes,
            };
            let result = run_capture(&mut source, &mut decoder, &opts)?;

            let block = render_capture(
                &input.port,
                input.monitor_baud,
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
        description = "Re-run the firmware already on the device: hardware reset (DTR/RTS), flush the buffer, then capture the fresh boot. One call instead of reset_device + monitor; ideal for iterating on the same binary without reflashing. Set repeat > 1 to run N cycles back-to-back and get a compact per-run summary (one matched line per run + a match count) - useful for characterizing flaky/intermittent bugs. ROM/bootloader noise and ANSI codes are stripped by default, and on a `stop` match the output is focused on the matched line."
    )]
    async fn rerun(
        &self,
        Parameters(input): Parameters<RerunInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let stop_re = compile_opt_regex(input.stop.as_deref())?;
            let grep_re = compile_opt_regex(input.grep.as_deref())?;
            let repeat = input.repeat.clamp(1, 50);
            let timeout = Duration::from_secs_f64(input.timeout_s);
            let idle = Duration::from_millis(input.idle_ms);

            // One reset (no stub, matches reset_device / espflash CLI) + flush + capture.
            let one_cycle = || -> Result<CaptureResult, String> {
                let mut flasher = connect_to_device(&input.port, 115_200, false)?;
                flasher
                    .connection()
                    .reset()
                    .map_err(|e| format!("Failed to reset device: {e}"))?;
                drop(flasher);
                // Let the ROM bootloader start before we open the monitor.
                std::thread::sleep(Duration::from_millis(100));

                let mut source = SerialSource::open(&input.port, input.baud)?;
                let mut decoder = TextDecoder::new();
                let opts = CaptureOpts {
                    timeout,
                    idle,
                    stop: stop_re.clone(),
                    flush: true, // flush: start each capture clean
                    max_bytes: input.max_bytes,
                };
                run_capture(&mut source, &mut decoder, &opts)
            };

            if repeat == 1 {
                let result = one_cycle()?;
                let block = render_capture(
                    &input.port,
                    input.baud,
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
                 Port: {} @ {} baud\n\
                 stop matched in {}/{} runs",
                input.port, input.baud, matched_count, repeat
            );
            Ok(format!("{header}\n\n```\n{rows}```"))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }
}
