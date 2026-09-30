//! Capture tools: `monitor` (attach only), `flash_monitor` (flash then capture
//! from boot), and `rerun` (reset + capture, optionally N times). Each resolves
//! a backend (espflash serial or probe-rs RTT), a decode mode (text or defmt,
//! from the ELF), then runs the shared capture pipeline. Grouped into the
//! `capture_router`.

/// How long to wait for the firmware to bring RTT up after a reset.
///
/// An ESP32-C5 booting through the ESP-IDF bootloader was measured attaching in
/// under 200 ms, so the default leaves roughly seven times that as headroom
/// while still reporting a firmware that never initializes RTT quickly. The
/// knob exists for a target whose bootloader takes materially longer.
#[cfg(feature = "probe-rs")]
fn rtt_attach_timeout(ms: Option<u64>) -> std::time::Duration {
    std::time::Duration::from_millis(ms.unwrap_or(1500))
}

use crate::backend::espflash::{SerialSource, connect_to_device, detect_serial_port, flash_file};
use crate::backend::{BackendKind, parse_backend};
use crate::capture::decode::load_defmt_table;
use crate::capture::filter::{compile_opt_regex, process_capture};
use crate::capture::render::{RenderOpts, last_nonempty_line, render_block, truncate_line};
use crate::capture::{
    ByteSource, CaptureOpts, CaptureResult, DecodeMode, DefmtFraming, DefmtStats, Level, capture,
    raw_text,
};
use crate::detect::Detector;
use crate::inputs::*;
use crate::server::Server;
use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    tool, tool_router,
};
use std::time::Duration;

#[cfg(feature = "probe-rs")]
use crate::backend::probers;

#[tool_router(router = capture_router, vis = "pub(crate)")]
impl Server {
    #[tool(
        description = "Read output from a device for a bounded window. An embedded-test ELF runs a fresh suite (resets before each test, RTT log read alongside, stack trace per failed test) after verifying that the flash holds that exact build - a rebuilt ELF that was not flashed is an error, use flash_monitor; ordinary firmware attaches without reset. Backend (REQUIRED): \"probe-rs\" (RTT/semihosting) or \"espflash\" (UART). The ELF auto-detects from the project for defmt decode (structured level/module; or pass `elf`); else plain text. Stops on: regex `stop`, `stop_on_level` (defmt), idle_ms, max timeout, or byte cap. Text mode strips boot noise + ANSI and focuses on the `stop` match. To drive a firmware command interface, set `send` (e.g. \"status\\n\"): it is written to the target after the flush and before reading, so the reply lands in this capture - pair it with `stop` to return the moment the answer arrives."
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
            let stop_on_level = parse_level_opt(input.stop_on_level.as_deref())?;
            // Parsed up front so a bad escape fails before we touch hardware.
            let send = input.send.as_deref().map(parse_escapes).transpose()?;
            let mut det = Detector::new(input.project_dir.as_deref(), input.bin.as_deref());

