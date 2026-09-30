//! Semihosting console capture and the embedded-test host protocol, with the
//! firmware's RTT log read alongside. Syscall decoding, target buffers and
//! responses are owned by probe-rs.
use crate::backend::stacktrace::StackTracer;
use crate::capture::{ByteSource, Stream};
use anyhow::{Context, Result, bail};
use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};
use probe_rs::rtt::{ChannelMode, Rtt, ScanRegion};
use probe_rs::{
    BreakpointCause, Core, CoreStatus, HaltReason, MemoryInterface, Session,
    semihosting::SemihostingCommand,
};
use serde::Deserialize;
use std::{
    collections::VecDeque,
    num::NonZeroU32,
    time::{Duration, Instant},
};

/// Per-test timeout when the test declares none, as in embedded-test.
const DEFAULT_TEST_TIMEOUT_S: u32 = 60;

#[derive(Debug, Deserialize)]
struct Test {
    name: String,
    should_panic: bool,
    ignored: bool,
    timeout: Option<u32>,
    #[serde(skip)]
    address: Option<u32>,
}

impl Test {
    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout.unwrap_or(DEFAULT_TEST_TIMEOUT_S) as u64)
    }
}

/// Whether the ELF is an embedded-test binary. Such firmware stops at a
/// semihosting request before its `#[init]` and waits for a runner, so it
/// cannot be captured by only reading RTT.
pub fn is_embedded_test(data: &[u8]) -> Result<bool> {
    Ok(object::File::parse(data)?
        .section_by_name(".embedded_test")
        .is_some())
}

/// None means ordinary firmware; Some includes an empty embedded-test suite.
fn tests_from_elf(data: &[u8]) -> Result<Option<Vec<Test>>> {
    let elf = object::File::parse(data)?;
    let Some(section) = elf.section_by_name(".embedded_test") else {
        return Ok(None);
    };
    let version = elf
        .symbol_by_name("EMBEDDED_TEST_VERSION")
        .context(".embedded_test is missing EMBEDDED_TEST_VERSION")?;
    let bytes = section
        .data_range(version.address(), 4)?
        .context("missing test version data")?;
    let version = u32::from_le_bytes(bytes.try_into()?);
    // Version 0 requires listing tests on the target; do not silently run it as an app.
    if version != 1 {
        bail!("Unsupported embedded-test protocol {version}; supported: 1 (embedded-test >= 0.7)");
    }
    let mut tests = Vec::new();
    for symbol in elf.symbols() {
        if !symbol.is_global()
            || symbol.kind() != SymbolKind::Data
            || symbol.section_index() != Some(section.index())
            || symbol.size() != 12
        {
            continue;
        }
        let bytes = section
            .data_range(symbol.address(), 12)?
            .context("missing test metadata")?;
        let address = u32::from_le_bytes(bytes[0..4].try_into()?);
        let path_addr = u32::from_le_bytes(bytes[4..8].try_into()?) as u64;
        let path_len = u32::from_le_bytes(bytes[8..12].try_into()?) as u64;
        let path_section = elf
            .sections()
            .find(|s| {
                path_addr >= s.address()
                    && path_addr
                        .checked_add(path_len)
                        .is_some_and(|end| end <= s.address().saturating_add(s.size()))
            })
            .context("test module path outside ELF sections")?;
        let path = std::str::from_utf8(
            path_section
                .data_range(path_addr, path_len)?
                .context("missing module path")?,
        )?;
        let path = path.split_once("::").map_or(path, |(_, rest)| rest);
        let mut test: Test = serde_json::from_str(symbol.name()?)?;
        test.name = format!("{path}::{}", test.name);
        test.address = Some(address);
        tests.push(test);
    }
    tests.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Some(tests))
}

struct Suite {
    tests: Vec<Test>,
    index: usize,
    passed: usize,
    failed: usize,
    ignored: usize,
    /// When the whole run began, for the summary.
    began: Instant,
    /// When the current test's reset happened; its timeout counts from here.
    started: Instant,
    commanded: bool,
    next: bool,
}

impl Suite {
    fn new(tests: Vec<Test>) -> Self {
        Self {
            tests,
            index: 0,
            passed: 0,
            failed: 0,
            ignored: 0,
            began: Instant::now(),
            started: Instant::now(),
            commanded: false,
            next: true,
        }
    }

