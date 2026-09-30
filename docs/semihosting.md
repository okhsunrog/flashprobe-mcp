# Semihosting capture

The probe-rs backend runs an ELF with an `.embedded_test` section over
semihosting. Any other ELF gets RTT when it defines `_SEGGER_RTT`, or
semihosting otherwise. `transport: "rtt"` / `"semihosting"` override selection,
except that `"rtt"` is refused for an embedded-test ELF; without an ELF, auto
retains the previous RTT RAM scan. See "RTT alongside semihosting" below for
why the section is checked first.

`SemihostingSource` implements the same `ByteSource` interface as RTT and serial.
It services core 0 using probe-rs' decoded semihosting requests and target-buffer
APIs, and feeds console bytes through the existing text capture/filter/render
pipeline. Finite sources can signal completion after draining their output.
Reset-owned captures still use `flush: false`; semihosting itself has no input
ring buffer to flush. It never resumes an exit trap.

The `test NAME ... ok` and `test result:` lines are host-generated: embedded-test
requires a runner, not just a console reader. Protocol 1 ELF metadata supplies
test addresses, names, panic expectations, ignored flags and timeouts. The
source supplies `run_addr ADDRESS` on SYS_GET_CMDLINE, collects the exit result,
and resets before the next case. A matched summary means the suite completed,
not that all tests passed. Earlier/future protocol versions are explicitly
rejected. No per-test structured MCP result format has been added.

For an embedded-test ELF, `monitor` starts a fresh suite, including an initial
reset. This is an explicit exception to ordinary attach-only monitoring:
resetting and then releasing the ESP debug session can turn the first
semihosting breakpoint into a firmware exception before the monitor attaches.
Ordinary semihosting firmware keeps attach-only behavior.

## Hardware evidence, 2026-09-07

ESP32-C5, ESP JTAG `303a:1001:3C:DC:75:8E:15:98`, GPIO9–GPIO10 jumper.
For the resumed final checks, the user added a 3 kΩ pull-up to 3.3 V on
the joined line. Earlier flash/repeat and reference results predate that change.
Neither firmware project was changed or rebuilt. The esp-hal checkout remained
on `canfd-esp32c5`. Reference probe-rs checkout: `c7903cb1`.

Semihosting ELF:
`/home/okhsunrog/code/rust/esp-hal/target/riscv32imac-unknown-none-elf/release/misc_drivers`

SHA-256: `f58dec7ddffc4ae91764a52df809d15a7f919d654f251ce69e38c22aa06152e0`.

Each MCP check launched a fresh release executable over stdio via
`uv run mcp_probe.py`, rather than using an already-running server.

| Check | Observed result |
| --- | --- |
| Reference `probe-rs run --chip=esp32c5 --preverify --no-location ELF` | Complete suite: 27 passed, 2 failed; failures are in the CAN FD tests under investigation |
| Initial MCP `flash_monitor`, auto transport, `stop: "test result:"`, `context: 100` | Header `via semihosting`, 1796 bytes, all 29 test lines, 29 passed; stop pattern matched |
| Initial MCP `rerun`, auto transport, `repeat: 3`, same stop | 3/3 summary matches, each 29 passed and 0 failed |
| Original attach-only MCP `monitor` after `reset_device` | Captured a firmware breakpoint-exception panic, demonstrating why embedded-test needs an initial reset |
| Updated embedded-test MCP `monitor`, forced semihosting | 1796 bytes, all 29 test lines, 29 passed; stop pattern matched |
| RTT `flash_monitor` and reset + `monitor` (`flush: false`) | Auto selected RTT, 738 bytes / 46 defmt frames each, `RESULT PASS 19/19`, `ASYNC PASS 27/27`, stop matched |
| RTT `rerun`, `repeat: 3` | `=== done ===` matched in 3/3 runs |

Final release build, after reconnect and pull-up addition:

| Check | Observed result |
| --- | --- |
| `flash_monitor`, auto, `stop: "test result:"`, `context: 100` | 29/29 passed, 1796 bytes, stop matched; 13.78 s including flash and server startup |
| `rerun`, auto, `repeat: 3`, same stop | 3/3 complete summaries, each 29/29 passed; 35.10 s total |
| `rerun` without `stop`, `grep: "test result:"` | `source completed`, 29/29 passed, 1796 raw bytes / 47 shown; 12.27 s including startup |

The final monitor result and RTT regression rows above also use this final
implementation. The timeout was 200 s per semihosting capture and idle timeout
65 s; the observed calls returned on the summary/completion, not those limits.
The original `misc_drivers` ELF was restored to the board after the RTT checks.
Both ELF hashes remained unchanged. These results establish output transport,
not a conclusion about the CAN FD driver or the electrical change.
ARM, other chips, multicore capture, and UART hardware were not tested in this
session. The serial path is covered by the existing tests and no-default-feature
build. File access, stdin/`send`, and embedded-test protocol 0 are unsupported.

