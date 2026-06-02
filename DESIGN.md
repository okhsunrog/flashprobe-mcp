# embedded-flash-mcp — module layout & dependency design

Design-for-review for the `espflash-mcp → embedded-flash-mcp` rework. No code
written yet. Maps directly onto the four plan milestones. Decisions already
settled in the plan (library-not-CLI, probe-rs-first, defmt on both backends,
no config file) are taken as given.

## 1. Module layout

The current `src/main.rs` (1419 lines) splits into a small tree. Grouping is by
**pipeline role**, so the milestone-2 probe-rs work and milestone-3 defmt work
each have an obvious home and the shared capture stage is touched once.

```
src/
  main.rs              tokio entry, tracing init, serve over stdio. ~30 lines.
  server.rs            Server struct, #[tool_router], ServerHandler::get_info +
                       instructions. Tool methods live here (rmcp needs the
                       #[tool] fns in one impl block) but stay THIN: parse args →
                       detect::resolve → backend dispatch → capture/render.

  detect.rs            Stateless auto-detection (milestone 4). Resolves, per call:
                         - elf: cargo_metadata + cwd → build artifact path
                         - chip: from the `target/<triple>/…` dir in the artifact
                                 path → chip id (override: `chip` arg)
                         - defmt: Table::parse(elf) is Some → defmt mode
                         - backend: probe present → ProbeRs, else Espflash
                                    (override: `backend` arg)
                       Ambiguity (multi-probe / multi-bin) → typed error telling
                       the agent which arg to pass in the SAME call.

  capture/
    mod.rs             run_capture(source, decoder, opts) -> CaptureResult.
                       The shared, transport-agnostic pipeline. THE preserved
                       asset: the early-exiting loop from read_serial_output.
    source.rs          `trait ByteSource` (the only backend-specific seam).
    decode.rs          `trait Decode`; TextDecoder + DefmtDecoder. Turns bytes
                       into rendered `Line { text, level, module }`.
    filter.rs          Stop conditions (stop regex, stop_on_level) + show filters
                       (grep, level, module, context). Ports trim_to_match /
                       filter_lines.
    render.rs          CaptureResult → compact header + fenced body + suppressed
                       counts. Ports render_capture.

  backend/
    mod.rs             Backend enum + the device-op trait surface
                       (flash, reset, erase_flash, erase_region, read_flash,
                       chip_info, checksum_md5) + ByteSource constructors.
    espflash.rs        connect_to_device, IdfBootloaderFormat flashing, raw-bin
                       write, all current device ops. SerialSource: ByteSource.
    probers.rs         probe-rs Session: flash via flashing::download_*, reset,
                       erase, read, chip info. RttSource: ByteSource over an RTT
                       up-channel. (milestone 2)

  esp_noise.rs         ESP-ROM/bootloader text scrubbing: strip_boot_noise,
                       strip_garbled_prefix, is_boot_noise_line, BOOT_NOISE_RE,
                       BOOT_BLOCK_GAP. ESP-text-mode only; defmt framing makes it
                       unnecessary in defmt mode. Lifted verbatim from main.rs.
```

Tests move next to their unit (`esp_noise.rs`, `filter.rs`, `decode.rs` each get
the relevant existing `#[cfg(test)]` cases). The current noise/filter tests
transplant unchanged — they're the regression net for milestone 1.

## 2. The core seam: `ByteSource`

Both espflash (`serialport`) and probe-rs (RTT) are **blocking** libraries. The
plan sketched an async source, but async here would only wrap blocking calls in
`spawn_blocking` — no real concurrency win. So the whole capture path stays
synchronous and runs inside the existing `tokio::task::spawn_blocking`, exactly
as today. This keeps milestone 1 a true no-behavior-change refactor.

