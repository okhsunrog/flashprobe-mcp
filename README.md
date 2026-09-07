# flashprobe-mcp

An [MCP](https://modelcontextprotocol.io) server for flashing and monitoring
embedded targets from any MCP client (Claude Code, Claude Desktop, …). It covers
the whole bench from one tool surface, over two backends:

- **probe-rs** — JTAG/SWD flashing + **RTT** capture. Any
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
firmware emits output** — RTT (probe-rs) vs UART (espflash). Picking the wrong
one flashes fine but shows no logs, so the server asks rather than guessing.

- defmt-rtt / rtt-target firmware → `probe-rs`
- esp-println / UART firmware → `espflash`
- any non-ESP chip → `probe-rs`

## Auto-detection

Everything except the backend is derived from the project on disk (no config
file, no state) and can be overridden per call:

| Derived | From | Override |
|---------|------|----------|
| ELF / file to flash | `cargo metadata`, then the **more recently built** of `release/` and `debug/` | `file_path` / `elf`, `project_dir`, `bin` |
| chip (probe-rs) | `.cargo/config.toml` runner `--chip` | `chip` |
| serial port (espflash) | the sole USB serial port | `port` |
| defmt vs text | the ELF's `.defmt` section | — (reliable) |

So from a project directory, `flash_monitor { "backend": "probe-rs", "stop":
"ready" }` flashes the built artifact to the detected chip and decodes defmt —
nothing else to pass.

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
| `monitor` | Attach + capture only | |
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
- **`idle_ms`** — no new data for this long (default `4000`).
- **`timeout_s`** — max wall-clock window (default `5`).
- **`max_bytes`** — byte cap; stops early and marks the output truncated
  (default `65536`). Reads arrive in chunks, so a capture stops just *past* the
  cap: a reported byte count above `max_bytes` is expected, not a miscount.

For probe-rs boot capture, the server temporarily makes the RTT up-channel
blocking to preserve the earliest frames, then restores the firmware's original
channel mode when capture ends.

After a reset the server waits up to `rtt_attach_timeout_ms` (default `1500`)
for the firmware to initialize RTT. An ESP32-C5 booting through the ESP-IDF
bootloader was measured attaching in under 200 ms, so the default has ample
headroom while still reporting a firmware that never brings RTT up. Raise it for
a target whose bootloader runs materially longer.

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
