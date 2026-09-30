# flashprobe-mcp

An [MCP](https://modelcontextprotocol.io) server for flashing and monitoring
embedded targets from any MCP client (Claude Code, Claude Desktop, …). It covers
the whole bench from one tool surface, over two backends:

- **probe-rs** — JTAG/SWD flashing + **RTT or semihosting** capture. Any
  [probe-rs-supported](https://probe.rs/targets/) chip: ESP (Xtensa + RISC-V),
  STM32, nRF, RP2040/RP2350, …
- **espflash** — UART flashing + serial capture for ESP32-family chips.

Output is decoded as **defmt** when the firmware uses it (`defmt-rtt`,
`rtt-target`, or `esp-println`'s `defmt-espflash`) and plain text otherwise —
detected automatically from the ELF.

## Why a server instead of the CLI

The win for an agent is **bounded, early-exiting capture**. `probe-rs run` /
`espflash monitor` never terminate on their own — an agent either forgets a
timeout and hangs, or sets one and burns the whole window every time. This
server stops the instant an expected line (or defmt error) appears — the
programmatic equivalent of watching the log and pressing Ctrl-C — and returns a
compact, cleaned result.

## Build

```sh
cargo build --release
# binary at: target/release/flashprobe-mcp
```

probe-rs is a default-on feature. For a lean espflash-only (serial) build with a
much smaller dependency tree:

```sh
cargo build --release --no-default-features
```

The server speaks MCP over stdio.

## Configure your MCP client

```sh
claude mcp add flashprobe -- /absolute/path/to/target/release/flashprobe-mcp
```

Or generic `mcpServers` JSON:

```json
{
  "mcpServers": {
    "flashprobe": {
      "command": "/absolute/path/to/target/release/flashprobe-mcp"
    }
  }
}
```

Serial access needs your user in the `dialout`/`uucp` group; probe access needs
the [probe-rs udev rules](https://probe.rs/docs/getting-started/probe-setup/).

## Choosing a backend (required)

Every flash/monitor call takes an explicit **`backend`**: `"probe-rs"` or
`"espflash"`. Both work on ESP chips, and the right one depends on **where the
firmware emits output** — RTT/semihosting (probe-rs) vs UART (espflash). Picking the wrong
one flashes fine but shows no logs, so the server asks rather than guessing.

- defmt-rtt / rtt-target firmware → `probe-rs`
- semihosting / embedded-test firmware → `probe-rs`
- esp-println / UART firmware → `espflash`
- any non-ESP chip → `probe-rs`

## Auto-detection

Everything except the backend is derived from the project on disk (no config
file, no state) and can be overridden per call:

| Derived | From | Override |
|---------|------|----------|
| ELF / file to flash | `cargo metadata`, then the **more recently built** of `release/` and `debug/` | `file_path` / `elf`, `project_dir`, `bin` |
| chip (probe-rs) | `.cargo/config.toml` runner `--chip` | `chip` |
| debug probe (probe-rs) | the sole probe, or the only one whose chip is `chip` | `probe` |
| serial port (espflash) | the sole USB serial port | `port` |
| defmt vs text | the ELF's `.defmt` section | — (reliable) |

With several probes connected and no `probe` given, each probe's chip is read
from the JTAG IDCODEs of its TAPs, and the one probe identified as `chip` is
used. That tells ESP boards apart, since each brings its own USB-JTAG probe and
the RISC-V ESPs have an IDCODE each. No debug session is opened on the other
boards: attaching one runs the chip's connect sequence, which on an ESP disables
its watchdogs, and reading a magic value halts the core. So the Xtensa ESPs,
which share an IDCODE, come out as "one of esp32, esp32s2, esp32s3", and a
probe that is not JTAG (an ARM target over SWD) as unidentified. When no single
probe matches, the error lists every probe with its chip:

```
2 probes are connected and none of them was identified as esp32h2; pass `probe` as VID:PID:SERIAL to choose one:
- 303a:1001:3C:DC:75:8E:15:98 (ESP JTAG): esp32c5
- 303a:1001:58:E6:C5:17:35:7C (ESP JTAG): esp32c6
```

So from a project directory, `flash_monitor { "backend": "probe-rs", "stop":
"ready" }` flashes the built artifact to the detected chip and decodes defmt —
nothing else to pass.

Artifact detection uses the `target_directory` reported by `cargo metadata`.
If a build wrapper overrides `CARGO_TARGET_DIR` only for the build, that directory
may differ from what metadata reports when the MCP server runs. Pass the actual
artifact path explicitly: `file_path` for `flash` / `flash_monitor`, or `elf` for
`monitor` / `rerun`. For example, esp-hal's `hil-test` is built by xtask into the
repository-root `target/`, although metadata from `hil-test/` reports its own
`target/`. For that workflow, pass the ELF path and `chip` explicitly.

Release is the usual thing to flash, but embedded projects routinely iterate on
an optimized debug build — the `esp-generate` template sets `opt-level = "s"` for
the dev profile precisely so it fits and runs. Picking whichever was built last
covers both, and fails safe: after a release build followed by an edit and a
plain `cargo build`, the debug binary is the one you meant, where preferring
release would have silently flashed the stale image.

## Tools

All flash/monitor/device tools work on **both backends** (each using its native
mechanism); only `list_ports` is serial-specific.

| Tool | Purpose | Backend notes |
|------|---------|---------------|
| `flash` | Flash an ELF/binary (no monitor) | espflash: IDF format / raw `flash_address`; probe-rs: flash-algo |
| `flash_monitor` | Flash, then capture from boot | |
| `rerun` | Reset (no reflash) + capture; `repeat > 1` for flaky-bug runs | |
| `monitor` | Attach + capture; embedded-test ELF starts a fresh test suite | |
| `reset_device` | Reset the device | espflash: DTR/RTS; probe-rs: core reset |
| `erase_flash` / `erase_region` | Erase flash (destructive) | espflash: ROM erase (4 KiB-aligned region); probe-rs: flash-algo (sector-covering) |
| `read_flash` | Read a memory/flash region to a file | espflash: ROM read; probe-rs: debug-port memory read |
| `chip_info` | Device/target info | espflash: ESP type/revision/MAC/crystal/flash; probe-rs: target name + cores + memory map |
| `checksum_md5` | MD5 of a region | espflash: on-device ROM MD5; probe-rs: read + host-side hash |
| `list_ports` | Discover serial ports | espflash/serial only |

## Capture

`flash_monitor`, `monitor`, and `rerun` capture until the first of:

- **`stop`** — an unanchored regex on the rendered line (`RESULT (PASS|FAIL)`,
  `panic|abort`). Plain text is a valid pattern.
- **`stop_on_level`** — defmt only: stop on the first frame at/above a level
  (e.g. `error`) — the "did it panic?" button.
- **`idle_ms`** — no new data for this long (default `4000`; not applied to an
  embedded-test suite unless set).
- **`timeout_s`** — max wall-clock window (default `5`; an embedded-test suite
  defaults to the sum of its per-test timeouts).
- **`max_bytes`** — byte cap; stops early and marks the output truncated
  (default `65536`). Reads arrive in chunks, so a capture stops just *past* the
  cap: a reported byte count above `max_bytes` is expected, not a miscount.

For probe-rs captures, `transport` accepts `"auto"` (default), `"rtt"`, or
`"semihosting"`. Auto runs an ELF with an `.embedded_test` section as a test
suite over semihosting; for any other ELF it uses RTT when the ELF defines
`_SEGGER_RTT`, otherwise semihosting. Without an ELF, auto retains RTT RAM
scanning; pass an ELF or force semihosting for a console-only application. The
output header reports `via RTT`, `via semihosting`, or `via semihosting + RTT`.
An unreadable/invalid ELF is an error, not an auto-detection fallback.
`rtt_attach_timeout_ms` applies only to RTT. Forcing `"rtt"` on an
embedded-test ELF is refused: that firmware stops at a semihosting request
before `#[init]` and waits for a runner, so RTT would never come up.

Semihosting uses probe-rs library requests for console writes, stdout/stderr,
command line, time, errno and exit. When the ELF also defines `_SEGGER_RTT`,
the firmware's RTT up-channel 0 is read in the same loop, as `probe-rs run`
does: RTT goes through the normal defmt/text decoder, semihosting console output
and runner lines are plain text, and the two are kept in the order they
happened. Without RTT, console data uses text decoding even if the ELF contains
a defmt table. `stop`, `grep`, `context`, idle/timeout/byte limits, ANSI/noise
filtering and empty/truncated reporting share the normal capture pipeline.
`level` and `module` filter only the firmware's defmt log and keep runner lines.
Target exit ends capture without waiting for idle. File access and stdin/`send`
are unsupported. A semihosting flush is a no-op: servicing a pending syscall
would execute firmware and consume fresh output.

An ELF with embedded-test protocol 1 metadata (embedded-test >= 0.7) runs as a
test suite: the host supplies each test address, resets between tests, honours
ignored tests, expected panics and per-test timeouts, and emits `running N
tests`, `test NAME ... ok` or `test NAME ... FAILED (reason)`, and `test result:
...`. Each test's RTT log appears before its verdict, and a failed test gets a
stack trace before its `FAILED` line (see below). Earlier/future protocol
versions produce an explicit error. For embedded-test only, `monitor` also
starts a fresh suite, resetting before the first test as well as between tests.
A detached ESP may already have treated the semihosting trap as an exception,
so attach-only cannot reliably recover its test command. Ordinary semihosting
applications still attach without reset. `rerun` resets and runs the entire
suite per repeat. A failed test remains visible in the text; matching `test
result:` means completion, not test success.

A suite needs no capture bounds: with `timeout_s` unset it may run for the sum
of its per-test timeouts, and with `idle_ms` unset it does not stop on silence,
since a test may stay quiet for most of its timeout. The call returns when the
suite finishes:

```json
{"backend":"probe-rs","chip":"esp32c5","file_path":"/path/to/test-elf"}
```

Semihosting polling services core 0; multicore console capture is not
implemented. Stopping a capture stops host servicing, so firmware can block at
its next semihosting request until another monitor attaches.

### Stack traces

Stack traces are unwound with the ELF's debug info. By default:

- each failed embedded-test case, including a timeout, gets a trace of where it
  stopped, placed before its `FAILED` line, as `probe-rs run` prints it;
- any other probe-rs capture whose output shows `panicked at` gets a trace of
  the firmware when the capture ended, in its own section after the output.
  Panic handlers such as `panic-rtt-target` spin in place after printing, so
  this shows where the panic came from. `probe-rs run` prints this one only with
  `--always-print-stacktrace`, on Ctrl+C.

`stacktrace: true` also traces a capture that shows no panic (the core is
halted briefly and resumed), and `stacktrace: false` turns traces off. `rerun`
with `repeat > 1` never traces.

The trace is short by default. Most frames of an embedded Rust stack are the
panic machinery, the executor and the startup code, and their names carry
whole future types, so a full trace of one failed async test runs to kilobytes.
The short form prints the project's own frames (anything outside the cargo
registry, git checkouts and the standard library, so path dependencies count
as the project), folds each run of dependency frames into one line naming its
crates, and trims generic arguments to `<…>`. The run at the top of the stack
also lists its functions, since they say what the core was doing:

```
Core 0 (27 frames; dependency frames folded, generics trimmed; stacktrace_full: true shows them all)
    Frames 0-9 (semihosting, embedded-test, core, defmt): syscall_readonly ← sys_exit_extended ← exit ← exit ← abort ← panic ← panic_fmt ← panic ← default_panic ← panic
    Frame 10: check_value @ 0x420030e4
        /path/to/project/tests/panic_suite.rs:11:9
    Frame 11: {async_fn#0} @ 0x4200301a
        /path/to/project/tests/panic_suite.rs:37:9
    Frames 13-19 (embassy-executor, esp-rtos)
    Frame 20: __b_panics_entrypoint @ 0x420032e2
        /path/to/project/tests/panic_suite.rs:15:1
    Frames 21-26 (embedded-test, esp-hal, riscv-rt, no source)
```

`stacktrace_full: true` prints every frame with its full name, exactly as
`probe-rs run` does.

The trace shows where a panic happened; the message is whatever the firmware
logged. embedded-test's panic handler logs the `PanicInfo` through defmt, whose
`Format` for it prints only the location, so the text of an `expect("...")`
never leaves the target, under `probe-rs run` as well. To see it, panic through
defmt (`defmt::unwrap!(x, "...")`, `defmt::panic!`), or turn off embedded-test's
`panic-handler` feature and log `defmt::Display2Format(info)` from your own
handler before `semihosting::process::abort()`.

Hardware evidence and reproducible MCP stdio commands are in
[docs/semihosting.md](docs/semihosting.md).

For probe-rs RTT boot capture, the server temporarily makes the RTT up-channel
blocking to preserve the earliest frames, then restores the firmware's original
channel mode when capture ends.

After a reset the server waits up to `rtt_attach_timeout_ms` (default `1500`)
for the firmware to initialize RTT. An ESP32-C5 booting through the ESP-IDF
bootloader was measured attaching in under 200 ms, so the default has ample
headroom while still reporting a firmware that never brings RTT up. Raise it for
a target whose bootloader runs materially longer.

When RTT does not come up, the error reports the core's state if that explains
it, rather than a list of possible causes. Where the core stopped only means
something for the firmware the ELF describes, so when the capture did not just
flash that ELF (`monitor`, `rerun`) and the core has stopped, the flash is first
compared with the ELF. A mismatch is reported as such: the device runs a
different build, and the fix is to flash this ELF or pass the right one. With a
matching flash, a core halted at a semihosting `SYS_GET_CMDLINE` is firmware
that asks the host for its command line (with no ELF given, most likely an
embedded-test binary waiting for its runner), a core halted at a semihosting
exit already finished, and a core halted for another reason stopped before RTT
was initialized. The comparison halts the core, so it is skipped while the
core is still running.

**Show filters:** `grep` (regex, both modes), `context` (N lines around the
`stop` match), and defmt-only `level` (minimum to show) / `module` (regex on the
module path). In defmt mode a suppressed-by-level count reports what a looser
`level` would reveal. In text mode, ROM/bootloader boot noise (`strip_boot_noise`)
and ANSI codes (`strip_ansi`) are stripped by default.

## When a capture comes back empty

An empty capture has three distinct causes, and the output names which one it
was rather than leaving you to guess:

- **`(nothing was received)`** — no bytes arrived at all. Loosening the filters
  cannot help. What follows depends on whether the capture reset the target:
  - After `monitor`, the usual cause is a target that prints at boot and then
    idles. `monitor` only sees what is sent *after* it attaches; use `rerun` or
    `flash_monitor`, which reset first and capture from the start.
  - After `rerun` or `flash_monitor`, the target was reset and still said
    nothing. Either it is not running, or it logs where this backend is not
    listening — RTT firmware produces nothing on the serial backend, and vice
    versa. Check `backend` first; it is the most common mistake.
- **`(bytes arrived, but every line was removed by the filters)`** — the data is
  there. Relax `level`, `module` or `grep`. In defmt mode the header also
  reports how many frames a looser `level` would reveal.
- **`(no application output — only boot/ROM noise was captured)`** — text mode
  only: bytes arrived but `strip_boot_noise` removed all of them. Set it to
  `false` to see the raw stream.

A capture that ran *after* another capture already drained the buffer is not a
failure either: `rerun` consumes the output it captures, so a `monitor` right
afterwards has genuinely nothing left to show for firmware that prints once.

## Known target quirks

- **ESP32-C5 over USB-Serial-JTAG.** espflash's DTR/RTS reset leaves this board
  in download mode rather than booting the application, so `rerun` and
  `flash_monitor` on the `espflash` backend capture nothing from it — the
  application never starts. Reset or flash through `probe-rs` to boot it
  normally. The serial backend still reads fine; it is the reset that differs.

## Sending to the target

The same three tools take `send`, a string written to the device *before* the
read loop, so a command and its reply fit in one call:

```jsonc
{ "backend": "probe-rs", "send": "status\n", "stop": "OK|ERR" }
```

It goes out on the RTT **down-channel 0** (probe-rs) or the serial **TX line**
(espflash). The escapes `\n` `\r` `\t` `\0` `\xNN` `\\` are interpreted —
line-based firmware almost always needs the trailing `\n`. Ordering is fixed at
flush → send → read, so the flush can never eat the reply. Pair it with `stop`
to return the instant the answer arrives.

`send_delay_ms` (default `0`) waits before sending. Serial has no target-side
buffer, so bytes that arrive before the firmware's RX is listening are lost —
give a just-reset or just-flashed device time to boot. RTT needs this less: the
bytes sit in the target's ring buffer until the firmware reads them.

> **probe-rs needs a firmware down-channel.** `defmt-rtt` declares
> `max_down_channels = 0`, so it can never receive host input and `send` reports
> that. Use `rtt-target` with an explicit down-channel; defmt still works there
> through `set_defmt_channel`:
>
> ```rust
> let channels = rtt_init! {
>     up:   { 0: { size: 1024, mode: NoBlockSkip, name: "defmt" } },
>     down: { 0: { size: 64, name: "input" } }
> };
> set_defmt_channel(channels.up.0);
> ```

## defmt note

defmt decode needs the **exact ELF that's running** — version skew yields
garbage, not an error. It's free in the flash-then-monitor flow (just built it);
for bare `monitor`/`rerun`, make sure the auto-detected (or passed) ELF matches.
The server surfaces a warning when a non-empty stream decodes to zero frames.

On the serial backend, defmt frames are marker-delimited (`0xFF 0x00`) precisely
so they can share the line with plain text, and both are shown. That matters
because the ROM and the ESP-IDF bootloader print text long before the
application's first frame: a target that dies in the bootloader still tells you
why, instead of looking like a target that said nothing.

## License

MIT — see [LICENSE](LICENSE).