            // ELF is needed both for defmt decode AND (probe-rs) to pin the RTT
            // control block to `_SEGGER_RTT` — resolve it before the attach.
            let elf = det.elf_opt(input.elf.as_deref());

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
                        let transport = probers::CaptureTransport::select(
                            input.transport.as_deref(),
                            elf.as_deref(),
                        )?;
                        let session = probers::open_session(&chip, input.probe.as_deref())?;
                        (
                            transport.attach(
                                session,
                                probers::AttachOpts {
                                    elf: elf.as_deref(),
                                    rtt_timeout: rtt_attach_timeout(input.rtt_attach_timeout_ms),
                                    reset: false,
                                    verify_flash: true,
                                    trace_failures: input.stacktrace != Some(false),
                                },
                            )?,
                            format!("Probe: {chip} via {}", transport.label()),
                            DefmtFraming::Raw,
                        )
                    }
                };
            let defmt = if source.text_only() {
                None
            } else {
                load_optional_table(elf.as_deref(), input.decode.as_deref())?
            };
            let mode = decode_mode(&defmt, framing);
            let (timeout, idle) = bounds(input.timeout_s, input.idle_ms, source.as_ref());
            let opts = CaptureOpts {
                timeout,
                idle,
                stop: stop_re.clone(),
                stop_on_level,
                flush: input.flush,
                max_bytes: input.max_bytes,
                send,
            };
            send_delay(opts.send.as_ref(), input.send_delay_ms);
            let (result, stats) = capture(source.as_mut(), &mode, &opts)?;
            let trace = stack_trace_section(source.as_mut(), &result, input.stacktrace);

            let header = format!(
                "{header}{}{}",
                send_note(opts.send.as_ref()),
                source_note(source.as_ref())
            );
            let block = render_block(
                &header,
                &result,
                stats,
                &render_opts(
                    // monitor attaches to a running target.
                    false,
                    &input.strip_boot_noise,
                    input.strip_ansi,
                    stop_re.as_ref(),
                    input.context,
                    grep_re.as_ref(),
                    min_level,
                    module_re.as_ref(),
                ),
            );
            Ok(format!("## Serial Monitor Output\n\n{block}{trace}"))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![ContentBlock::text(result)]))
    }

    #[tool(
        description = "Flash firmware, then immediately capture output to verify the boot. Backend (REQUIRED): \"probe-rs\" (flash + RTT/semihosting) or \"espflash\" (flash + UART). File/chip auto-detect from the project; the flashed ELF is the defmt source. Captures from boot. An embedded-test ELF (from `cargo test --no-run`) runs as a suite with probe-rs: each test's RTT/defmt log precedes its `test NAME ... ok/FAILED` line, failed tests get a stack trace, and the call returns on `test result:` with no bounds to set. Stops on: regex `stop`, `stop_on_level` (defmt), idle_ms, max timeout, or byte cap. With probe-rs, output showing `panicked at` ends with a stack trace. `send` writes a command to the target before reading; on a just-flashed device pair it with `send_delay_ms` so the firmware is up first."
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
            let stop_on_level = parse_level_opt(input.stop_on_level.as_deref())?;
            let send = input.send.as_deref().map(parse_escapes).transpose()?;
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
                    let transport = probers::CaptureTransport::select(
                        input.transport.as_deref(),
                        Some(&file_path),
                    )?;
                    let mut session = probers::open_session(&chip, input.probe.as_deref())?;
                    let msg = probers::download(&mut session, &file_path, &chip)?;
                    // Reset + attach the selected transport so capture starts at the run's beginning.
                    let src = transport.attach(
                        session,
                        probers::AttachOpts {
                            elf: Some(&file_path),
                            rtt_timeout: rtt_attach_timeout(input.rtt_attach_timeout_ms),
                            reset: true,
                            // Just flashed from this very file.
                            verify_flash: false,
                            trace_failures: input.stacktrace != Some(false),
                        },
                    )?;
                    (
                        msg,
                        src,
                        format!("Probe: {chip} via {}", transport.label()),
                        DefmtFraming::Raw,
                    )
                }
            };

            // The flashed ELF is the defmt source (unless a raw bin was flashed).
            let elf_path = input
                .elf
                .clone()
                .or_else(|| (input.flash_address.is_none()).then(|| file_path.clone()));
            let defmt = if source.text_only() {
                None
            } else {
                load_optional_table(elf_path.as_deref(), input.decode.as_deref())?
            };
            let mode = decode_mode(&defmt, framing);
            let (timeout, idle) = bounds(input.timeout_s, input.idle_ms, source.as_ref());
            let opts = CaptureOpts {
                timeout,
                idle,
                stop: stop_re.clone(),
                stop_on_level,
                flush: false, // do not flush: we want the boot output
                max_bytes: input.max_bytes,
                send,
            };
            send_delay(opts.send.as_ref(), input.send_delay_ms);
            let (result, stats) = capture(source.as_mut(), &mode, &opts)?;
            let trace = stack_trace_section(source.as_mut(), &result, input.stacktrace);

            let header = format!(
                "{header}{}{}",
                send_note(opts.send.as_ref()),
                source_note(source.as_ref())
            );
            let block = render_block(
                &header,
                &result,
                stats,
                &render_opts(
                    // flash_monitor resets as part of flashing.
                    true,
                    &input.strip_boot_noise,
                    input.strip_ansi,
                    stop_re.as_ref(),
                    input.context,
                    grep_re.as_ref(),
                    min_level,
                    module_re.as_ref(),
                ),
            );
            Ok(format!(
                "## Flash + Monitor\n\n{flash_msg}\n\n### Serial Output\n\n{block}{trace}"
            ))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![ContentBlock::text(result)]))
    }

    #[tool(
        description = "Re-run the firmware already on the device: reset (DTR/RTS for espflash, core reset for probe-rs), then capture the fresh boot. Backend (REQUIRED): \"probe-rs\" (RTT/semihosting) or \"espflash\" (UART). ELF/chip auto-detect from the project for defmt decode. Does not flash: for an embedded-test ELF the flash is verified against the file first, and a rebuilt ELF that was not flashed is an error (use flash_monitor). One call instead of reset + monitor. Set repeat > 1 to run N cycles back-to-back for a compact per-run summary - useful for characterizing flaky/intermittent bugs. `send` writes a command to the target after each reset (re-sent every cycle), so repeat > 1 also characterizes a flaky command response."
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
                Jtag(String, probers::CaptureTransport),
            }

            let elf = det.elf_opt(input.elf.as_deref());
            let (header, framing, conn) = match parse_backend(input.backend.as_deref())? {
                BackendKind::Espflash => {
                    let port = detect_serial_port(input.port.as_deref())?;
                    let header = format!("Port: {port} @ {} baud", input.baud);
                    (header, DefmtFraming::EspPrintln, Conn::Serial(port))
                }
                #[cfg(feature = "probe-rs")]
                BackendKind::ProbeRs => {
                    let chip = det.chip(input.chip.as_deref())?;
                    let transport = probers::CaptureTransport::select(
                        input.transport.as_deref(),
                        elf.as_deref(),
                    )?;
                    let header = format!("Probe: {chip} via {}", transport.label());
                    (header, DefmtFraming::Raw, Conn::Jtag(chip, transport))
                }
            };

            // A semihosting-only source is text whatever the ELF holds; `capture`
            // sees that from the source and ignores the table.
            let defmt = load_optional_table(elf.as_deref(), input.decode.as_deref())?;
            // Parsed once; every cycle re-sends it after its own reset.
            let send = input.send.as_deref().map(parse_escapes).transpose()?;

            // One reset + capture on the selected backend / decode mode, with
            // any stack trace section and source note for it.
            let one_cycle = |stacktrace: Option<bool>| -> Result<Cycle, String> {
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
                    Conn::Jtag(chip, transport) => {
                        let session = probers::open_session(chip, input.probe.as_deref())?;
                        // Reset + attach the selected transport so each cycle captures from the start.
                        transport.attach(
                            session,
                            probers::AttachOpts {
                                elf: elf.as_deref(),
                                rtt_timeout: rtt_attach_timeout(input.rtt_attach_timeout_ms),
                                reset: true,
                                verify_flash: true,
                                trace_failures: stacktrace != Some(false),
                            },
                        )?
                    }
                };
                let (timeout, idle) = bounds(input.timeout_s, input.idle_ms, source.as_ref());
                let opts = CaptureOpts {
                    timeout,
                    idle,
                    stop: stop_re.clone(),
                    stop_on_level,
                    // Never flush here. Every cycle has just reset the target,
                    // so everything buffered is this boot's output — which is
                    // exactly what the caller asked to see. Discarding it loses
                    // the early frames, or the whole log for a target that
                    // prints once and goes quiet.
                    //
                    // This holds for both backends. On RTT the reset-and-attach
                    // already invalidated the old control block; on serial the
                    // port is opened after the reset, so there is no stale
                    // input left to clean up either.
                    flush: false,
                    max_bytes: input.max_bytes,
                    send: send.clone(),
                };
                send_delay(opts.send.as_ref(), input.send_delay_ms);
                let (result, stats) = capture(source.as_mut(), &mode, &opts)?;
                Ok(Cycle {
                    trace: stack_trace_section(source.as_mut(), &result, stacktrace),
                    note: source_note(source.as_ref()),
                    result,
                    stats,
                })
            };

            let header = format!("{header}{}", send_note(send.as_ref()));

            if repeat == 1 {
                let Cycle {
                    result,
                    stats,
                    trace,
                    note,
                } = one_cycle(input.stacktrace)?;
                let block = render_block(
                    &format!("{header}{note}"),
                    &result,
                    stats,
                    &render_opts(
                        // rerun resets every cycle.
                        true,
                        &input.strip_boot_noise,
                        input.strip_ansi,
                        stop_re.as_ref(),
                        input.context,
                        grep_re.as_ref(),
                        min_level,
                        module_re.as_ref(),
                    ),
                );
                return Ok(format!("## Rerun (reset + monitor)\n\n{block}{trace}"));
            }

            // repeat > 1: compact summary, one line per run, so no stack traces.
            let mut matched_count = 0usize;
            let mut rows = String::new();
            for i in 1..=repeat {
                let Cycle {
                    result: mr, stats, ..
                } = one_cycle(Some(false))?;
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
                let tag = if mr.matched {
                    "match"
                } else {
                    mr.stop_reason.as_str()
                };
                rows.push_str(&format!(
                    "{i:>2}. [{tag}] {}\n",
                    truncate_line(&summary, 200)
                ));
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

        Ok(CallToolResult::success(vec![ContentBlock::text(result)]))
    }
}

