//! probe-rs backend: JTAG/SWD flashing via the flashing API and a [`ByteSource`]
//! over an RTT up-channel. Text mode only for now (defmt decode arrives later).
//!
//! This entire module is gated behind the default-on `probe-rs` cargo feature;
//! the dep tree is large, so an espflash-only build can drop it with
//! `--no-default-features`.

use crate::capture::ByteSource;
use probe_rs::config::MemoryRegion;
use probe_rs::flashing::{
    ElfOptions, FlashProgress, Format, IdfOptions, download_file, erase, erase_all,
};
use probe_rs::probe::DebugProbeInfo;
use probe_rs::probe::list::Lister;
use probe_rs::rtt::Rtt;
use probe_rs::{MemoryInterface, Permissions, Session};
use std::time::Duration;

/// Default RTT up-channel to read (channel 0 is the conventional terminal).
const RTT_UP_CHANNEL: usize = 0;

/// Open a session to `chip` through a connected probe. `probe_sel` optionally
/// selects a probe by `VID:PID` or `VID:PID:SERIAL` (hex VID/PID). With no
/// selector, a single connected probe is used; multiple probes is an error that
/// asks the caller to disambiguate.
pub fn open_session(chip: &str, probe_sel: Option<&str>) -> Result<Session, String> {
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
fn format_for_chip(chip: &str) -> Format {
    if chip.to_ascii_lowercase().starts_with("esp") {
        Format::Idf(IdfOptions::default())
    } else {
        Format::Elf(ElfOptions::default())
    }
}

/// Download `path` to the device, then reset-and-run so the firmware executes.
/// Returns a human-readable summary line.
pub fn flash(session: &mut Session, path: &str, chip: &str) -> Result<String, String> {
    download_file(session, path, format_for_chip(chip))
        .map_err(|e| format!("probe-rs flash failed: {e}"))?;
    reset(session)?;
    Ok(format!("Flashed {path} to {chip} via probe-rs (JTAG/SWD)"))
}

/// Reset-and-run the firmware already on the device (the probe-rs equivalent of
/// `rerun`'s DTR/RTS reset). After reset we briefly wait so the firmware can
/// re-initialize its RTT control block before a subsequent `RttSource::attach`
/// reads it — otherwise the attach can latch onto the stale pre-reset block and
/// return uninitialized buffer contents.
pub fn reset(session: &mut Session) -> Result<(), String> {
    session
        .core(0)
        .map_err(|e| format!("Failed to access core: {e}"))?
        .reset()
        .map_err(|e| format!("Failed to reset device: {e}"))?;
    std::thread::sleep(Duration::from_millis(200));
    Ok(())
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
}

impl RttSource {
    /// Attach RTT on an existing session. The firmware must already be running
    /// (just flashed/reset, or attached live) and built with an RTT transport.
    pub fn attach(mut session: Session) -> Result<Self, String> {
        let rtt = {
            let mut core = session
                .core(0)
                .map_err(|e| format!("Failed to access core: {e}"))?;
            Rtt::attach(&mut core).map_err(|e| {
                format!(
                    "Failed to attach RTT (is the firmware running and built with an RTT \
                     transport?): {e}"
                )
            })?
        };
        Ok(Self {
            session,
            rtt,
            channel: RTT_UP_CHANNEL,
        })
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

    fn idle_nap(&self) -> Duration {
        Duration::from_millis(10)
    }
}