    /// Why the current test failed, given how it ended; `None` if it passed.
    fn failure(&self, panic: bool, timeout: bool) -> Option<&'static str> {
        let test = &self.tests[self.index];
        if timeout {
            Some("test timeout")
        } else if !self.commanded {
            Some("exited before the runner sent it a test")
        } else if panic && !test.should_panic {
            // embedded-test aborts both on a panic and on an `Err` return;
            // the firmware log above says which.
            Some("panicked or returned Err")
        } else if !panic && test.should_panic {
            Some("expected a panic, but the test passed")
        } else {
            None
        }
    }

    fn result(&mut self, failure: Option<&str>) -> String {
        let test = &self.tests[self.index];
        let line = match failure {
            None => {
                self.passed += 1;
                format!("test {} ... ok\n", test.name)
            }
            Some(why) => {
                self.failed += 1;
                format!("test {} ... FAILED ({why})\n", test.name)
            }
        };
        self.index += 1;
        self.next = true;
        line
    }

    fn summary(&self) -> String {
        format!(
            "test result: {}. {} passed; {} failed; {} ignored; finished in {:.2}s\n",
            if self.failed == 0 { "ok" } else { "FAILED" },
            self.passed,
            self.failed,
            self.ignored,
            self.began.elapsed().as_secs_f64()
        )
    }

    /// The longest the remaining tests can take: each one's timeout plus a
    /// second for its reset and boot, and some slack for the runner.
    fn budget(&self) -> Duration {
        self.tests[self.index..]
            .iter()
            .filter(|t| !t.ignored)
            .map(|t| t.timeout() + Duration::from_secs(1))
            .sum::<Duration>()
            + Duration::from_secs(5)
    }
}

/// What the source has produced and not yet handed to the capture loop, in
/// the order it happened, each chunk tagged with its stream.
#[derive(Default)]
struct Output {
    chunks: VecDeque<(Stream, Vec<u8>)>,
    /// Last runner byte queued, so a runner line never joins a console write
    /// that did not end its line.
    runner_tail: Option<u8>,
}

impl Output {
    fn push(&mut self, stream: Stream, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if stream == Stream::Runner {
            self.runner_tail = bytes.last().copied();
        }
        match self.chunks.back_mut() {
            Some((last, chunk)) if *last == stream => chunk.extend_from_slice(bytes),
            _ => self.chunks.push_back((stream, bytes.to_vec())),
        }
    }

    /// A runner status line, which has to start a line of its own.
    fn line(&mut self, text: &str) {
        if self.runner_tail.is_some_and(|b| b != b'\n') {
            self.push(Stream::Runner, b"\n");
        }
        self.push(Stream::Runner, text.as_bytes());
    }

    fn read(&mut self, buf: &mut [u8]) -> Option<(usize, Stream)> {
        let (stream, chunk) = self.chunks.front_mut()?;
        let stream = *stream;
        let n = buf.len().min(chunk.len());
        buf[..n].copy_from_slice(&chunk[..n]);
        chunk.drain(..n);
        if chunk.is_empty() {
            self.chunks.pop_front();
        }
        Some((n, stream))
    }

    fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

/// The firmware's RTT log, read while semihosting drives the target.
///
/// embedded-test firmware logs over defmt-rtt or rtt-target while the runner
/// talks to it over semihosting. `probe-rs run` reads both, and a test result
/// without the log that explains it is not much use, so this does too.
struct RttLog {
    /// `_SEGGER_RTT` from the ELF. Only an exact address is cheap enough to
    /// retry on every poll until the firmware initializes RTT.
    addr: u64,
    rtt: Option<Rtt>,
    /// The firmware's own up-channel mode, put back when the capture ends.
    restore_mode: Option<ChannelMode>,
    attached_once: bool,
}

impl RttLog {
    fn new(addr: u64) -> Self {
        Self {
            addr,
            rtt: None,
            restore_mode: None,
            attached_once: false,
        }
    }

    /// Zero the control block on a halted core, so the next attach finds the
    /// one the firmware writes after this reset rather than the last run's.
    fn invalidate(&mut self, core: &mut Core<'_>) -> Result<()> {
        core.write(self.addr, &vec![0u8; Rtt::control_block_size()])?;
        self.rtt = None;
        self.restore_mode = None;
        Ok(())
    }