/// Parse an optional level name (`info`, `error`, …).
fn parse_level_opt(s: Option<&str>) -> Result<Option<Level>, String> {
    s.map(Level::parse).transpose()
}

/// One `rerun` cycle: the capture plus what goes around its rendered block.
struct Cycle {
    result: CaptureResult,
    stats: Option<DefmtStats>,
    trace: String,
    note: String,
}

/// The capture's timeout and idle bound: what the caller set, else the defaults.
///
/// A test suite is the exception. Its runner already bounds every test with
/// the test's own timeout and ends by itself, so an unset timeout becomes the
/// suite's whole budget and an unset idle bound is dropped: the 5 s default
/// would cut a suite off halfway, and a test may stay quiet for most of its
/// timeout without anything being wrong.
fn bounds(
    timeout_s: Option<f64>,
    idle_ms: Option<u64>,
    source: &dyn ByteSource,
) -> (Duration, Duration) {
    let budget = source.run_budget();
    let timeout = timeout_s
        .map(Duration::from_secs_f64)
        .or(budget)
        .unwrap_or_else(|| Duration::from_secs_f64(default_timeout_secs()));
    let idle = idle_ms.map(Duration::from_millis).unwrap_or(match budget {
        Some(_) => Duration::MAX,
        None => Duration::from_millis(default_idle_ms()),
    });
    (timeout, idle)
}

