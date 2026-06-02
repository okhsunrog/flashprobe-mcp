//! Stateless project auto-detection. Each call derives what it needs from the
//! project on disk — no config file, no persisted state:
//!
//! - **elf**: the build artifact, located via `cargo metadata` (target dir + bin
//!   name) and the target triple from `.cargo/config.toml`.
//! - **chip**: parsed from the `--chip` argument of the `.cargo/config.toml`
//!   runner (set by esp-generate / probe-rs / espflash project templates).
//!
//! Ambiguity (multiple binaries) fails with guidance to pass `bin`, rather than
//! silently picking one.

use regex::Regex;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

#[derive(Deserialize)]
struct Metadata {
    target_directory: String,
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    targets: Vec<CargoTarget>,
}

#[derive(Deserialize)]
struct CargoTarget {
    name: String,
    kind: Vec<String>,
}

/// Resolves project fields on demand, letting an explicit value win over the
/// detected one. Chip and ELF are detected independently: the chip comes from
/// `.cargo/config.toml` (no `cargo metadata`, no `bin` needed), while the ELF
/// needs the build artifact (which does need `bin` to disambiguate). Each is
/// computed at most once. Tools create one per call.
pub struct Detector<'a> {
    project_dir: Option<&'a str>,
    bin: Option<&'a str>,
    elf_cache: Option<Result<PathBuf, String>>,
    chip_cache: Option<Option<String>>,
}

impl<'a> Detector<'a> {
    pub fn new(project_dir: Option<&'a str>, bin: Option<&'a str>) -> Self {
        Self {
            project_dir,
            bin,
            elf_cache: None,
            chip_cache: None,
        }
    }

    fn dir(&self) -> PathBuf {
        match self.project_dir {
            Some(d) => PathBuf::from(d),
            None => std::env::current_dir().unwrap_or_default(),
        }
    }

    fn elf_result(&mut self) -> &Result<PathBuf, String> {
        if self.elf_cache.is_none() {
            self.elf_cache = Some(detect_elf(self.project_dir, self.bin));
        }
        self.elf_cache.as_ref().unwrap()
    }

    /// The file to flash / ELF: explicit path wins, else the detected artifact.
    pub fn elf(&mut self, explicit: Option<&str>) -> Result<String, String> {
        if let Some(e) = explicit {
            return Ok(e.to_string());
        }
        match self.elf_result() {
            Ok(p) => Ok(p.display().to_string()),
            Err(e) => Err(e.clone()),
        }
    }

    /// Optional ELF for defmt decode: explicit wins, else the detected artifact
    /// if detection succeeds; `None` (→ text mode) if it can't be found.
    pub fn elf_opt(&mut self, explicit: Option<&str>) -> Option<String> {
        if let Some(e) = explicit {
            return Some(e.to_string());
        }
        self.elf_result()
            .as_ref()
            .ok()
            .map(|p| p.display().to_string())
    }

    /// Chip/target name: explicit wins, else the `--chip` from the config runner.
    /// Does not need `cargo metadata` or `bin`. Only the probe-rs backend uses it.
    #[cfg_attr(not(feature = "probe-rs"), allow(dead_code))]
    pub fn chip(&mut self, explicit: Option<&str>) -> Result<String, String> {
        if let Some(c) = explicit {
            return Ok(c.to_string());
        }
        if self.chip_cache.is_none() {
            self.chip_cache = Some(read_cargo_config(&self.dir()).1);
        }
        self.chip_cache.clone().flatten().ok_or_else(|| {
            "could not auto-detect `chip` (no --chip in .cargo/config.toml); pass it \
             explicitly, e.g. \"esp32c6\""
                .to_string()
        })
    }
}

/// Locate the project's build artifact ELF. `project_dir` defaults to the
/// process cwd; `bin` disambiguates a multi-binary workspace.
pub fn detect_elf(project_dir: Option<&str>, bin: Option<&str>) -> Result<PathBuf, String> {
    let dir = match project_dir {
        Some(d) => PathBuf::from(d),
        None => {
            std::env::current_dir().map_err(|e| format!("Failed to get current directory: {e}"))?
        }
    };

    let meta = cargo_metadata(&dir)?;
    let bin_name = pick_bin(&meta, bin)?;
    let (triple, _chip) = read_cargo_config(&dir);
    locate_artifact(
        Path::new(&meta.target_directory),
        triple.as_deref(),
        &bin_name,
    )
}

