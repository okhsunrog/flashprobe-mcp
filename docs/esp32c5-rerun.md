# ESP32-C5 empty rerun capture

Verified on 2026-09-07 with probe-rs 0.32 and the existing `canfd-probe`
debug ELF at
`/home/okhsunrog/tmp_zfs/canfd-probe/target/riscv32imac-unknown-none-elf/debug/canfd-probe`.
The firmware and its local esp-hal dependency were not rebuilt or edited.

## Cause

`rerun` set `CaptureOpts::flush` to `true`. Its probe-rs source had already
reset the target, invalidated the previous RTT control block, resumed execution,
and attached to the newly initialized block. `run_capture` then called
`RttSource::flush_input`, draining the **new boot's output** before decoding or
counting any bytes.

Temporary instrumentation of the drain measured exactly **735 discarded bytes**;
the same call subsequently reported **0 raw bytes, 0 frames decoded**. This
explains why a later `monitor` also saw nothing: the flush had consumed the log
and advanced the target's RTT read pointer. The firmware had produced its output.

`flash_monitor` used the same reset-and-attach sequence but set `flush: false`,
so it captured all 735 bytes. The difference was capture policy, not flashing,
reset timing, a partially initialized header, or the RAM-header clearing policy.

## Fix and scope

Only serial rerun captures retain the existing input flush. RTT rerun captures
preserve the bytes buffered since reset, just as `flash_monitor` does. The
reset-and-attach sequence, exact ELF symbol lookup, stale-block invalidation,
temporary blocking channel mode, and mode restoration are unchanged. This also
preserves boot output produced during an optional delay before sending a command.

Hardware validation covers ESP32-C5 only. The fix applies to all probe-rs targets
because flushing after the shared reset-and-attach sequence discards fresh output
on any target; it does not introduce a target-specific reset workaround. The
independent UART mixed plain-text/defmt decoding issue is outside this fix.

## Hardware results

Each command used a freshly launched release binary over MCP stdio, avoiding the
long-lived server's older executable. Captures stopped at `=== done ===`.

| Operation | Result |
| --- | --- |
| Original `rerun`, 5 s timeout | 0 bytes, 0 frames; instrumented flush discarded 735 bytes |
| Fixed `rerun`, `repeat: 3`, 5 s timeout per run | Stop matched in 3/3 runs |
| Fixed single `rerun`, 5 s timeout | 735 bytes, 46 decoded frames |
| `flash_monitor`, 10 s timeout | 735 bytes, 46 decoded frames |
| `reset_device`, then `monitor` with `flush: false`, 5 s timeout | 735 bytes, 46 decoded frames |

All three full captures included the first boot line, `RESULT PASS 19/19`,
`ASYNC PASS 26/26`, and `=== done ===`. Temporary instrumentation was removed.

Repository checks: formatting and Clippy with warnings denied pass for the
CI configurations; all 42 default-feature tests and all 39 serial-only tests
pass. The checks also required gating the RTT timeout helper behind the
`probe-rs` feature and formatting an existing retry match arm.

Reproduce the repeated check using the repository's existing harness:

```sh
cargo build --release --locked
uv run mcp_probe.py rerun '{"backend":"probe-rs","project_dir":"/home/okhsunrog/tmp_zfs/canfd-probe","stop":"=== done ===","timeout_s":5,"repeat":3}'
```

Restart any existing MCP server before testing through its client; rebuilding
does not replace the executable of an already running server.