```rust
/// The only thing that differs between backends.
pub trait ByteSource {
    /// Read whatever bytes are available right now into `buf`.
    /// Returns Ok(0) when nothing is ready yet (caller paces itself); the loop
    /// treats 0 as "no data this tick", not EOF. Err only on a real failure.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;

    /// Discard already-buffered input (the `flush` option). Default: no-op.
    fn flush_input(&mut self) -> std::io::Result<()> { Ok(()) }
}
```

- `SerialSource`: `serialport` opened with a 100 ms read timeout; maps
  `ErrorKind::TimedOut` → `Ok(0)`. Behaviour identical to today's loop.
- `RttSource`: holds the probe-rs `Session` + up-channel number; each `read`
  does `session.core(0)` then `channel.read(&mut core, buf)`, which returns 0
  immediately when empty. The loop sleeps ~10 ms on a 0 to avoid a busy-spin
  (serial already blocks 100 ms, so only RTT needs the nap).

## 3. Capture loop change (incremental decode)

Today the loop accumulates one raw `String`, runs the stop regex over the whole
buffer, and post-processes once at the end. That can't support defmt (frames
must be decoded as they arrive) or `stop_on_level` (needs a decoded level to
short-circuit). So the loop decodes **incrementally**:

```
loop tick:
  n = source.read(buf)
  for line in decoder.push(&buf[..n]):     // Vec<Line>, may be empty
      captured.push(line)
      if stop matches line.text  OR  stop_on_level <= line.level:
          arm match-grace
  enforce bounds: timeout / idle / max_bytes / match-grace expiry
```

`Decode::push(&[u8]) -> Vec<Line>`:
- **TextDecoder**: buffers bytes, splits on `\n`, emits `Line { text, level:
  None, module: None }`. Keeps a "pending partial line" the stop regex is also
  tested against, so a match on a not-yet-newline-terminated line still fires —
  preserving today's mid-line-match + grace behaviour.
- **DefmtDecoder**: feeds bytes to `defmt_decoder::Table::new_stream_decoder()`;
  each decoded `Frame` → `Line { text: display_message, level: frame.level(),
  module: frame.module_path() }`. The StreamDecoder picks rzCOBS-vs-raw framing
  from the ELF's encoding metadata, so the **same** decoder serves RTT and
  serial defmt — we feed raw bytes either way (no manual rzcobs).