    /// Attach once the firmware has initialized RTT, then queue everything in
    /// up-channel 0.
    fn drain(&mut self, core: &mut Core<'_>, output: &mut Output) -> Result<()> {
        if self.rtt.is_none() {
            let Ok(mut rtt) = Rtt::attach_region(core, &ScanRegion::Exact(self.addr)) else {
                return Ok(()); // Not initialized yet; try again next poll.
            };
            // Block rather than drop while the host is reading: a test that
            // logs faster than the runner polls would otherwise lose lines.
            if let Some(up) = rtt.up_channel(0) {
                self.restore_mode = Some(up.mode(core)?);
                up.set_mode(core, ChannelMode::BlockIfFull)?;
            }
            self.rtt = Some(rtt);
            self.attached_once = true;
        }
        let Some(up) = self.rtt.as_mut().and_then(|rtt| rtt.up_channel(0)) else {
            return Ok(());
        };
        let mut buf = [0u8; 1024];
        loop {
            let n = up.read(core, &mut buf)?;
            if n == 0 {
                return Ok(());
            }
            output.push(Stream::Firmware, &buf[..n]);
        }
    }

    fn restore(&mut self, core: &mut Core<'_>) -> Result<()> {
        if let (Some(mode), Some(up)) = (
            self.restore_mode.take(),
            self.rtt.as_mut().and_then(|rtt| rtt.up_channel(0)),
        ) {
            up.set_mode(core, mode)?;
        }
        Ok(())
    }
}

/// Reset into a fresh run, with the old RTT control block cleared first.
fn restart(session: &mut Session, rtt: Option<&mut RttLog>) -> Result<()> {
    let mut core = session.core(0)?;
    core.reset_and_halt(Duration::from_millis(500))?;
    if let Some(rtt) = rtt {
        rtt.invalidate(&mut core)?;
    }
    core.run()?;
    Ok(())
}

/// Queue a stack trace of the halted core for a failed test.
fn trace_failure(tracer: Option<&mut StackTracer>, core: &mut Core<'_>, output: &mut Output) {
    if let Some(tracer) = tracer {
        let trace = tracer
            .trace(core)
            .unwrap_or_else(|e| format!("(no stack trace: {e})\n"));
        output.line(&trace);
    }
}

pub struct SemihostingSource {
    session: Session,
    output: Output,
    handles: Vec<bool>,
    suite: Option<Suite>,
    rtt: Option<RttLog>,
    tracer: Option<StackTracer>,
    /// Unwind the stack for each failed test, as `probe-rs run` does.
    trace_failures: bool,
    done: bool,
}

impl SemihostingSource {
    pub fn attach(
        mut session: Session,
        elf: Option<&str>,
        reset: bool,
        verify_flash: bool,
        trace_failures: bool,
    ) -> Result<Self, String> {
        let data = elf
            .map(|path| std::fs::read(path).with_context(|| format!("reading {path}")))
            .transpose()
            .map_err(|e| format!("Semihosting ELF: {e:#}"))?;
        let tests = data
            .as_deref()
            .map(tests_from_elf)
            .transpose()
            .map_err(|e| format!("Semihosting ELF: {e:#}"))?
            .flatten();
        let mut rtt = data
            .as_deref()
            .and_then(|d| probe_rs::rtt::find_rtt_control_block_in_raw_file(d).ok())
            .flatten()
            .map(RttLog::new);
        // The runner drives the target by addresses read from this ELF, so the
        // flash has to hold this very build. A stale image runs the wrong code
        // at those addresses and dies with exceptions that look like firmware
        // bugs; refuse up front with the actual cause instead.
        if verify_flash && tests.is_some() {
            let path = elf.expect("tests come from an ELF path");
            let chip = session.target().name.clone();
            if !super::probers::verify_flash(&mut session, path, &chip)? {
                return Err(format!(
                    "The flash does not hold the image built from '{path}': the firmware on the \
                     device is a different build. embedded-test selects tests by address from \
                     this ELF, so running it against another build jumps into the wrong code. \
                     Flash it first: `flash_monitor` with this file, or `flash` and then this call."
                ));
            }
        }
        // Test harnesses need a fresh command-line request for every case,
        // including the first: without an attached debugger, ESP semihosting
        // traps can already have become firmware exceptions. Ordinary apps
        // retain attach-only monitor semantics.
        if reset || tests.is_some() {
            restart(&mut session, rtt.as_mut()).map_err(|e| e.to_string())?;
        }
        let mut output = Output::default();
        let suite = tests.map(|tests| {
            output.line(&format!("running {} tests\n", tests.len()));
            let mut suite = Suite::new(tests);
            suite.next = false;
            suite
        });
        Ok(Self {
            session,
            output,
            handles: Vec::new(),
            suite,
            rtt,
            tracer: elf.map(StackTracer::new),
            trace_failures,
            done: false,
        })
    }

