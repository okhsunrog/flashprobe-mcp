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

/// What we could derive about the project.
pub struct Detected {
    /// Path to the build artifact ELF.
    pub elf: PathBuf,
    /// Chip/target name from the runner config, if found. Used by probe-rs only.
    #[cfg_attr(not(feature = "probe-rs"), allow(dead_code))]
    pub chip: Option<String>,
}

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

/// Runs project detection at most once and resolves individual fields, letting
/// an explicit value win over the detected one. Tools create one per call.
pub struct Detector<'a> {
    project_dir: Option<&'a str>,
    bin: Option<&'a str>,
    cache: Option<Result<Detected, String>>,
}

impl<'a> Detector<'a> {
    pub fn new(project_dir: Option<&'a str>, bin: Option<&'a str>) -> Self {
        Self {
            project_dir,
            bin,
            cache: None,
        }
    }

    fn get(&mut self) -> &Result<Detected, String> {
        if self.cache.is_none() {
            self.cache = Some(detect_project(self.project_dir, self.bin));
        }
        self.cache.as_ref().unwrap()
    }

    /// The file to flash / ELF: explicit path wins, else the detected artifact.
    /// Errors (with detection guidance) if neither is available.
    pub fn elf(&mut self, explicit: Option<&str>) -> Result<String, String> {
        if let Some(e) = explicit {
            return Ok(e.to_string());
        }
        match self.get() {
            Ok(d) => Ok(d.elf.display().to_string()),
            Err(e) => Err(e.clone()),
        }
    }

    /// Optional ELF for defmt decode: explicit wins, else the detected artifact
    /// if detection succeeds; `None` (→ text mode) if it can't be found.
    pub fn elf_opt(&mut self, explicit: Option<&str>) -> Option<String> {
        if let Some(e) = explicit {
            return Some(e.to_string());
        }
        match self.get() {
            Ok(d) => Some(d.elf.display().to_string()),
            Err(_) => None,
        }
    }

    /// Chip/target name: explicit wins, else the detected chip. Errors if neither.
    /// Only the probe-rs backend needs a chip name.
    #[cfg_attr(not(feature = "probe-rs"), allow(dead_code))]
    pub fn chip(&mut self, explicit: Option<&str>) -> Result<String, String> {
        if let Some(c) = explicit {
            return Ok(c.to_string());
        }
        match self.get() {
            Ok(d) => d.chip.clone().ok_or_else(|| {
                "could not auto-detect `chip` (no --chip in .cargo/config.toml); pass it \
                 explicitly, e.g. \"esp32c6\""
                    .to_string()
            }),
            Err(e) => Err(e.clone()),
        }
    }
}

/// Resolve the project's build artifact and chip. `project_dir` defaults to the
/// process cwd; `bin` disambiguates a multi-binary workspace.
pub fn detect_project(project_dir: Option<&str>, bin: Option<&str>) -> Result<Detected, String> {
    let dir = match project_dir {
        Some(d) => PathBuf::from(d),
        None => std::env::current_dir()
            .map_err(|e| format!("Failed to get current directory: {e}"))?,
    };

    let meta = cargo_metadata(&dir)?;
    let bin_name = pick_bin(&meta, bin)?;
    let (triple, chip) = read_cargo_config(&dir);
    let elf = locate_artifact(Path::new(&meta.target_directory), triple.as_deref(), &bin_name)?;

    Ok(Detected { elf, chip })
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

/// Pick the build artifact under `target/<triple>/{release,debug}/<bin>`,
/// preferring the most recently built.
fn locate_artifact(target_dir: &Path, triple: Option<&str>, bin: &str) -> Result<PathBuf, String> {
    let base = match triple {
        Some(t) => target_dir.join(t),
        None => target_dir.to_path_buf(),
    };
    let newest = ["release", "debug"]
        .iter()
        .map(|profile| base.join(profile).join(bin))
        .filter(|p| p.is_file())
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());

    newest.ok_or_else(|| {
        format!(
            "no build artifact for '{bin}' under {} (run `cargo build` first, or pass an explicit path)",
            base.display()
        )
    })
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
        let triple = v.get("build").and_then(|b| b.get("target")).and_then(|t| t.as_str());
        let chip = v.get("target").and_then(|t| t.as_table()).and_then(|s| {
            s.values().find_map(|sec| sec.get("runner").and_then(|r| r.as_str()).and_then(|r| super::CHIP_RE.captures(r)).map(|c| c[1].to_string()))
        });
        assert_eq!(triple, Some("riscv32imac-unknown-none-elf"), "triple");
        assert_eq!(chip.as_deref(), Some("esp32c6"), "chip");
    }
}