/// Whether the capture shows a Rust panic. Every panic handler prints the
/// standard `panicked at` message, over whichever transport it uses.
fn shows_panic(result: &CaptureResult) -> bool {
    const MARKER: &str = "panicked at";
    result.lines.iter().any(|l| l.text.contains(MARKER)) || result.pending.contains(MARKER)
}

/// A stack trace of the firmware after the capture, rendered as its own
/// section. `stacktrace` is the tool argument: unset traces only a capture
/// that shows a panic, since that is when the frames say where it happened.
fn stack_trace_section(
    source: &mut dyn ByteSource,
    result: &CaptureResult,
    stacktrace: Option<bool>,
) -> String {
    if !stacktrace.unwrap_or_else(|| shows_panic(result)) {
        return String::new();
    }
    match source.stack_trace() {
        None => String::new(),
        Some(Ok(trace)) => format!("\n\nStack trace when the capture ended:\n\n```\n{trace}```"),
        Some(Err(e)) => format!("\n\nNo stack trace: {e}"),
    }
}

/// Header suffix for what the source knows but the output cannot show.
fn source_note(source: &dyn ByteSource) -> String {
    source
        .note()
        .map(|n| format!("\nNote: {n}"))
        .unwrap_or_default()
}

/// Interpret C-style escapes in a `send` payload.
///
/// Firmware command interfaces are overwhelmingly line-based, so the payload
/// has to be able to carry a real newline. A JSON client may send one already
/// (in which case it arrives here as a literal `\n` byte and passes straight
/// through), but an agent writing the argument by hand typically emits the two
/// characters `\` + `n` — this turns those into the byte they mean.
fn parse_escapes(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('0') => out.push(0),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let (Some(hi), Some(lo)) = (chars.next(), chars.next()) else {
                    return Err("`send`: truncated escape, expected `\\xNN`".into());
                };
                let byte = u8::from_str_radix(&format!("{hi}{lo}"), 16)
                    .map_err(|_| format!("`send`: invalid hex escape `\\x{hi}{lo}`"))?;
                out.push(byte);
            }
            Some(other) => {
                return Err(format!(
                    "`send`: unknown escape `\\{other}` (supported: \\n \\r \\t \\0 \\xNN \\\\)"
                ));
            }
            None => return Err("`send`: trailing backslash".into()),
        }
    }
    Ok(out)
}

/// Pause between the source becoming ready and the send, when there is anything
/// to send. Gives a just-reset device time to boot: on serial there is no
/// target-side buffer, so bytes arriving before the firmware's RX is listening
/// are lost outright.
fn send_delay(send: Option<&Vec<u8>>, delay_ms: u64) {
    if send.is_some() && delay_ms > 0 {
        std::thread::sleep(Duration::from_millis(delay_ms));
    }
}

/// Header suffix noting what was sent, so the caller can see it took effect.
fn send_note(send: Option<&Vec<u8>>) -> String {
    send.map(|d| format!("\nSent: {} bytes to target", d.len()))
        .unwrap_or_default()
}