    fn poll(&mut self) -> Result<()> {
        if self.done {
            return Ok(());
        }
        if let Some(suite) = &mut self.suite {
            while suite.index < suite.tests.len() && suite.tests[suite.index].ignored {
                self.output.line(&format!(
                    "test {} ... ignored\n",
                    suite.tests[suite.index].name
                ));
                suite.ignored += 1;
                suite.index += 1;
            }
            if suite.index == suite.tests.len() {
                self.output.line(&suite.summary());
                self.done = true;
                return Ok(());
            }
            if suite.next {
                restart(&mut self.session, self.rtt.as_mut())?;
                self.handles.clear();
                suite.started = Instant::now();
                suite.commanded = false;
                suite.next = false;
            }
            if suite.started.elapsed() >= suite.tests[suite.index].timeout() {
                let mut core = self.session.core(0)?;
                core.halt(Duration::from_millis(500))?;
                if let Some(rtt) = &mut self.rtt {
                    rtt.drain(&mut core, &mut self.output)?;
                }
                if self.trace_failures {
                    trace_failure(self.tracer.as_mut(), &mut core, &mut self.output);
                }
                let line = suite.result(suite.failure(false, true));
                self.output.line(&line);
                return Ok(());
            }
        }
        let mut core = self.session.core(0)?;
        let status = core.status()?;
        // RTT after the status: a core seen halted has already written
        // everything it logged before stopping, so that log is queued ahead
        // of whatever the runner reports about the halt.
        if let Some(rtt) = &mut self.rtt {
            rtt.drain(&mut core, &mut self.output)?;
        }
        let command = match status {
            CoreStatus::Halted(HaltReason::Breakpoint(BreakpointCause::Semihosting(command))) => {
                command
            }
            CoreStatus::Halted(reason) => {
                bail!("Core halted unexpectedly during semihosting capture: {reason:?}")
            }
            CoreStatus::LockedUp => bail!("Core locked up during semihosting capture"),
            _ => return Ok(()),
        };
        match command {
            SemihostingCommand::ExitSuccess | SemihostingCommand::ExitError(_) => {
                let panic = matches!(command, SemihostingCommand::ExitError(_));
                if let Some(suite) = &mut self.suite {
                    if !suite.commanded {
                        bail!("embedded-test exited before requesting its test command");
                    }
                    let failure = suite.failure(panic, false);
                    if failure.is_some() && self.trace_failures {
                        trace_failure(self.tracer.as_mut(), &mut core, &mut self.output);
                    }
                    let line = suite.result(failure);
                    self.output.line(&line);
                } else {
                    self.output.line(&match command {
                        SemihostingCommand::ExitError(details) => {
                            format!("\nsemihosting exit: {details}\n")
                        }
                        _ => "\nsemihosting exit: success\n".into(),
                    });
                    self.done = true;
                }
                return Ok(()); // Never resume past an exit trap.
            }
            SemihostingCommand::GetCommandLine(request) => {
                let command = if let Some(suite) = &mut self.suite {
                    if suite.commanded {
                        bail!("embedded-test requested command line twice");
                    }
                    suite.commanded = true;
                    format!(
                        "run_addr {}",
                        suite.tests[suite.index]
                            .address
                            .context("missing test address")?
                    )
                } else {
                    String::new()
                };
                request.write_command_line_to_target(&mut core, &command)?;
            }
            SemihostingCommand::Open(request) => {
                if request.path(&mut core)? == ":tt"
                    && matches!(request.mode(), "w" | "wb" | "a" | "ab")
                {
                    self.handles.push(true);
                    request.respond_with_handle(
                        &mut core,
                        NonZeroU32::new(self.handles.len() as u32)
                            .context("too many semihosting handles")?,
                    )?;
                } // probe-rs pre-fills failure for unsupported file operations.
            }
            SemihostingCommand::Close(request) => {
                if let Some(handle) = request
                    .file_handle()
                    .checked_sub(1)
                    .and_then(|i| self.handles.get_mut(i as usize))
                    && *handle
                {
                    *handle = false;
                    request.success(&mut core)?;
                }
            }
            SemihostingCommand::WriteConsole(request) => {
                let text = request.read(&mut core)?;
                self.output.push(Stream::Runner, text.as_bytes());
            }
            SemihostingCommand::Write(request) => {
                // Handles 1/2 also allow attach to a program which already opened stdout/stderr.
                let valid = if self.handles.is_empty() {
                    matches!(request.file_handle(), 1 | 2)
                } else {
                    request
                        .file_handle()
                        .checked_sub(1)
                        .and_then(|i| self.handles.get(i as usize))
                        .copied()
                        .unwrap_or(false)
                };
                if valid {
                    let bytes = request.read(&mut core)?;
                    self.output.push(Stream::Runner, &bytes);
                    request.write_status(&mut core, 0)?;
                }
            }
            SemihostingCommand::Time(request) => request.write_current_time(&mut core)?,
            SemihostingCommand::Errno(request) => request.write_errno(&mut core, 0)?,
            SemihostingCommand::Read(_)
            | SemihostingCommand::Seek(_)
            | SemihostingCommand::FileLength(_)
            | SemihostingCommand::Remove(_)
            | SemihostingCommand::Rename(_) => {}
            SemihostingCommand::Unknown(details) => {
                bail!("Unsupported semihosting operation {:#x}", details.operation)
            }
        }
        core.run()?;
        Ok(())
    }
}

impl ByteSource for SemihostingSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.read_tagged(buf).map(|(n, _)| n)
    }
    fn read_tagged(&mut self, buf: &mut [u8]) -> std::io::Result<(usize, Stream)> {
        if self.output.is_empty() {
            self.poll()
                .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
        }
        Ok(self.output.read(buf).unwrap_or((0, Stream::Runner)))
    }
    // There is no target-side ring buffer to flush. Servicing a pending syscall
    // produces fresh output; draining it would also execute tests before capture.
    fn text_only(&self) -> bool {
        self.rtt.is_none()
    }
    fn finished(&self) -> bool {
        self.done && self.output.is_empty()
    }
    fn idle_nap(&self) -> Duration {
        Duration::from_millis(1)
    }
    fn run_budget(&self) -> Option<Duration> {
        self.suite.as_ref().map(Suite::budget)
    }
    fn note(&self) -> Option<String> {
        let rtt = self.rtt.as_ref()?;
        (!rtt.attached_once).then(|| {
            format!(
                "RTT: the ELF defines `_SEGGER_RTT` at {:#x}, but the firmware never \
                 initialized it during this capture, so no firmware log is shown. \
                 embedded-test logging usually starts in `#[init]`; a run that fails \
                 before that logs nothing.",
                rtt.addr
            )
        })
    }
    /// A suite has already traced each failed test in place.
    fn stack_trace(&mut self) -> Option<Result<String, String>> {
        if self.suite.is_some() {
            return None;
        }
        let tracer = self.tracer.as_mut()?;
        Some(tracer.trace_session(&mut self.session))
    }
}

