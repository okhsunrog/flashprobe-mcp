# espflash-mcp

An [MCP](https://modelcontextprotocol.io) server that wraps
[espflash](https://crates.io/crates/espflash) to flash, erase, read, and monitor
ESP32-family devices over a serial port — from any MCP client (Claude Code,
Claude Desktop, etc.).

Supported chips: ESP32, ESP32-S2/S3, ESP32-C2/C3/C5/C6, ESP32-H2, ESP32-P4.

The serial monitor is tuned for **LLM context efficiency**: by default captures
are cleaned so only the application output reaches the model — the ROM
baud-mismatch garbage, ESP-IDF bootloader log lines, and ANSI color codes are
stripped, and on a pattern match the output is focused on the matched line.

## Build

```sh
cargo build --release
# binary at: target/release/espflash-mcp
```

The server speaks MCP over stdio.

## Configure your MCP client

### Claude Code

```sh
claude mcp add espflash -- /absolute/path/to/espflash-mcp/target/release/espflash-mcp
```

### Generic (`mcpServers` JSON)

```json
{
  "mcpServers": {
    "espflash": {
      "command": "/absolute/path/to/espflash-mcp/target/release/espflash-mcp"
    }
  }
}
```

Serial access usually needs your user in the `dialout` (or `uucp`) group.

## Tools

| Tool | Purpose |
|------|---------|
| `list_ports` | Discover serial ports (with USB VID/PID) |
| `chip_info` | Chip type, revision, MAC, crystal freq, flash size, features |
| `flash` | Flash an ELF (IDF bootloader format) or a raw binary at an address |
| `flash_monitor` | Flash, then capture the boot output in one call |
| `rerun` | Reset (DTR/RTS) + flush + monitor — re-run the current firmware without reflashing |
| `monitor` | Read serial output for a bounded window |
| `reset_device` | Hardware reset via DTR/RTS |
| `erase_flash` | Erase the entire flash (destructive) |
| `erase_region` | Erase an aligned region (destructive) |
| `read_flash` | Read a flash region to a file |
| `checksum_md5` | MD5 of a flash region |

Every device tool takes a `port` (e.g. `/dev/ttyACM0`, `/dev/ttyUSB0`). Run
`list_ports` first to find it.

## Monitoring

`monitor`, `flash_monitor`, and `rerun` capture serial output until one of:

- `timeout_secs` — max wall-clock window (default `5`)
- `stop_pattern` — an **unanchored** regex matched anywhere in the output
  (alternation works: `RESULT (PASS|FAIL)`, `panic|abort|Guru Meditation`)
- `idle_timeout_ms` — no new data for this long (default `4000`; raise to
  6000–10000 for firmware that pauses between prints)
- `max_bytes` — byte cap; stops early and marks the output truncated
  (default `65536`, guards against reboot-loop floods)

### Output cleaning (defaults)

| Option | Default | Effect |
|--------|---------|--------|
| `strip_boot_noise` | `true` | Drop ROM baud-mismatch garbage and ESP-IDF bootloader log lines; keep output from the first application line |
| `strip_ansi` | `true` | Remove ANSI escape / color sequences (pure noise tokens for an LLM) |
| `context_lines` | _unset_ | On a `stop_pattern` match, return only the matched line plus this many lines before it (post-match reboot junk is always dropped) |
| `flush` | `true` (`monitor`/`rerun`) | Discard bytes buffered before the capture starts (drops a previous run's tail). `flash_monitor` never flushes — it captures from boot |

Set `strip_boot_noise: false` and `strip_ansi: false` to get the raw bytes.

### Typical flows

- **Flash and verify boot** — `flash_monitor` with a `stop_pattern` like
  `app_main|Ready` (or your own marker).
- **Iterate on the same binary** — `rerun` instead of `reset_device` + `monitor`;
  it resets, flushes the stale tail, and captures the fresh boot in one call.
- **Get a test result** — `stop_pattern: "RESULT (PASS|FAIL)"` with
  `context_lines: 5` to return just the verdict and a little context.

## License

See repository.
