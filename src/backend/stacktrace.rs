//! Stack traces in the shape `probe-rs run` prints after a failure.
//!
//! A panic message alone often names the wrong place: `defmt::panic!` reports
//! a line inside defmt, and an embedded-test abort reports the panic handler.
//! The frames below those are what locate the failure, so failed tests and
//! panicking firmware get them the way `probe-rs run` shows them.
//!
//! By default the trace is short. Most frames of an embedded Rust stack are
//! the panic machinery, the executor and the startup code, and their names
//! carry whole future types (`Executor::run_inner<…Join5<…>…>`), so a full
//! trace of a single test ran to kilobytes. The short form keeps the project's
//! own frames and folds the rest into one line per run of dependency frames.

use probe_rs::{Core, CoreStatus, Session};
use probe_rs_debug::{
    ColumnType, DebugInfo, DebugRegisters, StackFrame, exception_handler_for_core,
};
use std::fmt::Write;
use std::time::Duration;

/// Frames to unwind before giving up. `probe-rs run` defaults to 500, which a
/// runaway recursion turns into a thousand lines of output; the frames that
/// matter are at the top.
const FRAME_LIMIT: usize = 100;

/// Function names listed for the dependency frames at the top of the stack.
const TOP_NAMES: usize = 12;

/// Unwinds with the ELF's DWARF, parsed on first use and then reused: one
/// suite can fail several tests, and a large debug build takes a moment to
/// parse.
pub struct StackTracer {
    elf: String,
    debug_info: Option<DebugInfo>,
}

impl StackTracer {
    pub fn new(elf: &str) -> Self {
        Self {
            elf: elf.to_string(),
            debug_info: None,
        }
    }

    /// Trace a halted core. `full`: every frame with its full name, exactly
    /// as `probe-rs run` prints it, instead of the short form.
    pub fn trace(&mut self, core: &mut Core<'_>, full: bool) -> Result<String, String> {
        if self.debug_info.is_none() {
            let info = DebugInfo::from_file(&self.elf)
                .map_err(|e| format!("Cannot read debug info from '{}': {e}", self.elf))?;
            self.debug_info = Some(info);
        }
        let debug_info = self.debug_info.as_ref().expect("parsed above");
        let registers = DebugRegisters::from_core(core);
        let exceptions = exception_handler_for_core(core.core_type());
        let instruction_set = core.instruction_set().ok();
        let frames = debug_info
            .unwind(
                core,
                registers,
                exceptions.as_ref(),
                instruction_set,
                FRAME_LIMIT,
            )
            .map_err(|e| format!("Stack unwind failed: {e}"))?;
        let frames: Vec<Frame> = frames.iter().map(Frame::from).collect();
        Ok(if full {
            format_full(core.id(), &frames)
        } else {
            format_short(core.id(), &frames)
        })
    }

    /// Trace core 0 of a target that may still be running: halt it for the
    /// unwind, then let it continue so a later capture sees it as it was.
    pub fn trace_session(&mut self, session: &mut Session, full: bool) -> Result<String, String> {
        let mut core = session
            .core(0)
            .map_err(|e| format!("Failed to access core: {e}"))?;
        let was_running = !matches!(
            core.status()
                .map_err(|e| format!("Failed to read core status: {e}"))?,
            CoreStatus::Halted(_)
        );
        if was_running {
            core.halt(Duration::from_millis(500))
                .map_err(|e| format!("Failed to halt the core for a stack trace: {e}"))?;
        }
        let trace = self.trace(&mut core, full);
        if was_running {
            core.run()
                .map_err(|e| format!("Failed to resume after the stack trace: {e}"))?;
        }
        trace
    }
}

/// The parts of a [`StackFrame`] a trace prints.
struct Frame {
    function: String,
    pc: u64,
    inlined: bool,
    /// `path:line:column`, as far as it is known.
    location: Option<String>,
}

impl From<&StackFrame> for Frame {
    fn from(frame: &StackFrame) -> Self {
        let location = frame.source_location.as_ref().map(|location| {
            let mut text = location.path.to_path().display().to_string();
            if let Some(line) = location.line {
                let _ = write!(text, ":{line}");
                if let Some(ColumnType::Column(column)) = location.column {
                    let _ = write!(text, ":{column}");
                }
            }
            text
        });
        Self {
            function: frame.function_name.clone(),
            pc: frame.pc.try_into().unwrap_or(0),
            inlined: frame.is_inlined,
            location,
        }
    }
}