impl Drop for SemihostingSource {
    fn drop(&mut self) {
        if let Some(rtt) = &mut self.rtt {
            let restored = self
                .session
                .core(0)
                .map_err(anyhow::Error::from)
                .and_then(|mut core| rtt.restore(&mut core));
            if let Err(error) = restored {
                tracing::warn!("Failed to restore the RTT channel mode: {error:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object::write::{Object, Symbol, SymbolSection};
    use object::{Architecture, BinaryFormat, Endianness, SectionKind, SymbolFlags, SymbolScope};

    fn fixture(version: u32, metadata: &str) -> Vec<u8> {
        let mut elf = Object::new(BinaryFormat::Elf, Architecture::Riscv32, Endianness::Little);
        let section = elf.add_section(
            vec![],
            b".embedded_test".to_vec(),
            SectionKind::ReadOnlyData,
        );
        let mut data = version.to_le_bytes().to_vec();
        data.extend(0x4200_1234u32.to_le_bytes());
        data.extend(16u32.to_le_bytes());
        let path = b"misc_drivers::tests";
        data.extend((path.len() as u32).to_le_bytes());
        data.extend(path);
        elf.append_section_data(section, &data, 4);
        for (name, value, size) in [("EMBEDDED_TEST_VERSION", 0, 4), (metadata, 4, 12)] {
            elf.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value,
                size,
                kind: SymbolKind::Data,
                scope: SymbolScope::Linkage,
                weak: false,
                section: SymbolSection::Section(section),
                flags: SymbolFlags::None,
            });
        }
        elf.write().unwrap()
    }
    const META: &str = r#"{"name":"example","should_panic":true,"ignored":false,"timeout":3}"#;

    #[test]
    fn elf_metadata_resolves_names_addresses_and_expectations() {
        let tests = tests_from_elf(&fixture(1, META)).unwrap().unwrap();
        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].name, "tests::example");
        assert_eq!(tests[0].address, Some(0x4200_1234));
        assert!(tests[0].should_panic);
        assert!(!tests[0].ignored);
        assert_eq!(tests[0].timeout, Some(3));
    }
    #[test]
    fn malformed_and_future_metadata_are_errors() {
        assert!(tests_from_elf(b"not an ELF").is_err());
        assert!(
            tests_from_elf(&fixture(2, META))
                .unwrap_err()
                .to_string()
                .contains("protocol 2")
        );
        assert!(tests_from_elf(&fixture(1, "bad json")).is_err());
    }
    #[test]
    fn embedded_test_section_identifies_test_elfs() {
        assert!(is_embedded_test(&fixture(1, META)).unwrap());
        let plain = Object::new(BinaryFormat::Elf, Architecture::Riscv32, Endianness::Little);
        assert!(!is_embedded_test(&plain.write().unwrap()).unwrap());
    }
    #[test]
    fn panic_expectations_and_timeouts_affect_summary() {
        let tests = tests_from_elf(&fixture(1, META)).unwrap().unwrap();
        let mut suite = Suite::new(tests);
        suite.commanded = true;
        let failure = suite.failure(true, false);
        assert_eq!(failure, None);
        assert!(suite.result(failure).contains("... ok"));
        assert!(
            suite
                .summary()
                .starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; finished in ")
        );

        let mut suite = Suite::new(tests_from_elf(&fixture(1, META)).unwrap().unwrap());
        suite.commanded = true;
        let failure = suite.failure(false, true);
        assert!(suite.result(failure).contains("FAILED (test timeout)"));
        assert!(suite.summary().contains("0 passed; 1 failed"));

        // A should_panic test that returns normally fails, and says why.
        let mut suite = Suite::new(tests_from_elf(&fixture(1, META)).unwrap().unwrap());
        suite.commanded = true;
        let failure = suite.failure(false, false);
        assert!(suite.result(failure).contains("expected a panic"));
    }
    #[test]
    fn budget_covers_every_remaining_timeout() {
        let suite = Suite::new(tests_from_elf(&fixture(1, META)).unwrap().unwrap());
        // One test with a 3 s timeout: 3 s + 1 s reset + 5 s slack.
        assert_eq!(suite.budget(), Duration::from_secs(9));
    }
    #[test]
    fn runner_lines_start_on_their_own_line_and_keep_order() {
        let mut out = Output::default();
        out.push(Stream::Runner, b"console without newline");
        out.push(Stream::Firmware, b"\x01\x02");
        out.line("test a ... ok\n");
        let mut buf = [0u8; 64];
        let mut chunks = Vec::new();
        while let Some((n, stream)) = out.read(&mut buf) {
            chunks.push((stream, buf[..n].to_vec()));
        }
        assert_eq!(
            chunks,
            vec![
                (Stream::Runner, b"console without newline".to_vec()),
                (Stream::Firmware, vec![1, 2]),
                (Stream::Runner, b"\ntest a ... ok\n".to_vec()),
            ]
        );
    }
    #[test]
    fn a_chunk_larger_than_the_buffer_is_read_in_parts() {
        let mut out = Output::default();
        out.push(Stream::Firmware, b"abcdef");
        let mut buf = [0u8; 4];
        assert_eq!(out.read(&mut buf), Some((4, Stream::Firmware)));
        assert_eq!(&buf, b"abcd");
        assert_eq!(out.read(&mut buf), Some((2, Stream::Firmware)));
        assert_eq!(&buf[..2], b"ef");
        assert!(out.is_empty());
    }
}
