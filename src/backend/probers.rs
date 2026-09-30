//! probe-rs backend: JTAG/SWD flashing via the flashing API and a [`ByteSource`]
//! over RTT or semihosting, selected from the ELF or an explicit override.
//!
//! This entire module is gated behind the default-on `probe-rs` cargo feature;
//! the dep tree is large, so an espflash-only build can drop it with
//! `--no-default-features`.

use super::semihosting::SemihostingSource;
use super::stacktrace::StackTracer;
use crate::capture::ByteSource;
use probe_rs::config::{MemoryRegion, Registry};
use probe_rs::flashing::{
    ElfLoader, ElfOptions, FlashError, FlashProgress, ImageLoader, build_loader, download_file,
    erase, erase_all,
};
use probe_rs::probe::list::Lister;
use probe_rs::probe::{DebugProbeInfo, WireProtocol};
use probe_rs::rtt::{ChannelMode, Rtt, ScanRegion};
use probe_rs::semihosting::SemihostingCommand;
use probe_rs::{BreakpointCause, CoreStatus, HaltReason, MemoryInterface, Permissions, Session};
use std::time::{Duration, Instant};

/// Default RTT up-channel to read (channel 0 is the conventional terminal).
const RTT_UP_CHANNEL: usize = 0;

/// Default RTT down-channel to write (channel 0 is the conventional input).
const RTT_DOWN_CHANNEL: usize = 0;

/// Address of the `_SEGGER_RTT` control block from the ELF symbol table, if
/// present. Lets us attach via [`ScanRegion::Exact`] — an instant pointer read
/// — instead of scanning the whole RAM (which is seconds-slow over SWD on a
/// large-RAM target like RP2350).
///
/// This is literally the helper cargo-embed attaches with, so we inherit its
/// symbol-table handling (notably: skip an *undefined* `_SEGGER_RTT`, whose
/// address would be bogus) instead of maintaining our own ELF parse.
fn rtt_control_block_addr(elf_path: &str) -> Option<u64> {
    let data = std::fs::read(elf_path).ok()?;
    probe_rs::rtt::find_rtt_control_block_in_raw_file(&data).ok()?
}

/// Register Espressif support with probe-rs' global registry.
///
/// As of probe-rs 0.32 the core crate ships *no* ESP chip targets at all — the
/// families, debug sequences, the esp-usb-jtag probe driver and the IDF image
/// format all live in `probe-rs-espressif` and are pulled in through the plugin
/// system. Without this call every `chip = "esp..."` request fails at target
/// lookup, so it has to run before any registry read (i.e. before attaching).
/// Idempotent: registration appends to global lists, so it must happen once.
fn register_espressif() {
    static ESPRESSIF: std::sync::Once = std::sync::Once::new();
    ESPRESSIF.call_once(probe_rs_espressif::register_plugin);
}

/// Open a session to `chip` through a connected probe. `probe_sel` optionally
/// selects a probe by `VID:PID` or `VID:PID:SERIAL` (hex VID/PID). With no
/// selector, a single connected probe is used; with several, the one whose
/// chip is identified as `chip` (see [`identify_chip`]). When that does not
/// single one out, the error lists every probe with its chip.
pub fn open_session(chip: &str, probe_sel: Option<&str>) -> Result<Session, String> {
    register_espressif();

    let lister = Lister::new();
    let probes = lister.list_all();
    if probes.is_empty() {
        return Err(
            "No debug probes found. Connect a probe, or use backend=espflash for UART.".into(),
        );
    }

    let info = match probe_sel {
        Some(sel) => probes
            .iter()
            .find(|p| probe_matches(p, sel))
            .ok_or_else(|| {
                format!(
                    "No probe matches '{sel}'. Connected:\n{}",
                    probe_listing(&identify_probes(&probes))
                )
            })?,
        None if probes.len() == 1 => &probes[0],
        None => &probes[pick_by_chip(chip, &identify_probes(&probes))?],
    };

    let probe = info
        .open()
        .map_err(|e| format!("Failed to open probe '{}': {e}", info.identifier))?;
    probe
        .attach(chip, Permissions::default())
        .map_err(|e| format!("Failed to attach to '{chip}': {e}"))
}

/// `true` if the probe matches a `VID:PID` or `VID:PID:SERIAL` selector (hex).
fn probe_matches(p: &DebugProbeInfo, sel: &str) -> bool {
    selector_matches(sel, p.vendor_id, p.product_id, p.serial_number.as_deref())
}

/// Everything after the second colon is the serial, verbatim: ESP USB-JTAG
/// serials are MAC addresses (`1C:DB:D4:48:ED:98`), so splitting on every colon
/// would keep only their first byte. Serials compare case-insensitively since
/// they are usually hex and tools disagree on the case they print.
fn selector_matches(sel: &str, vendor_id: u16, product_id: u16, serial: Option<&str>) -> bool {
    let mut parts = sel.splitn(3, ':');
    let (Some(vid), Some(pid)) = (parts.next(), parts.next()) else {
        return false;
    };
    let vid = u16::from_str_radix(vid.trim_start_matches("0x"), 16);
    let pid = u16::from_str_radix(pid.trim_start_matches("0x"), 16);
    let (Ok(vid), Ok(pid)) = (vid, pid) else {
        return false;
    };
    if vendor_id != vid || product_id != pid {
        return false;
    }
    match parts.next() {
        Some(wanted) => serial.is_some_and(|s| s.eq_ignore_ascii_case(wanted)),
        None => true,
    }
}