Local validation of the final source: 54 default-feature tests and 47
espflash-only tests passed. Clippy (`--all-targets --locked -- -D warnings`)
passed in both configurations, as did `cargo check --no-default-features`,
formatting, diff whitespace checks and the release build.

## Reproduce

```sh
cargo build --release --locked
uv run mcp_probe.py flash_monitor '{"backend":"probe-rs","chip":"esp32c5","file_path":"/home/okhsunrog/code/rust/esp-hal/target/riscv32imac-unknown-none-elf/release/misc_drivers","timeout_s":200,"idle_ms":65000,"stop":"test result:","context":100}'
uv run mcp_probe.py rerun '{"backend":"probe-rs","chip":"esp32c5","elf":"/home/okhsunrog/code/rust/esp-hal/target/riscv32imac-unknown-none-elf/release/misc_drivers","timeout_s":200,"idle_ms":65000,"stop":"test result:","repeat":3}'
uv run mcp_probe.py monitor '{"backend":"probe-rs","chip":"esp32c5","elf":"/home/okhsunrog/code/rust/esp-hal/target/riscv32imac-unknown-none-elf/release/misc_drivers","timeout_s":200,"idle_ms":65000,"stop":"test result:","context":100,"transport":"semihosting"}'
```

RTT regression ELF:
`/home/okhsunrog/tmp_zfs/canfd-probe/target/riscv32imac-unknown-none-elf/debug/canfd-probe`

SHA-256: `a4a3f25c21288764447e4c5597bfc5fc7fe2b2be9f5026f2118bb63e65ca7f00`.
For another regression check, flash it with `flash_monitor`, then use `rerun` with `repeat: 3`, and
`reset_device` followed by `monitor` with `flush: false`; use `stop: "=== done ==="`
and check for `RESULT PASS 19/19` and `ASYNC PASS 27/27`. Restore the semihosting
ELF with `flash_monitor` after checking RTT.

## Flash verification before a test run

`rerun` and `monitor` do not flash. For an embedded-test ELF they now verify
the flash against the file before the first reset, with the same read-back
comparison `probe-rs run --preverify` uses, and refuse to run on a mismatch.
`flash_monitor` skips the check because it has just written that image.

The runner selects each test by address (`run_addr ADDRESS`) taken from the
ELF on disk. When the flash holds an older build, those addresses land in the
wrong functions and the run fails with random exceptions that look like
firmware bugs. Reproduced on ESP32-C5 on 2026-09-08 by rebuilding
`hil-test/src/bin/canfd.rs` with one extra test and calling `rerun` without
flashing: `Load access fault` and `Illegal instruction` traps, tests reported
under the wrong names, and one trap inside ROM at `0x40038504`. The same ELF
after `probe-rs run --preverify` passed 40/40. The earlier "MCP rerun gives
load access faults" observation on this driver had the same cause: a rebuild
followed by `rerun` instead of `flash_monitor`.

Verified on the same board on 2026-09-09 with a fresh release binary over
`mcp_probe.py`, ELF `hil-test` `canfd` from esp-hal `4032a055f`:

| Check | Observed result |
| --- | --- |
| `rerun`, flash matches the ELF | Verification passed, 40/40, 14.6 s including server start |
| `rerun`, ELF rebuilt with one extra test, flash not updated | Refused in 1.1 s with the mismatch error; nothing ran on the target |
| `flash_monitor` with that rebuilt ELF | Flashed and ran 41/41 in 14.8 s |

The committed firmware was restored with `probe-rs run --preverify` afterwards
(40/40).

## RTT alongside semihosting

An embedded-test binary usually logs over defmt-rtt or rtt-target, which its
`#[init]` sets up. The ELF therefore defines `_SEGGER_RTT`, and auto used to
pick RTT for it on that symbol alone. That can never work: embedded-test stops
at a semihosting `SYS_GET_CMDLINE` before `#[init]` and waits for a runner, so
`flash_monitor` reported "RTT control block did not appear" and `monitor`
"Failed to attach RTT". Forcing semihosting ran the suite, but showed only the
runner's verdicts and none of the defmt log that explains a failure.

Auto now checks the `.embedded_test` section first. The semihosting source
reads RTT up-channel 0 in the same loop that services semihosting, the way
`probe-rs run` combines its RTT poller with the test runner:

- Before each test's reset, the RTT control block is zeroed, so the attach can
  only find the block that test's `#[init]` writes. Attach is retried on every
  poll at the ELF's `_SEGGER_RTT` address.
- Each poll reads the core status first and drains RTT second. A core seen
  halted at an exit trap has already written everything it logged, so a test's
  log is queued ahead of its verdict.
