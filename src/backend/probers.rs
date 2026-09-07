//! probe-rs backend: JTAG/SWD flashing via the flashing API and a [`ByteSource`]
//! over RTT or semihosting, selected from the ELF or an explicit override.
//!
//! This entire module is gated behind the default-on `probe-rs` cargo feature;
//! the dep tree is large, so an espflash-only build can drop it with
//! `--no-default-features`.

use crate::capture::ByteSource;
use probe_rs::config::MemoryRegion;
use probe_rs::flashing::{
    ElfLoader, ElfOptions, FlashProgress, ImageLoader, download_file, erase, erase_all,
};
use probe_rs::probe::DebugProbeInfo;
use probe_rs::probe::list::Lister;
use probe_rs::rtt::{ChannelMode, Rtt, ScanRegion};
use probe_rs::{MemoryInterface, Permissions, Session};
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
/// selector, a single connected probe is used; multiple probes is an error that
/// asks the caller to disambiguate.
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
            .ok_or_else(|| format!("No probe matches '{sel}'. Connected: {}", list_str(&probes)))?,
        None if probes.len() == 1 => &probes[0],
        None => {
            return Err(format!(
                "Multiple probes connected; pass `probe` as VID:PID[:SERIAL]. Connected: {}",
                list_str(&probes)
            ));
        }
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
    let parts: Vec<&str> = sel.split(':').collect();
    if parts.len() < 2 {
        return false;
    }
    let vid = u16::from_str_radix(parts[0].trim_start_matches("0x"), 16);
    let pid = u16::from_str_radix(parts[1].trim_start_matches("0x"), 16);
    let (Ok(vid), Ok(pid)) = (vid, pid) else {
        return false;
    };
    if p.vendor_id != vid || p.product_id != pid {
        return false;
    }
    match parts.get(2) {
        Some(serial) => p.serial_number.as_deref() == Some(*serial),
        None => true,
    }
}

fn list_str(probes: &[DebugProbeInfo]) -> String {
    probes
        .iter()
        .map(|p| {
            let serial = p.serial_number.as_deref().unwrap_or("-");
            format!(
                "{:04x}:{:04x}:{serial} ({})",
                p.vendor_id, p.product_id, p.identifier
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
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
pub fn reset_and_attach_rtt(
    mut session: Session,
    elf_path: Option<&str>,
    attach_timeout: Duration,
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
}

impl RttSource {
    /// Attach RTT on an existing session. The firmware must already be running
    /// (just flashed/reset, or attached live) and built with an RTT transport.
    ///
    /// `elf_path` lets us pin the control block to the `_SEGGER_RTT` symbol
    /// ([`ScanRegion::Exact`]) — without it, a whole-RAM scan can find STALE
    /// control blocks left by previous firmware images in uninitialized RAM
    /// (CCMRAM/SRAM2 on STM32, etc.) and fail with "multiple control blocks".
    pub fn attach(
        mut session: Session,
        elf_path: Option<&str>,
        attach_timeout: Duration,
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
                    return Err(format!(
                        "Failed to attach RTT within {:.1}s (is the firmware running and \
                         built with an RTT transport?): {e}",
                        attach_timeout.as_secs_f32()
                    ));
                }
            }
        };
        Ok(Self {
            session,
            rtt,
            channel: RTT_UP_CHANNEL,
            restore_mode: None,
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
    Semihosting,
}

impl CaptureTransport {
    pub fn select(value: Option<&str>, elf: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("auto") {
            "rtt" => Ok(Self::Rtt),
            "semihosting" => Ok(Self::Semihosting),
            "auto" => {
                // Without an ELF retain the existing RTT RAM scan behavior.
                let Some(path) = elf else {
                    return Ok(Self::Rtt);
                };
                let data =
                    std::fs::read(path).map_err(|e| format!("Cannot read ELF '{path}': {e}"))?;
                let addr = probe_rs::rtt::find_rtt_control_block_in_raw_file(&data)
                    .map_err(|e| format!("Cannot inspect ELF '{path}': {e}"))?;
                Ok(if addr.is_some() {
                    Self::Rtt
                } else {
                    Self::Semihosting
                })
            }
            other => Err(format!(
                "Unknown transport '{other}'; use auto, rtt, or semihosting"
            )),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Rtt => "RTT",
            Self::Semihosting => "semihosting",
        }
    }
    pub fn attach(
        self,
        session: Session,
        elf: Option<&str>,
        timeout: Duration,
        reset: bool,
    ) -> Result<Box<dyn ByteSource>, String> {
        match self {
            Self::Rtt if reset => Ok(Box::new(reset_and_attach_rtt(session, elf, timeout)?)),
            Self::Rtt => Ok(Box::new(RttSource::attach(session, elf, timeout)?)),
            Self::Semihosting => Ok(Box::new(super::semihosting::SemihostingSource::attach(
                session, elf, reset,
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
            CaptureTransport::Semihosting
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
            CaptureTransport::Semihosting
        );
        std::fs::write(path, b"invalid ELF").unwrap();
        assert!(CaptureTransport::select(None, Some(path)).is_err());
        assert!(CaptureTransport::select(Some("typo"), None).is_err());
        assert_eq!(
            CaptureTransport::select(None, None).unwrap(),
            CaptureTransport::Rtt
        );
    }
}
