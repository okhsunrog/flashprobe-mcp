//! Stack traces in the shape `probe-rs run` prints after a failure.
//!
//! A panic message alone often names the wrong place: `defmt::panic!` reports
//! a line inside defmt, and an embedded-test abort reports the panic handler.
//! The frames below those are what locate the failure, so failed tests and
//! panicking firmware get them the way `probe-rs run` shows them.

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

    /// Trace a halted core.
    pub fn trace(&mut self, core: &mut Core<'_>) -> Result<String, String> {
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
        Ok(format_frames(core.id(), &frames))
    }

    /// Trace core 0 of a target that may still be running: halt it for the
    /// unwind, then let it continue so a later capture sees it as it was.
    pub fn trace_session(&mut self, session: &mut Session) -> Result<String, String> {
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
        let trace = self.trace(&mut core);
        if was_running {
            core.run()
                .map_err(|e| format!("Failed to resume after the stack trace: {e}"))?;
        }
        trace
    }
}

fn format_frames(core: usize, frames: &[StackFrame]) -> String {
    let mut out = format!("Core {core}\n");
    for (i, frame) in frames.iter().enumerate() {
        let pc: u64 = frame.pc.try_into().unwrap_or(0);
        let _ = write!(out, "    Frame {i}: {} @ {pc:#x}", frame.function_name);
        if frame.is_inlined {
            out.push_str(" inline");
        }
        out.push('\n');
        if let Some(location) = &frame.source_location {
            let _ = write!(out, "        {}", location.path.to_path().display());
            if let Some(line) = location.line {
                let _ = write!(out, ":{line}");
                if let Some(ColumnType::Column(column)) = location.column {
                    let _ = write!(out, ":{column}");
                }
            }
            out.push('\n');
        }
    }
    if frames.len() >= FRAME_LIMIT {
        let _ = writeln!(out, "    (stopped after {FRAME_LIMIT} frames)");
    }
    out
}