impl Frame {
    /// The crate a frame's code comes from, when that is a dependency rather
    /// than the project: a crates.io or git crate, or the standard library.
    /// Judged by where cargo and rustup keep those sources; path dependencies
    /// are the project's own code and stay visible.
    fn dependency(&self) -> Option<String> {
        let Some(path) = &self.location else {
            return Some("no source".into());
        };
        let after = |marker: &str| path.split_once(marker).map(|(_, rest)| rest);
        if let Some(rest) = after("/.cargo/registry/src/") {
            // index.crates.io-<hash>/<crate>-<version>/src/...
            let dir = rest.split('/').nth(1)?;
            return Some(strip_version(dir).to_string());
        }
        if let Some(rest) = after("/.cargo/git/checkouts/") {
            // <repo>-<hash>/<rev>/<crate dir>/src/..., or <repo>-<hash>/<rev>/src/...
            let mut parts = rest.split('/');
            let repo = parts.next()?;
            let dir = parts.nth(1)?;
            return Some(if dir == "src" {
                repo.rsplit_once('-')
                    .map_or(repo, |(name, _)| name)
                    .to_string()
            } else {
                dir.to_string()
            });
        }
        // The standard library: rust-src from rustup, or the remapped
        // `/rustc/<commit>/library/...` paths of a toolchain without it.
        let std = after("/rustlib/src/rust/library/").or_else(|| {
            path.starts_with("/rustc/")
                .then(|| after("/library/"))
                .flatten()
        });
        std.and_then(|rest| rest.split('/').next())
            .map(str::to_string)
    }
}

/// `semihosting-0.1.25` → `semihosting`.
fn strip_version(dir: &str) -> &str {
    dir.rsplit_once('-')
        .filter(|(_, version)| version.starts_with(|c: char| c.is_ascii_digit()))
        .map_or(dir, |(name, _)| name)
}

/// Replace every generic argument list with `<…>`: `run_inner<Join5<…>, …>`
/// becomes `run_inner<…>`. A `->` inside the arguments is not a bracket.
fn trim_generics(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut depth = 0usize;
    let mut previous = ' ';
    for c in name.chars() {
        match c {
            '<' => {
                if depth == 0 {
                    out.push_str("<…");
                }
                depth += 1;
            }
            '>' if depth > 0 && previous != '-' => {
                depth -= 1;
                if depth == 0 {
                    out.push('>');
                }
            }
            _ if depth == 0 => out.push(c),
            _ => {}
        }
        previous = c;
    }
    out
}

fn write_frame(out: &mut String, index: usize, frame: &Frame, name: &str) {
    let _ = write!(out, "    Frame {index}: {name} @ {:#x}", frame.pc);
    if frame.inlined {
        out.push_str(" inline");
    }
    out.push('\n');
    if let Some(location) = &frame.location {
        let _ = writeln!(out, "        {location}");
    }
}

fn write_limit_note(out: &mut String, frames: &[Frame]) {
    if frames.len() >= FRAME_LIMIT {
        let _ = writeln!(out, "    (stopped after {FRAME_LIMIT} frames)");
    }
}

/// Every frame, as `probe-rs run` prints it.
fn format_full(core: usize, frames: &[Frame]) -> String {
    let mut out = format!("Core {core}\n");
    for (i, frame) in frames.iter().enumerate() {
        write_frame(&mut out, i, frame, &frame.function);
    }
    write_limit_note(&mut out, frames);
    out
}