**Deliberate semantic shift:** the stop pattern is now matched per rendered line
rather than across the whole raw buffer (so it can't span a newline). This is
the right model for an agent ("stop on the line that says X") and simplifies the
grace logic. Called out here so it's a conscious choice, not a silent regression.

Post-loop (in `render.rs` / `filter.rs`), text mode still runs `esp_noise`
stripping + `trim_to_match` exactly as today; defmt mode skips noise stripping
and applies structured `level`/`module` filters instead.

## 4. Field set (unified tool args)

One shared `CaptureArgs` behind `flash` / `rerun` / `monitor`:

| field          | mode      | role                                            |
|----------------|-----------|-------------------------------------------------|
| `stop`         | both      | regex on rendered line (was `stop_pattern`)     |
| `stop_on_level`| defmt     | stop on first frame ≥ level (e.g. `error`)      |
| `timeout_s`    | both      | wall-clock bound (was `timeout_secs`)           |
| `idle_ms`      | both      | no-new-data bound (was `idle_timeout_ms`)       |
| `max_bytes`    | both      | flood cap                                       |
| `grep`         | both      | keep matching lines (was `filter`)              |
| `level`        | defmt     | min level to show                               |
| `module`       | defmt     | regex on module path                            |
| `context`      | both      | N lines around the stop match (was context_lines)|
| `backend`      | both      | force `espflash`/`probe-rs` (override)          |
| `chip`,`probe`,`bin`,`manifest_path` | both | disambiguation overrides   |

Old `strip_boot_noise`/`strip_ansi`/`flush` stay (text-mode knobs), defaulting
`true` as today. Renames are **hard** (no serde aliases) — the project has no
known users, so back-compat is a non-goal and the surface stays clean.

## 5. Response shape (render.rs)

Compact header so the agent decides its next move without re-running:
`stop reason (matched|idle|timeout|cap)` · pattern hit y/n · `bytes captured vs
shown`. defmt mode adds a one-line **suppressed count**
(`hidden: 412 debug, 3 info`) — per the open-default decision, default to
**show-everything** and report what a tighter `level` would reveal, rather than
silently hiding at `info`.

## 6. Dependencies to add (verified versions)

| crate           | version  | why                                              |
|-----------------|----------|--------------------------------------------------|
| `probe-rs`      | `0.31`   | flashing API + RTT (RTT lives in-crate now; the standalone `probe-rs-rtt 0.14` is the old split). Big dep tree — isolated entirely in `backend/probers.rs`. |
| `defmt-decoder` | `1.1`    | `Table::parse` + `StreamDecoder`. **1.x is wire-stable-ish but still version-locks to the encoding — pin exact, expect to track breakage** (plan cost). |
| `cargo_metadata`| `0.23`   | locate the build artifact / workspace for `detect.rs`. |

Not needed: `object`/`goblin` (defmt presence comes from `Table::parse → Option`;
chip comes from the `target/<triple>/` path, not ELF bytes). `rzcobs` (the defmt
StreamDecoder frames internally). All kept out unless a milestone proves a need.

Feature-gating consideration: probe-rs roughly triples build time and dep count.
**Option** — put it behind a default-on `probe-rs` cargo feature so the espflash-
only build stays lean for users without a probe. Flagged for your call (§8).

## 7. Build order (milestone-mapped)

1. **M1 — refactor, zero behavior change.** Carve out the tree above; introduce
   `ByteSource` + `SerialSource`; convert the loop to incremental decode with
   `TextDecoder` only; move esp_noise + filter + render out. Existing tests pass
   verbatim (plus a TextDecoder line-emit test). No new deps. *Reviewable as a
   pure refactor.*
2. **M2 — probe-rs backend, text mode.** `backend/probers.rs`: flash via
   `flashing::download_file_with_options`, `RttSource`. Wire `backend` override.
   No defmt yet.
3. **M3 — defmt.** `DefmtDecoder` in the shared stage; `level`/`module`/
   `stop_on_level`; suppressed-count rendering. Validate framing on real
   hardware for *both* sources (serial esp-println framing is the risk).
4. **M4 — auto-detection + rename.** `detect.rs`; ambiguity errors; crate rename
   to `embedded-flash-mcp` (Cargo `name`, binary, README, MCP `server_info`).

## 8. Resolved decisions (locked for implementation)

1. **probe-rs gating:** default-on cargo `probe-rs` feature. espflash-only users
   build lean via `--no-default-features`; `backend/probers.rs` and its deps are
   `#[cfg(feature = "probe-rs")]`-gated.
2. **Arg renames:** hard rename now (`stop_pattern`→`stop`, `timeout_secs`→
   `timeout_s`, `idle_timeout_ms`→`idle_ms`, `filter`→`grep`, `context_lines`→
   `context`). No serde aliases — no known users, back-compat is a non-goal.
3. **Crate rename:** at M4, as planned. Earlier diffs stay small/bisectable.

## 9. Risks carried from the plan

- defmt needs the **exact** running ELF; skew → garbage not error. Free in
  flash-then-monitor; a footgun for bare `monitor`. `DefmtDecoder` surfaces a
  clear error when frames won't decode (malformed-frame rate high → "ELF/firmware
  mismatch?").
- Serial defmt shares the UART with ROM/boot **text**; the decoder must find
  frame boundaries amid non-defmt bytes. Relies on esp-println's framing markers
  — must be validated against hardware in M3, not assumed.
- probe-rs API moves faster than its CLI; pinned and isolated behind
  `backend/probers.rs` + the `ByteSource` seam.