/// Load a defmt table from an optional ELF path (None → text mode).
type DefmtTable = (defmt_decoder::Table, Vec<u8>);
fn load_optional_table(
    elf: Option<&str>,
    decode: Option<&str>,
) -> Result<Option<DefmtTable>, String> {
    match decode.map(|d| d.trim().to_ascii_lowercase()).as_deref() {
        None | Some("auto") | Some("") => match elf {
            Some(path) => load_defmt_table(path),
            None => Ok(None),
        },
        Some("text") => Ok(None),
        Some("defmt") => match elf {
            Some(path) => match load_defmt_table(path)? {
                Some(table) => Ok(Some(table)),
                None => Err(format!(
                    "decode=\"defmt\" but '{path}' has no `.defmt` section"
                )),
            },
            None => Err("decode=\"defmt\" needs an ELF (pass `elf` or build the project)".into()),
        },
        Some(other) => Err(format!(
            "Unknown decode mode '{other}' (use auto, text or defmt)"
        )),
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
    captured_from_reset: bool,
    strip_boot_noise: &bool,
    strip_ansi: bool,
    stop_re: Option<&'a regex::Regex>,
    context: Option<usize>,
    grep: Option<&'a regex::Regex>,
    min_level: Option<Level>,
    module: Option<&'a regex::Regex>,
) -> RenderOpts<'a> {
    RenderOpts {
        captured_from_reset,
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
        if mr.matched
            && let Some(re) = stop_re
        {
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
        process_capture(
            &raw,
            strip_boot_noise,
            strip_ansi,
            stop_re,
            true,
            Some(0),
            None,
        )
    } else {
        let clean = process_capture(&raw, strip_boot_noise, strip_ansi, None, false, None, grep);
        last_nonempty_line(&clean)
    }
}

#[cfg(test)]
mod tests {
    use super::{bounds, parse_escapes, shows_panic};
    use crate::capture::{ByteSource, CaptureResult, Line, StopReason};
    use std::time::Duration;

    struct Source(Option<Duration>);
    impl ByteSource for Source {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
        fn run_budget(&self) -> Option<Duration> {
            self.0
        }
    }

    #[test]
    fn a_suite_is_bounded_by_its_own_budget_unless_told_otherwise() {
        let suite = Source(Some(Duration::from_secs(65)));
        assert_eq!(
            bounds(None, None, &suite),
            (Duration::from_secs(65), Duration::MAX)
        );
        assert_eq!(
            bounds(Some(3.0), Some(500), &suite),
            (Duration::from_secs(3), Duration::from_millis(500))
        );
        assert_eq!(
            bounds(None, None, &Source(None)),
            (Duration::from_secs(5), Duration::from_millis(4000))
        );
    }

    #[test]
    fn a_panic_is_recognized_in_complete_and_pending_lines() {
        let capture = |lines: &[&str], pending: &str| CaptureResult {
            lines: lines.iter().map(|l| Line::text(*l)).collect(),
            pending: pending.into(),
            raw_bytes: 0,
            firmware_bytes: 0,
            stop_reason: StopReason::Idle,
            matched: false,
            truncated: false,
        };
        assert!(shows_panic(&capture(
            &["INFO tick", "ERROR panicked at 'boom'"],
            ""
        )));
        assert!(shows_panic(&capture(&[], "panicked at src/main.rs:3:5")));
        assert!(!shows_panic(&capture(&["INFO tick"], "")));
    }

    #[test]
    fn escapes_decode_to_bytes() {
        assert_eq!(parse_escapes("status\\n").unwrap(), b"status\n");
        assert_eq!(parse_escapes("a\\r\\n").unwrap(), b"a\r\n");
        assert_eq!(parse_escapes("\\t\\0").unwrap(), b"\t\0");
        assert_eq!(parse_escapes("\\x41\\x7f").unwrap(), b"A\x7f");
        assert_eq!(parse_escapes("c:\\\\tmp").unwrap(), b"c:\\tmp");
    }

    /// A client that already decoded JSON escapes sends a real newline; it must
    /// survive untouched rather than being double-processed.
    #[test]
    fn real_newline_passes_through() {
        assert_eq!(parse_escapes("status\n").unwrap(), b"status\n");
    }

    #[test]
    fn non_ascii_is_sent_as_utf8() {
        assert_eq!(parse_escapes("привет").unwrap(), "привет".as_bytes());
    }

    #[test]
    fn malformed_escapes_are_rejected() {
        for bad in ["\\q", "\\", "\\x", "\\x4", "\\xZZ"] {
            assert!(parse_escapes(bad).is_err(), "should reject {bad:?}");
        }
    }
}