/// A probe as messages show it: `VID:PID:SERIAL (identifier)`. The part
/// before the space is exactly what the `probe` argument takes.
fn probe_label(p: &DebugProbeInfo) -> String {
    let serial = p.serial_number.as_deref().unwrap_or("-");
    format!(
        "{:04x}:{:04x}:{serial} ({})",
        p.vendor_id, p.product_id, p.identifier
    )
}

/// What the chip behind a probe was identified as.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Identity {
    Chip(String),
    /// Chips that share the IDCODE read, such as the Xtensa ESPs.
    OneOf(Vec<String>),
    /// Nothing the IDCODE alone can name, e.g. an ARM target.
    Unknown,
    /// The probe could not be asked, typically because another program holds it.
    Unavailable(String),
}

/// The chips an Espressif detection entry names for `idcode`. The entries
/// map a magic value, read from the chip's memory, to a target; every RISC-V
/// ESP has an IDCODE of its own and its magic values only tell revisions
/// apart, so the IDCODE alone already names the chip.
fn identity_from_idcode(registry: &Registry, idcode: u32) -> Option<Identity> {
    let mut chips: Vec<String> = Vec::new();
    for detection in registry
        .families()
        .iter()
        .flat_map(|family| family.chip_detection.iter())
        .filter_map(|detection| detection.as_espressif())
        .filter(|detection| detection.idcode == idcode)
    {
        for chip in detection.variants.values() {
            if !chips.contains(chip) {
                chips.push(chip.clone());
            }
        }
    }
    match chips.len() {
        0 => None,
        1 => chips.pop().map(Identity::Chip),
        _ => Some(Identity::OneOf(chips)),
    }
}

/// The chip behind a JTAG probe, identified from the IDCODEs of its TAPs.
///
/// That is the case that needs it: every ESP board brings its own USB-JTAG
/// probe, so two boards on one host are two probes that differ only in
/// serial. Anything more (an Xtensa ESP's magic value, an ARM ROM table)
/// needs a debug session, and attaching one has side effects on a board
/// nobody asked to touch: the ESP sequences disable its watchdogs, and the
/// magic value is read with the core halted. Reading IDCODEs does not involve
/// the core.
fn identify_chip(info: &DebugProbeInfo, registry: &Registry) -> Identity {
    let mut probe = match info.open() {
        Ok(probe) => probe,
        Err(e) => return Identity::Unavailable(format!("cannot open it: {e}")),
    };
    if probe.protocol().is_none() && probe.select_protocol(WireProtocol::Jtag).is_err() {
        return Identity::Unknown;
    }
    if probe.protocol() != Some(WireProtocol::Jtag) {
        return Identity::Unknown;
    }
    if let Err(e) = probe.attach_to_unspecified() {
        return Identity::Unavailable(format!("cannot attach to it: {e}"));
    }
    let Some(jtag) = probe.try_as_jtag_probe() else {
        return Identity::Unknown;
    };
    let taps = match jtag.scan_chain() {
        Ok(chain) => chain.len(),
        Err(e) => return Identity::Unavailable(format!("cannot scan its JTAG chain: {e}")),
    };
    for tap in 0..taps {
        if jtag.select_target(tap).is_err() {
            break;
        }
        let Ok(bits) = jtag.read_register(1, 32) else {
            break;
        };
        let idcode = bits
            .iter()
            .by_vals()
            .take(32)
            .enumerate()
            .fold(0u32, |acc, (i, bit)| acc | (u32::from(bit) << i));
        if let Some(identity) = identity_from_idcode(registry, idcode) {
            return identity;
        }
    }
    Identity::Unknown
}

/// Every probe with what it was identified as.
fn identify_probes(probes: &[DebugProbeInfo]) -> Vec<(String, Identity)> {
    let registry = Registry::from_builtin_families();
    probes
        .iter()
        .map(|p| (probe_label(p), identify_chip(p, &registry)))
        .collect()
}