- RTT bytes and runner text travel as separate tagged streams. RTT goes through
  the defmt or text decoder; runner lines go through their own text decoder and
  survive the `level` and `module` filters.
- The up-channel is switched to `BlockIfFull` while the host reads, and the
  firmware's mode is restored when the capture ends.

A failed test, including a timeout, gets a stack trace before its `FAILED`
line, unwound with `probe-rs-debug` from the ELF the way `probe-rs run` does it.
A suite also sets its own capture bounds: an unset `timeout_s` becomes the sum
of the remaining per-test timeouts plus a second each and 5 s of slack, and an
unset `idle_ms` does not apply.

When RTT still does not come up in an RTT capture, the error now reads the
core's state: a core halted at `SYS_GET_CMDLINE` is reported as an
embedded-test binary waiting for its runner, with the transport to use.

### Hardware evidence, 2026-09-30

ESP32-C5, ESP JTAG `303a:1001:3C:DC:75:8E:15:98` (a second probe was connected,
so every call passed `probe`). probe-rs CLI 0.32.0 for the reference runs. Each
MCP check launched a fresh release executable over stdio via
`uv run mcp_probe.py`.

Suite under investigation: `canfd-ergot-testing` `tests/ergot_can.rs`
(embedded-test 0.7.2, esp-rtos executor, defmt over rtt-target initialized in
`#[init]`), ELF SHA-256
`2c505d2a0187160bd5f0a0288d4342e17c44bbb43bcef0af8e5060e5e17d3d12`.

| Check | Before | After |
| --- | --- | --- |
| `flash_monitor`, only backend/chip/probe/file | RTT control block timeout after 1.5 s | `via semihosting + RTT`, 2/2 ok, defmt log before each verdict, returned on completion in 10.7 s |
| `flash`, then `monitor` auto | RTT attach timeout | Same suite output as above |
| `monitor`, `transport: "semihosting"` | Verdicts only, no defmt | Same suite output as above |
| `monitor`, `transport: "rtt"` | RTT attach timeout | Refused up front, naming the embedded-test cause |
| `rerun` with a plain-app ELF while the test ELF is flashed | Not run; same code path as the first row, whose error lists generic RTT causes | "halted at a semihosting SYS_GET_CMDLINE request ... embedded-test" |

Panic fixture (built in a scratch project against the same esp-hal revision):
a suite with a passing test, a `defmt::panic!` from a nested call, a
`should_panic` test, an `Err` return, and a `#[timeout(2)]` test that never
finishes; and a plain `panic-rtt-target` app that panics after four ticks.

| Check | Observed result |
| --- | --- |
| Suite, `probe-rs run` vs `flash_monitor` | Same defmt lines and verdicts (2 passed, 3 failed); all stack trace frame and location lines identical for the panic, the `Err` return and the timeout |
| Suite, `rerun` after the flash | Same output; flash verification passed |
| Plain app, `probe-rs run --always-print-stacktrace` + Ctrl+C vs `flash_monitor` | Same panic message lines; the automatic trace's 20 frames and locations identical |
| Plain RTT app without a panic (`canfd-ergot-testing` bin) | No trace section, output as before |
| Semihosting-only suite (`misc_drivers`, no RTT) | `via semihosting`, text mode, 13/13 |

The suite binary under investigation was left flashed on the board.

## Short stack traces and probe selection, 2026-09-30

Traces are now short by default: the project's frames in full, each run of
dependency frames (cargo registry, git checkouts, the standard library) folded
into one line naming its crates, the top run also listing its functions, and
generic arguments trimmed to `<…>`. `stacktrace_full: true` restores the
`probe-rs run` output. With several probes and no `probe`, the probe whose JTAG
IDCODE names `chip` is used; otherwise the error lists each probe with its chip.

Checked on the same bench, now with an ESP32-C6 (`303a:1001:58:E6:C5:17:35:7C`)
connected next to the ESP32-C5:

| Check | Observed result |
| --- | --- |
| `chip_info`, `chip: "esp32c5"`, no `probe` | ESP32-C5 selected, 0.45 s including server start |
| `chip_info`, `chip: "esp32c6"`, no `probe` | ESP32-C6 selected |
| `chip_info`, `chip: "esp32h2"`, no `probe` | Error listing `…15:98: esp32c5` and `…35:7C: esp32c6` |
| Panic fixture suite, `flash_monitor` without `probe` | Same verdicts; output 3.8 KB instead of 10.7 KB; every project frame kept, top runs `abort ← panic_fmt …` and `wait_for_interrupt ← idle_hook` |
| Same suite, `rerun` with `stacktrace_full: true` | All 51 frame and location lines identical to `probe-rs run` |
| Plain `panic-rtt-target` app | Short trace: `check_value` and the calling `async fn` with their lines, 20 frames folded to 7 lines |

The `ergot_can` suite was flashed back afterwards (2/2).