fn cargo_metadata(dir: &Path) -> Result<Metadata, String> {
    let out = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(dir)
        .output()
        .map_err(|e| format!("Failed to run `cargo metadata` (is cargo on PATH?): {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`cargo metadata` failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("Failed to parse cargo metadata: {e}"))
}

/// Choose the binary target: the explicit `bin`, else the sole bin, else error.
fn pick_bin(meta: &Metadata, bin: Option<&str>) -> Result<String, String> {
    let bins: Vec<&str> = meta
        .packages
        .iter()
        .flat_map(|p| &p.targets)
        .filter(|t| t.kind.iter().any(|k| k == "bin"))
        .map(|t| t.name.as_str())
        .collect();

    match bin {
        Some(b) if bins.contains(&b) => Ok(b.to_string()),
        Some(b) => Err(format!(
            "binary '{b}' not found; available: {}",
            bins.join(", ")
        )),
        None => match bins.as_slice() {
            [one] => Ok((*one).to_string()),
            [] => Err("no `[[bin]]` targets found in the project".to_string()),
            many => Err(format!(
                "multiple binaries ({}); pass `bin` to choose one",
                many.join(", ")
            )),
        },
    }
}

/// Pick the build artifact at `target/<triple>/release/<bin>`. Only the release
/// profile is considered: a debug build is essentially never what gets flashed on
/// embedded (and usually won't fit in flash). Pass an explicit path to flash
/// anything else (e.g. a debug build).
fn locate_artifact(target_dir: &Path, triple: Option<&str>, bin: &str) -> Result<PathBuf, String> {
    let base = match triple {
        Some(t) => target_dir.join(t),
        None => target_dir.to_path_buf(),
    };
    let artifact = base.join("release").join(bin);
    if artifact.is_file() {
        Ok(artifact)
    } else {
        Err(format!(
            "no release build artifact for '{bin}' at {} (run `cargo build --release` first, or pass an explicit path)",
            artifact.display()
        ))
    }
}

/// `--chip esp32c6` or `--chip=esp32c6`.
static CHIP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"--chip[=\s]+([A-Za-z0-9_]+)").unwrap());

/// Read `.cargo/config.toml` for the build target triple and the chip name (from
/// the runner's `--chip` argument). Both optional; missing config → `(None, None)`.
fn read_cargo_config(dir: &Path) -> (Option<String>, Option<String>) {
    let content = ["config.toml", "config"]
        .iter()
        .map(|name| dir.join(".cargo").join(name))
        .find_map(|p| std::fs::read_to_string(p).ok());
    let Some(content) = content else {
        return (None, None);
    };
    // NB: toml 1.x `str::parse::<Value>()` parses a single value, not a
    // document — use `from_str` to deserialize the whole config table.
    let Ok(value) = toml::from_str::<toml::Value>(&content) else {
        return (None, None);
    };

    let triple = value
        .get("build")
        .and_then(|b| b.get("target"))
        .and_then(|t| t.as_str())
        .map(String::from);

    // Scan every [target.<triple>] section's runner for a --chip argument.
    let chip = value
        .get("target")
        .and_then(|t| t.as_table())
        .and_then(|sections| {
            sections.values().find_map(|sec| {
                sec.get("runner")
                    .and_then(|r| r.as_str())
                    .and_then(|r| CHIP_RE.captures(r))
                    .map(|c| c[1].to_string())
            })
        });

    (triple, chip)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_build_target_and_chip() {
        let cfg = "[target.riscv32imac-unknown-none-elf]\nrunner = \"probe-rs run --chip=esp32c6\"\n[build]\nrustflags = [\"-C\", \"x\"]\ntarget = \"riscv32imac-unknown-none-elf\"\n";
        let v: toml::Value = toml::from_str(cfg).expect("parse");
        let triple = v
            .get("build")
            .and_then(|b| b.get("target"))
            .and_then(|t| t.as_str());
        let chip = v.get("target").and_then(|t| t.as_table()).and_then(|s| {
            s.values().find_map(|sec| {
                sec.get("runner")
                    .and_then(|r| r.as_str())
                    .and_then(|r| super::CHIP_RE.captures(r))
                    .map(|c| c[1].to_string())
            })
        });
        assert_eq!(triple, Some("riscv32imac-unknown-none-elf"), "triple");
        assert_eq!(chip.as_deref(), Some("esp32c6"), "chip");
    }
}
