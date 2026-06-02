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
| ELF / file to flash | `cargo metadata` build artifact | `file_path` / `elf`, `project_dir`, `bin` |
| chip (probe-rs) | `.cargo/config.toml` runner `--chip` | `chip` |
| serial port (espflash) | the sole USB serial port | `port` |
| defmt vs text | the ELF's `.defmt` section | — (reliable) |

So from a project directory, `flash_monitor { "backend": "probe-rs", "stop":
"ready" }` flashes the built artifact to the detected chip and decodes defmt —
nothing else to pass.

## Tools

| Tool | Purpose |
|------|---------|
| `flash` | Flash an ELF/binary (no monitor) |
| `flash_monitor` | Flash, then capture from boot |
| `rerun` | Reset (no reflash) + capture; `repeat > 1` for flaky-bug runs |
| `monitor` | Attach + capture only |
| `chip_info` | Chip type, revision, MAC, flash size (espflash) |
| `reset_device` | Reset via DTR/RTS (espflash) |
| `erase_flash` / `erase_region` | Erase flash (destructive, espflash) |
| `read_flash` / `checksum_md5` | Read / checksum a flash region (espflash) |
| `list_ports` | Discover serial ports |

## Capture

`flash_monitor`, `monitor`, and `rerun` capture until the first of:

- **`stop`** — an unanchored regex on the rendered line (`RESULT (PASS|FAIL)`,
  `panic|abort`). Plain text is a valid pattern.
- **`stop_on_level`** — defmt only: stop on the first frame at/above a level
  (e.g. `error`) — the "did it panic?" button.
- **`idle_ms`** — no new data for this long (default `4000`).
- **`timeout_s`** — max wall-clock window (default `5`).
- **`max_bytes`** — byte cap; stops early and marks the output truncated
  (default `65536`).

**Show filters:** `grep` (regex, both modes), `context` (N lines around the
`stop` match), and defmt-only `level` (minimum to show) / `module` (regex on the
module path). In defmt mode a suppressed-by-level count reports what a looser
`level` would reveal. In text mode, ROM/bootloader boot noise (`strip_boot_noise`)
and ANSI codes (`strip_ansi`) are stripped by default.

## defmt note

defmt decode needs the **exact ELF that's running** — version skew yields
garbage, not an error. It's free in the flash-then-monitor flow (just built it);
for bare `monitor`/`rerun`, make sure the auto-detected (or passed) ELF matches.
The server surfaces a warning when a non-empty stream decodes to zero frames.

## License

MIT — see [LICENSE](LICENSE).
