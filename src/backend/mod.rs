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

/// Resolve the `backend` argument. Defaults to espflash. Selecting probe-rs when
/// the feature was compiled out is a clear error rather than a silent fallback.
pub fn parse_backend(s: Option<&str>) -> Result<BackendKind, String> {
    match s.map(str::to_ascii_lowercase).as_deref() {
        None | Some("espflash") => Ok(BackendKind::Espflash),
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