/// One line per probe, with its chip where known.
fn probe_listing(probes: &[(String, Identity)]) -> String {
    probes
        .iter()
        .map(|(label, identity)| match identity {
            Identity::Chip(chip) => format!("- {label}: {chip}"),
            Identity::OneOf(chips) => {
                format!("- {label}: one of {} (same JTAG IDCODE)", chips.join(", "))
            }
            Identity::Unknown => {
                format!("- {label}: chip not identifiable without a debug session")
            }
            Identity::Unavailable(e) => format!("- {label}: not checked, {e}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Which of several probes to use for `chip`: the only one identified as it.
fn pick_by_chip(chip: &str, probes: &[(String, Identity)]) -> Result<usize, String> {
    let matches: Vec<usize> = probes
        .iter()
        .enumerate()
        .filter(|(_, (_, identity))| {
            matches!(identity, Identity::Chip(c) if c.eq_ignore_ascii_case(chip))
        })
        .map(|(i, _)| i)
        .collect();
    if let [only] = matches[..] {
        return Ok(only);
    }
    let how_many = match matches.len() {
        0 => format!("none of them was identified as {chip}"),
        n => format!("{n} of them are {chip}"),
    };
    Err(format!(
        "{} probes are connected and {how_many}; pass `probe` as VID:PID:SERIAL to choose \
         one:\n{}",
        probes.len(),
        probe_listing(probes)
    ))
}

/// probe-rs flashes ESP chips through the IDF bootloader image; everything else
/// is a straight ELF download.
fn format_for_chip(chip: &str) -> Box<dyn ImageLoader> {
    if chip.to_ascii_lowercase().starts_with("esp") {
        Box::new(probe_rs_espressif::image_format::IdfLoader::default())
    } else {
        Box::new(ElfLoader(ElfOptions::default()))
    }
}

/// Download `path` to flash (no reset). Returns a human-readable summary.
pub fn download(session: &mut Session, path: &str, chip: &str) -> Result<String, String> {
    download_file(session, path, format_for_chip(chip))
        .map_err(|e| format!("probe-rs flash failed: {e}"))?;
    Ok(format!("Flashed {path} to {chip} via probe-rs (JTAG/SWD)"))
}

/// Whether the flash holds exactly the image `path` would be flashed as.
///
/// Reads back every range the image covers and compares, the way
/// `probe-rs run --preverify` decides whether it can skip flashing. Nothing
/// is written. `Ok(false)` is a plain mismatch; `Err` is a probe or image
/// problem.
///
/// This matters for embedded-test firmware in particular: the host runner
/// tells the target which test to run by *address*, taken from the ELF on
/// disk. If the flash holds an older build, those addresses land in the wrong
/// functions and the run fails with random exceptions that look like firmware
/// bugs. A capture that does not flash first has to check.
pub fn verify_flash(session: &mut Session, path: &str, chip: &str) -> Result<bool, String> {
    let loader = build_loader(session, path, format_for_chip(chip), None)
        .map_err(|e| format!("Cannot load '{path}' for verification: {e}"))?;
    match loader.verify(session, &mut FlashProgress::empty()) {
        Ok(()) => Ok(true),
        Err(FlashError::Verify) => Ok(false),
        Err(e) => Err(format!("probe-rs flash verification failed: {e}")),
    }
}

/// Where the core stopped, when it stopped before RTT came up.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stuck {
    /// At SYS_GET_CMDLINE, the way an embedded-test binary waits for its runner.
    CommandLine,
    /// At a semihosting exit.
    Exited,
    /// At another semihosting request, waiting for a host to answer it.
    Semihosting,
    /// For another reason, e.g. an exception.
    Halted(String),
    LockedUp,
}

impl Stuck {
    fn of(session: &mut Session) -> Option<Self> {
        let status = session.core(0).ok()?.status().ok()?;
        Some(match status {
            CoreStatus::Halted(HaltReason::Breakpoint(BreakpointCause::Semihosting(command))) => {
                match command {
                    SemihostingCommand::GetCommandLine(_) => Self::CommandLine,
                    SemihostingCommand::ExitSuccess | SemihostingCommand::ExitError(_) => {
                        Self::Exited
                    }
                    _ => Self::Semihosting,
                }
            }
            CoreStatus::Halted(reason) => Self::Halted(format!("{reason:?}")),
            CoreStatus::LockedUp => Self::LockedUp,
            _ => return None,
        })
    }

    fn describe(&self) -> String {
        match self {
            Self::CommandLine => "halted at a semihosting SYS_GET_CMDLINE request, the way an \
                                  embedded-test binary waits for its runner"
                .into(),
            Self::Exited => "halted at a semihosting exit".into(),
            Self::Semihosting => {
                "halted at a semihosting request, waiting for a host to answer it".into()
            }
            Self::Halted(reason) => format!("halted ({reason})"),
            Self::LockedUp => "locked up".into(),
        }
    }
}

/// Why RTT never came up, when the core's state says so.
///
/// A timeout alone cannot tell a firmware that is slow to initialize RTT from
/// one that will never get there. A core that is halted settles it, and the
/// halt reason usually names the fix, so a caller need not guess.
///
/// Where it stopped only means something for the firmware `elf` describes,
/// though. `check_flash` says the flash may hold another build (the capture
/// did not just flash `elf`); the flash is then compared with `elf` first, and
/// a mismatch is the answer. That comparison stops the core, so it only runs
/// on a core that has already stopped by itself.
fn rtt_failure_cause(
    session: &mut Session,
    elf: Option<&str>,
    check_flash: bool,
) -> Option<String> {
    let stuck = Stuck::of(session)?;
    let what = stuck.describe();
    if check_flash && let Some(path) = elf {
        let chip = session.target().name.clone();
        if verify_flash(session, path, &chip) == Ok(false) {
            return Some(format!(
                "The flash does not hold the image built from '{path}': the device runs a \
                 different build, whose core is {what}. Flash this ELF first (`flash_monitor` \
                 with it), or pass the ELF of the firmware that is on the device."
            ));
        }
    }
    Some(match (stuck, elf) {
        // The flash holds `elf`, and RTT was chosen for it, so it has no
        // `.embedded_test` section: the firmware asks for its command line itself.
        (Stuck::CommandLine, Some(path)) => format!(
            "The core is halted at a semihosting SYS_GET_CMDLINE request. '{path}' is not an \
             embedded-test binary, so the firmware asks the host for its command line itself, \
             and only the semihosting transport answers that. Run it with transport \
             \"semihosting\"."
        ),
        (Stuck::CommandLine, None) => format!(
            "The core is {what}, before `#[init]` sets up RTT. Pass the ELF of that test \
             binary as `elf`: \"auto\" then runs it as a suite over semihosting and reads \
             its RTT log alongside."
        ),
        (Stuck::Exited, _) => format!(
            "The core is {what}: the firmware finished before it initialized RTT. Run it with \
             transport \"semihosting\" to see its exit status."
        ),
        (Stuck::Semihosting, _) => {
            format!("The core is {what}. Run it with transport \"semihosting\".")
        }
        (Stuck::Halted(_) | Stuck::LockedUp, _) => {
            format!("The core is {what}, so the firmware stopped before it initialized RTT.")
        }
    })
}

/// Download then reset-and-run so the firmware executes (no monitoring).
pub fn flash(session: &mut Session, path: &str, chip: &str) -> Result<String, String> {
    let summary = download(session, path, chip)?;
    reset(session)?;
    Ok(summary)
}

/// Reset-and-run the firmware (probe-rs equivalent of a DTR/RTS reset).
pub fn reset(session: &mut Session) -> Result<(), String> {
    session
        .core(0)
        .map_err(|e| format!("Failed to access core: {e}"))?
        .reset()
        .map_err(|e| format!("Failed to reset device: {e}"))?;
    Ok(())
}

/// Reset the device and attach RTT so capture begins at the *start* of the run.
///
/// Three things have to line up to capture from the first frame:
///
/// 1. **Fast attach.** Resolve the control block via the `_SEGGER_RTT` ELF
///    symbol ([`ScanRegion::Exact`]) so attach is an instant pointer read. The
///    default whole-RAM scan takes *seconds* over SWD on a large-RAM target
///    (e.g. ~18 s on RP2350's 520 KB), by which point the firmware has run to
///    completion and overwritten the buffer.
/// 2. **No stale block.** Reset-and-halt at the vector, zero the old control
///    block, then run — so the poll loop can only latch on once the firmware
///    re-initializes RTT with a fresh read pointer at 0.
/// 3. **Temporary blocking mode.** defmt-rtt boots the up-channel in a
///    non-blocking mode. Switch it to `BlockIfFull` while the host is actively
///    draining the channel so the earliest frames are not discarded. Remember
///    the firmware-configured mode and restore it when capture ends; otherwise
///    a later log write can freeze the target once no host is reading RTT.
///
/// If the ELF has no `_SEGGER_RTT` symbol we fall back to the slow RAM scan,
/// which is correct but may miss early frames on large-RAM targets.
///
/// `check_flash`: the flash may hold another build than `elf_path`, which a
/// failure to attach then checks (see [`rtt_failure_cause`]).
pub fn reset_and_attach_rtt(
    mut session: Session,
    elf_path: Option<&str>,
    attach_timeout: Duration,
    check_flash: bool,
) -> Result<RttSource, String> {
    let exact = elf_path.and_then(rtt_control_block_addr);
    let region = match exact {
        Some(addr) => ScanRegion::Exact(addr),
        None => ScanRegion::Ram,
    };

    {
        let mut core = session
            .core(0)
            .map_err(|e| format!("Failed to access core: {e}"))?;
        core.reset_and_halt(Duration::from_millis(500))
            .map_err(|e| format!("Failed to reset-and-halt: {e}"))?;
        // Invalidate a stale control block (magic + pointers from the previous
        // run) so the poll loop below can't latch onto it before the firmware
        // re-initializes RTT with a fresh read pointer at 0.
        if let Ok(addr) = Rtt::find_control_block(&mut core, &region) {
            let zeros = vec![0u8; Rtt::control_block_size()];
            let _ = core.write(addr, &zeros);
        }
        core.run()
            .map_err(|e| format!("Failed to run after reset: {e}"))?;
    }

    let deadline = Instant::now() + attach_timeout;
    let mut rtt = loop {
        let attached = {
            let mut core = session
                .core(0)
                .map_err(|e| format!("Failed to access core: {e}"))?;
            Rtt::attach_region(&mut core, &region).ok()
        };
        match attached {
            Some(rtt) => break rtt,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            None => {
                if let Some(cause) = rtt_failure_cause(&mut session, elf_path, check_flash) {
                    return Err(format!(
                        "RTT control block did not appear within {:.1}s after reset. {cause}",
                        attach_timeout.as_secs_f32()
                    ));
                }
                // Say which of the two failure shapes this is: a firmware that
                // never initializes RTT looks the same as one that simply took
                // longer to get there, and the fix is different for each.
                let how = match exact {
                    Some(addr) => format!(
                        "`_SEGGER_RTT` resolved to {addr:#x} from the ELF, so the block was \
                         read directly and the firmware simply never wrote its magic there"
                    ),
                    None => "the ELF has no `_SEGGER_RTT` symbol, so this fell back to \
                             scanning the whole RAM, which is slow on a large-RAM target"
                        .to_string(),
                };
                return Err(format!(
                    "RTT control block did not appear within {:.1}s after reset: {how}.\n\
                     Worth checking:\n\
                     - Is the firmware built with an RTT transport such as defmt-rtt or \
                       rtt-target, and does it initialize it before any long setup work?\n\
                     - Does the target boot through a bootloader that runs for longer than \
                       this? Raise `rtt_attach_timeout_ms`.\n\
                     - Does the firmware repurpose the debug pins (MTCK/MTDO/MTMS/MTDI)? \
                       That disturbs reset-and-attach on some targets.\n\
                     If the debug connection itself looks wedged, reflashing over UART \
                     with the espflash backend resets the target out of band and has \
                     recovered it before.",
                    attach_timeout.as_secs_f32()
                ));
            }
        }
    };

    // Temporarily switch the up-channel to BlockIfFull (see item 3 above),
    // before the ~1 KB buffer can wrap. RttSource restores this mode on drop.
    let restore_mode = {
        let mut core = session
            .core(0)
            .map_err(|e| format!("Failed to access core: {e}"))?;
        let up = rtt
            .up_channel(RTT_UP_CHANNEL)
            .ok_or_else(|| format!("RTT up-channel {RTT_UP_CHANNEL} not found"))?;
        let original_mode = up
            .mode(&mut core)
            .map_err(|e| format!("Failed to read RTT channel mode: {e}"))?;
        up.set_mode(&mut core, ChannelMode::BlockIfFull)
            .map_err(|e| format!("Failed to set RTT channel to blocking mode: {e}"))?;
        original_mode
    };

    Ok(RttSource {
        session,
        rtt,
        channel: RTT_UP_CHANNEL,
        restore_mode: Some(restore_mode),
        tracer: elf_path.map(StackTracer::new),
    })
}

/// Erase the entire flash via the flash algorithm.
pub fn erase_flash(session: &mut Session) -> Result<String, String> {
    erase_all(session, &mut FlashProgress::empty(), false)
        .map_err(|e| format!("probe-rs erase_all failed: {e}"))?;
    Ok("Successfully erased entire flash memory.".to_string())
}

/// Erase the flash sectors covering `[address, address+size)`.
pub fn erase_region(session: &mut Session, address: u32, size: u32) -> Result<String, String> {
    let start = address as u64;
    let end = start + size as u64;
    erase(session, &mut FlashProgress::empty(), start, end, false)
        .map_err(|e| format!("probe-rs erase failed: {e}"))?;
    Ok(format!(
        "Successfully erased the sectors covering 0x{size:x} bytes at 0x{address:08x}"
    ))
}

/// Read `size` bytes of target memory at `address` into `out_path`.
pub fn read_flash(
    session: &mut Session,
    address: u32,
    size: u32,
    out_path: &str,
) -> Result<u64, String> {
    let mut buf = vec![0u8; size as usize];
    session
        .core(0)
        .map_err(|e| format!("Failed to access core: {e}"))?
        .read(address as u64, &mut buf)
        .map_err(|e| format!("probe-rs memory read failed: {e}"))?;
    std::fs::write(out_path, &buf).map_err(|e| format!("Failed to write '{out_path}': {e}"))?;
    Ok(buf.len() as u64)
}

/// MD5 of `size` bytes of target memory at `address` (read host-side; probe-rs
/// has no on-device MD5 like the ESP ROM does).
pub fn checksum_md5(session: &mut Session, address: u32, size: u32) -> Result<String, String> {
    let mut buf = vec![0u8; size as usize];
    session
        .core(0)
        .map_err(|e| format!("Failed to access core: {e}"))?
        .read(address as u64, &mut buf)
        .map_err(|e| format!("probe-rs memory read failed: {e}"))?;
    Ok(format!("{:x}", md5::compute(&buf)))
}

/// Target/chip information from the probe-rs target description: name, cores, and
/// memory map. (MAC/crystal/revision are ESP-ROM concepts, not available here.)
pub fn chip_info(session: &mut Session) -> Result<String, String> {
    let name = session.target().name.clone();
    let cores: Vec<String> = session
        .list_cores()
        .into_iter()
        .map(|(i, kind)| format!("core {i}: {kind:?}"))
        .collect();
    let regions: Vec<String> = session
        .target()
        .memory_map
        .iter()
        .map(|r| {
            let (kind, label) = match r {
                MemoryRegion::Ram(m) => ("RAM", m.name.as_deref()),
                MemoryRegion::Nvm(m) => ("flash/NVM", m.name.as_deref()),
                MemoryRegion::Generic(m) => ("generic", m.name.as_deref()),
            };
            let range = r.address_range();
            format!(
                "- {kind}{}: 0x{:08x}..0x{:08x} ({} KiB)",
                label.map(|n| format!(" \"{n}\"")).unwrap_or_default(),
                range.start,
                range.end,
                (range.end - range.start) / 1024
            )
        })
        .collect();

    let mut out = format!("## Target Information (probe-rs)\n\n- Target: {name}\n");
    out.push_str(&format!("- Cores: {}\n", cores.join(", ")));
    out.push_str("- Memory map:\n");
    for region in regions {
        out.push_str(&format!("  {region}\n"));
    }
    Ok(out)
}

/// A [`ByteSource`] over an RTT up-channel. Holds the session and re-borrows the
/// core on each read; RTT reads return immediately, so the loop naps ~10 ms on an
/// empty read to avoid busy-spinning.
pub struct RttSource {
    session: Session,
    rtt: Rtt,
    channel: usize,
    /// Mode to restore after a temporary host-side override.
    restore_mode: Option<ChannelMode>,
    /// Present when the ELF is known, which unwinding needs.
    tracer: Option<StackTracer>,
}

impl RttSource {
    /// Attach RTT on an existing session. The firmware must already be running
    /// (just flashed/reset, or attached live) and built with an RTT transport.
    ///
    /// `elf_path` lets us pin the control block to the `_SEGGER_RTT` symbol
    /// ([`ScanRegion::Exact`]) — without it, a whole-RAM scan can find STALE
    /// control blocks left by previous firmware images in uninitialized RAM
    /// (CCMRAM/SRAM2 on STM32, etc.) and fail with "multiple control blocks".
    ///
    /// `check_flash` is as for [`reset_and_attach_rtt`].
    pub fn attach(
        mut session: Session,
        elf_path: Option<&str>,
        attach_timeout: Duration,
        check_flash: bool,
    ) -> Result<Self, String> {
        let region = match elf_path.and_then(rtt_control_block_addr) {
            Some(addr) => ScanRegion::Exact(addr),
            None => ScanRegion::Ram,
        };
        // Retry rather than give up on the first miss: the target may have been
        // reset by hand a moment ago and still be in its bootloader.
        let deadline = Instant::now() + attach_timeout;
        let rtt = loop {
            let attached = {
                let mut core = session
                    .core(0)
                    .map_err(|e| format!("Failed to access core: {e}"))?;
                Rtt::attach_region(&mut core, &region)
            };
            match attached {
                Ok(rtt) => break rtt,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => {
                    let within = attach_timeout.as_secs_f32();
                    // A known cause replaces probe-rs' generic hints rather than
                    // trailing after them.
                    return Err(
                        match rtt_failure_cause(&mut session, elf_path, check_flash) {
                            Some(cause) => {
                                format!("Failed to attach RTT within {within:.1}s. {cause}")
                            }
                            None => format!(
                                "Failed to attach RTT within {within:.1}s: {e}\nIs the firmware \
                                 running and built with an RTT transport?"
                            ),
                        },
                    );
                }
            }
        };
        Ok(Self {
            session,
            rtt,
            channel: RTT_UP_CHANNEL,
            restore_mode: None,
            tracer: elf_path.map(StackTracer::new),
        })
    }

    fn restore_channel_mode(&mut self) -> Result<(), String> {
        let Some(mode) = self.restore_mode else {
            return Ok(());
        };
        let mut core = self
            .session
            .core(0)
            .map_err(|e| format!("Failed to access core while restoring RTT mode: {e}"))?;
        let up = self
            .rtt
            .up_channel(self.channel)
            .ok_or_else(|| format!("RTT up-channel {} not found", self.channel))?;
        up.set_mode(&mut core, mode)
            .map_err(|e| format!("Failed to restore RTT channel mode: {e}"))?;
        self.restore_mode = None;
        Ok(())
    }
}

impl ByteSource for RttSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut core = self
            .session
            .core(0)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let ch = self.rtt.up_channel(self.channel).ok_or_else(|| {
            std::io::Error::other(format!("RTT up-channel {} not found", self.channel))
        })?;
        ch.read(&mut core, buf)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }

    fn flush_input(&mut self) -> std::io::Result<()> {
        // Drain whatever is already buffered in the channel.
        let mut scratch = [0u8; 1024];
        while self.read(&mut scratch)? > 0 {}
        Ok(())
    }

    /// Push bytes into the RTT down-channel. Non-blocking on the probe-rs side:
    /// it writes as much as fits in the ring buffer and reports the count, so a
    /// short write here is backpressure that [`send_all`] retries.
    ///
    /// [`send_all`]: crate::capture::source::send_all
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut core = self
            .session
            .core(0)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let ch = self.rtt.down_channel(RTT_DOWN_CHANNEL).ok_or_else(|| {
            std::io::Error::other(format!(
                "RTT down-channel {RTT_DOWN_CHANNEL} not found: this firmware exposes no \
                 down-channels, so it cannot receive host input. `defmt-rtt` declares \
                 max_down_channels = 0; to accept input use `rtt-target` with an explicit \
                 `rtt_init!` that declares a down-channel (defmt still works via \
                 `set_defmt_channel` on the up-channel)."
            ))
        })?;
        ch.write(&mut core, buf)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }

    fn idle_nap(&self) -> Duration {
        Duration::from_millis(10)
    }

    fn stack_trace(&mut self, full: bool) -> Option<Result<String, String>> {
        let tracer = self.tracer.as_mut()?;
        Some(tracer.trace_session(&mut self.session, full))
    }
}

