//! Device backends. Currently espflash (serial); probe-rs is added in a later
//! milestone behind a default-on `probe-rs` cargo feature.

pub mod espflash;
#[cfg(feature = "probe-rs")]
pub mod probers;

/// Which backend a tool call should use.
pub enum BackendKind {
    Espflash,
    #[cfg(feature = "probe-rs")]
    ProbeRs,
}

/// Resolve the `backend` argument. It is required and explicit: both backends
/// work on ESP chips and the right one depends on where the firmware emits
/// output (UART vs RTT), which can't be inferred reliably — so the tool asks
/// rather than silently picking the wrong transport (and showing no logs).
pub fn parse_backend(s: Option<&str>) -> Result<BackendKind, String> {
    match s.map(str::to_ascii_lowercase).as_deref() {
        None => Err(
            "`backend` is required: \"probe-rs\" (JTAG/SWD + RTT) or \"espflash\" (UART). \
             Pick probe-rs for RTT/defmt-rtt firmware, espflash for UART/esp-println."
                .to_string(),
        ),
        Some("espflash") => Ok(BackendKind::Espflash),
        Some("probe-rs" | "probers" | "probe_rs") => {
            #[cfg(feature = "probe-rs")]
            {
                Ok(BackendKind::ProbeRs)
            }
            #[cfg(not(feature = "probe-rs"))]
            {
                Err("probe-rs backend not built; recompile with the `probe-rs` feature".into())
            }
        }
        Some(other) => Err(format!(
            "Unknown backend '{other}' (use 'espflash' or 'probe-rs')"
        )),
    }
}
