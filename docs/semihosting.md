# Semihosting capture

The probe-rs backend now selects RTT for an ELF defining `_SEGGER_RTT`, or
semihosting otherwise. `transport: "rtt"` / `"semihosting"` override selection;
without an ELF, auto retains the previous RTT RAM scan.

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