impl Drop for RttSource {
    fn drop(&mut self) {
        if let Err(error) = self.restore_channel_mode() {
            tracing::warn!("{error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use probe_rs::config::Registry;

    #[test]
    fn selector_keeps_colons_in_serial() {
        let serial = Some("1C:DB:D4:48:ED:98");
        assert!(selector_matches(
            "303a:1001:1C:DB:D4:48:ED:98",
            0x303a,
            0x1001,
            serial
        ));
        assert!(selector_matches(
            "303a:1001:1c:db:d4:48:ed:98",
            0x303a,
            0x1001,
            serial
        ));
        assert!(!selector_matches("303a:1001:1C", 0x303a, 0x1001, serial));
        assert!(!selector_matches(
            "303a:1001:58:E6:C5:17:35:7C",
            0x303a,
            0x1001,
            serial
        ));
    }

    fn probes(chips: &[Identity]) -> Vec<(String, Identity)> {
        chips
            .iter()
            .enumerate()
            .map(|(i, chip)| (format!("303a:1001:0{i} (ESP JTAG)"), chip.clone()))
            .collect()
    }

    /// The IDCODEs come from the targets' detection data: each RISC-V ESP has
    /// its own, while the Xtensa ones share one and need the magic value.
    #[test]
    fn the_idcode_names_riscv_esp_chips() {
        register_espressif();
        let registry = Registry::from_builtin_families();
        for (idcode, chip) in [
            (0x5c25, "esp32c3"),
            (0x17c25, "esp32c5"),
            (0xdc25, "esp32c6"),
            (0x10c25, "esp32h2"),
        ] {
            assert_eq!(
                identity_from_idcode(&registry, idcode),
                Some(Identity::Chip(chip.into()))
            );
        }
        let Some(Identity::OneOf(xtensa)) = identity_from_idcode(&registry, 0x120034e5) else {
            panic!("the Xtensa ESPs share an IDCODE");
        };
        assert!(xtensa.iter().any(|c| c == "esp32s3"), "{xtensa:?}");
        assert_eq!(identity_from_idcode(&registry, 0x4ba00477), None);
    }

    #[test]
    fn the_only_probe_with_the_chip_is_picked() {
        let connected = probes(&[
            Identity::Chip("esp32c6".into()),
            Identity::Chip("esp32c5".into()),
            Identity::Unknown,
            Identity::Unavailable("cannot open it: busy".into()),
        ]);
        assert_eq!(pick_by_chip("ESP32C5", &connected), Ok(1));
    }

    #[test]
    fn an_ambiguous_choice_lists_every_probe_with_its_chip() {
        let connected = probes(&[
            Identity::Chip("esp32c5".into()),
            Identity::Chip("esp32c5".into()),
            Identity::OneOf(vec!["esp32".into(), "esp32s3".into()]),
            Identity::Unknown,
            Identity::Unavailable("cannot open it: busy".into()),
        ]);
        let err = pick_by_chip("esp32c5", &connected).unwrap_err();
        assert!(err.contains("2 of them are esp32c5"), "{err}");
        assert!(err.contains("- 303a:1001:00 (ESP JTAG): esp32c5"), "{err}");
        assert!(err.contains("one of esp32, esp32s3"), "{err}");
        assert!(
            err.contains("not identifiable without a debug session"),
            "{err}"
        );
        assert!(
            err.contains("04 (ESP JTAG): not checked, cannot open it"),
            "{err}"
        );

        // A probe that may be the chip is not picked on a guess.
        let err = pick_by_chip("esp32s3", &connected).unwrap_err();
        assert!(
            err.contains("none of them was identified as esp32s3"),
            "{err}"
        );
    }

    #[test]
    fn selector_vid_pid_only() {
        assert!(selector_matches("0x303a:1001", 0x303a, 0x1001, None));
        assert!(!selector_matches("303a:1002", 0x303a, 0x1001, None));
        assert!(!selector_matches("303a:1001:ABC", 0x303a, 0x1001, None));
        assert!(!selector_matches("303a", 0x303a, 0x1001, None));
    }

    /// probe-rs 0.32 moved all Espressif support out of the core crate, so ESP
    /// chips only resolve once [`register_espressif`] has run. These assertions
    /// need no hardware: they exercise the target registry directly, which is
    /// the exact lookup `open_session` does before it ever touches a probe.
    #[test]
    fn espressif_targets_are_registered() {
        register_espressif();
        let registry = Registry::from_builtin_families();

        for chip in [
            "esp32", "esp32c3", "esp32c6", "esp32s3", "esp32h2", "esp32p4",
        ] {
            assert!(
                registry.get_target_by_name(chip).is_ok(),
                "ESP target '{chip}' missing — is probe-rs-espressif still registered?"
            );
        }
    }

    /// The plugin adds to the registry rather than replacing it; make sure the
    /// non-ESP targets that probe-rs ships builtin are still reachable.
    #[test]
    fn builtin_targets_survive_plugin_registration() {
        register_espressif();
        let registry = Registry::from_builtin_families();

        for chip in ["STM32F103C8", "RP2350", "nRF52840_xxAA"] {
            assert!(
                registry.get_target_by_name(chip).is_ok(),
                "builtin target '{chip}' no longer resolves"
            );
        }
    }

    /// ESP chips are flashed as an IDF bootloader image; that image format also
    /// ships in the plugin crate now, so `format_for_chip` depends on it.
    #[test]
    fn idf_image_format_is_registered() {
        register_espressif();
        assert!(
            probe_rs::flashing::image_format("idf").is_some(),
            "IDF image format missing — ESP flashing would fall back to raw ELF"
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTransport {
    Rtt,
    /// Semihosting. `rtt`: the ELF defines `_SEGGER_RTT`, so the firmware's
    /// RTT log is read alongside.
    Semihosting {
        rtt: bool,
    },
}

/// How a capture transport attaches.
pub struct AttachOpts<'a> {
    pub elf: Option<&'a str>,
    /// How long RTT may take to come up (RTT transport only).
    pub rtt_timeout: Duration,
    /// Reset first, so the capture starts at the beginning of the run.
    pub reset: bool,
    /// The flash may hold another build than `elf`, so check it: before running
    /// an embedded-test suite, and on RTT when a stopped core keeps RTT from
    /// coming up. `false` right after flashing that same file, when it could
    /// only pass.
    pub verify_flash: bool,
    /// Unwind the stack for each failed embedded-test case.
    pub trace_failures: bool,
    /// Print every frame of those traces instead of the short form.
    pub full_traces: bool,
}

impl CaptureTransport {
    pub fn select(value: Option<&str>, elf: Option<&str>) -> Result<Self, String> {
        let value = value.unwrap_or("auto");
        if !matches!(value, "auto" | "rtt" | "semihosting") {
            return Err(format!(
                "Unknown transport '{value}'; use auto, rtt, or semihosting"
            ));
        }
        // Without an ELF, auto keeps the RTT RAM scan and an override is taken as given.
        let Some(path) = elf else {
            return Ok(if value == "semihosting" {
                Self::Semihosting { rtt: false }
            } else {
                Self::Rtt
            });
        };
        let data = std::fs::read(path).map_err(|e| format!("Cannot read ELF '{path}': {e}"))?;
        let inspect = |e: &dyn std::fmt::Display| format!("Cannot inspect ELF '{path}': {e}");
        // An embedded-test binary halts at its first semihosting request, before
        // `#[init]` sets up any logging, and waits there for a runner. Only the
        // semihosting transport runs it; its RTT log is read alongside.
        let embedded_test = super::semihosting::is_embedded_test(&data).map_err(|e| inspect(&e))?;
        let rtt = probe_rs::rtt::find_rtt_control_block_in_raw_file(&data)
            .map_err(|e| inspect(&e))?
            .is_some();
        match value {
            "rtt" if embedded_test => Err(format!(
                "transport=\"rtt\" cannot run '{path}': it is an embedded-test binary, which \
                 stops at a semihosting request before `#[init]` and waits for a test runner, \
                 so RTT never comes up. Use transport \"auto\" or \"semihosting\": that runs \
                 the suite and reads its RTT log alongside."
            )),
            "rtt" => Ok(Self::Rtt),
            "semihosting" => Ok(Self::Semihosting { rtt }),
            _ if embedded_test || !rtt => Ok(Self::Semihosting { rtt }),
            _ => Ok(Self::Rtt),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Rtt => "RTT",
            Self::Semihosting { rtt: false } => "semihosting",
            Self::Semihosting { rtt: true } => "semihosting + RTT",
        }
    }
    pub fn attach(self, session: Session, opts: AttachOpts) -> Result<Box<dyn ByteSource>, String> {
        let AttachOpts {
            elf,
            rtt_timeout,
            reset,
            verify_flash,
            trace_failures,
            full_traces,
        } = opts;
        match self {
            Self::Rtt if reset => Ok(Box::new(reset_and_attach_rtt(
                session,
                elf,
                rtt_timeout,
                verify_flash,
            )?)),
            Self::Rtt => Ok(Box::new(RttSource::attach(
                session,
                elf,
                rtt_timeout,
                verify_flash,
            )?)),
            Self::Semihosting { .. } => Ok(Box::new(SemihostingSource::attach(
                session,
                elf,
                reset,
                verify_flash,
                trace_failures,
                full_traces,
            )?)),
        }
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use object::write::{Object, Symbol, SymbolSection};
    use object::{
        Architecture, BinaryFormat, Endianness, SectionKind, SymbolFlags, SymbolKind, SymbolScope,
    };

    #[test]
    fn selection_uses_defined_rtt_symbol_and_respects_override() {
        let mut elf = Object::new(BinaryFormat::Elf, Architecture::Riscv32, Endianness::Little);
        let section = elf.add_section(vec![], b".bss".to_vec(), SectionKind::UninitializedData);
        elf.append_section_bss(section, 64, 4);
        let path =
            std::env::temp_dir().join(format!("flashprobe-transport-{}.elf", std::process::id()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Cleanup(path.clone());
        let path = path.to_str().unwrap();
        std::fs::write(path, elf.write().unwrap()).unwrap();
        assert_eq!(
            CaptureTransport::select(None, Some(path)).unwrap(),
            CaptureTransport::Semihosting { rtt: false }
        );
        assert_eq!(
            CaptureTransport::select(Some("rtt"), Some(path)).unwrap(),
            CaptureTransport::Rtt
        );
        elf.add_symbol(Symbol {
            name: b"_SEGGER_RTT".to_vec(),
            value: 0,
            size: 64,
            kind: SymbolKind::Data,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(section),
            flags: SymbolFlags::None,
        });
        std::fs::write(path, elf.write().unwrap()).unwrap();
        assert_eq!(
            CaptureTransport::select(Some("auto"), Some(path)).unwrap(),
            CaptureTransport::Rtt
        );
        assert_eq!(
            CaptureTransport::select(Some("semihosting"), Some(path)).unwrap(),
            CaptureTransport::Semihosting { rtt: true }
        );

        // An embedded-test ELF that also logs over RTT: the suite needs the
        // semihosting runner, and forcing RTT is refused with the reason.
        let tests = elf.add_section(
            vec![],
            b".embedded_test".to_vec(),
            SectionKind::ReadOnlyData,
        );
        elf.append_section_data(tests, &1u32.to_le_bytes(), 4);
        std::fs::write(path, elf.write().unwrap()).unwrap();
        assert_eq!(
            CaptureTransport::select(None, Some(path)).unwrap(),
            CaptureTransport::Semihosting { rtt: true }
        );
        let err = CaptureTransport::select(Some("rtt"), Some(path)).unwrap_err();
        assert!(err.contains("embedded-test"), "{err}");

        std::fs::write(path, b"invalid ELF").unwrap();
        assert!(CaptureTransport::select(None, Some(path)).is_err());
        assert!(CaptureTransport::select(Some("typo"), None).is_err());
        assert_eq!(
            CaptureTransport::select(None, None).unwrap(),
            CaptureTransport::Rtt
        );
    }
}