/// The project's frames, with each run of dependency frames folded into one
/// line naming its crates. The run at the top of the stack also lists its
/// functions, since they say what the core was doing when it stopped: in a
/// panic's abort, or idle waiting for an interrupt.
fn format_short(core: usize, frames: &[Frame]) -> String {
    let mut out = format!(
        "Core {core} ({} frames; dependency frames folded, generics trimmed; \
         stacktrace_full: true shows them all)\n",
        frames.len()
    );
    let mut i = 0;
    while i < frames.len() {
        if frames[i].dependency().is_none() {
            write_frame(&mut out, i, &frames[i], &trim_generics(&frames[i].function));
            i += 1;
            continue;
        }
        let start = i;
        let mut crates: Vec<String> = Vec::new();
        while let Some(krate) = frames.get(i).and_then(Frame::dependency) {
            if !crates.contains(&krate) {
                crates.push(krate);
            }
            i += 1;
        }
        let span = if i - start == 1 {
            format!("Frame {start}")
        } else {
            format!("Frames {start}-{}", i - 1)
        };
        let _ = write!(out, "    {span} ({})", crates.join(", "));
        if start == 0 {
            let names: Vec<String> = frames[start..i]
                .iter()
                .take(TOP_NAMES)
                .map(|f| trim_generics(&f.function))
                .collect();
            let _ = write!(out, ": {}", names.join(" ← "));
            if i - start > TOP_NAMES {
                let _ = write!(out, " ← … {} more", i - start - TOP_NAMES);
            }
        }
        out.push('\n');
    }
    write_limit_note(&mut out, frames);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = "/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f";
    const GIT: &str = "/home/u/.cargo/git/checkouts/esp-hal-29ef8683203c9b81/0e9fe8d";
    const RUSTLIB: &str =
        "/home/u/.rustup/toolchains/nightly-x86_64-unknown-linux-gnu/lib/rustlib/src/rust/library";

    fn frame(function: &str, location: Option<&str>) -> Frame {
        Frame {
            function: function.into(),
            pc: 0x4200_0000,
            inlined: false,
            location: location.map(str::to_string),
        }
    }

    #[test]
    fn dependencies_are_told_apart_from_project_code() {
        let dep = |path: &str| frame("f", Some(path)).dependency();
        assert_eq!(
            dep(&format!(
                "{REGISTRY}/semihosting-0.1.25/src/process.rs:83:5"
            ))
            .as_deref(),
            Some("semihosting")
        );
        assert_eq!(
            dep(&format!(
                "{REGISTRY}/embassy-executor-0.10.0/src/raw/mod.rs:1"
            ))
            .as_deref(),
            Some("embassy-executor")
        );
        assert_eq!(
            dep(&format!("{GIT}/esp-rtos/src/embassy/mod.rs:293:31")).as_deref(),
            Some("esp-rtos")
        );
        assert_eq!(
            dep(&format!("{RUSTLIB}/core/src/panicking.rs:80:14")).as_deref(),
            Some("core")
        );
        assert_eq!(
            dep("/rustc/0123abcd/library/core/src/option.rs:2021:5").as_deref(),
            Some("core")
        );
        assert_eq!(frame("f", None).dependency().as_deref(), Some("no source"));
        // The project and its path dependencies.
        assert_eq!(dep("/home/u/code/app/tests/ergot_can.rs:200:5"), None);
        assert_eq!(dep("/home/u/code/ergot/crates/ergot/src/lib.rs:10"), None);
    }

    #[test]
    fn generic_arguments_are_trimmed() {
        assert_eq!(
            trim_generics(
                "Executor::run_inner<ergot_can::tests::__can_fd_entrypoint::{closure_env#0}, \
                 esp_rtos::embassy::{impl#5}::run::NoHooks>"
            ),
            "Executor::run_inner<…>"
        );
        assert_eq!(
            trim_generics("TaskStorage<Join5<A<B>, C>>::poll"),
            "TaskStorage<…>::poll"
        );
        assert_eq!(trim_generics("call<fn() -> u32>"), "call<…>");
        assert_eq!(trim_generics("{async_fn#0}"), "{async_fn#0}");
    }

    #[test]
    fn short_trace_keeps_project_frames_and_folds_the_rest() {
        let frames = [
            frame(
                "abort",
                Some(&format!("{REGISTRY}/semihosting-0.1.25/src/p.rs:83")),
            ),
            frame(
                "panic_fmt",
                Some(&format!("{RUSTLIB}/core/src/panicking.rs:80")),
            ),
            frame("check_value", Some("/home/u/app/tests/t.rs:11:9")),
            frame(
                "TaskStorage<Join5<A, B>>::poll",
                Some(&format!(
                    "{REGISTRY}/embassy-executor-0.10.0/src/raw/mod.rs:253"
                )),
            ),
            frame(
                "run<X>",
                Some(&format!("{GIT}/esp-rtos/src/embassy/mod.rs:234")),
            ),
            frame("__entrypoint", Some("/home/u/app/tests/t.rs:15:1")),
            frame(".Lpcrel_hi61", None),
        ];
        let short = format_short(0, &frames);
        assert_eq!(
            short,
            "Core 0 (7 frames; dependency frames folded, generics trimmed; \
             stacktrace_full: true shows them all)\n\
             \x20   Frames 0-1 (semihosting, core): abort ← panic_fmt\n\
             \x20   Frame 2: check_value @ 0x42000000\n\
             \x20       /home/u/app/tests/t.rs:11:9\n\
             \x20   Frames 3-4 (embassy-executor, esp-rtos)\n\
             \x20   Frame 5: __entrypoint @ 0x42000000\n\
             \x20       /home/u/app/tests/t.rs:15:1\n\
             \x20   Frame 6 (no source)\n"
        );
        let full = format_full(0, &frames);
        assert!(full.contains("Frame 3: TaskStorage<Join5<A, B>>::poll @ 0x42000000"));
        assert!(full.starts_with("Core 0\n    Frame 0: abort @ 0x42000000\n"));
    }
}
