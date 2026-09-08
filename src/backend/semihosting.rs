//! Semihosting console capture and the embedded-test host protocol.
//! Syscall decoding, target buffers and responses are owned by probe-rs.
use crate::capture::ByteSource;
use anyhow::{Context, Result, bail};
use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};
use probe_rs::{BreakpointCause, CoreStatus, HaltReason, Session, semihosting::SemihostingCommand};
use serde::Deserialize;
use std::{
    collections::VecDeque,
    num::NonZeroU32,
    time::{Duration, Instant},
};

#[derive(Debug, Deserialize)]
struct Test {
    name: String,
    should_panic: bool,
    ignored: bool,
    timeout: Option<u32>,
    #[serde(skip)]
    address: Option<u32>,
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
            started: Instant::now(),
            commanded: false,
            next: true,
        }
    }
    fn result(&mut self, panic: bool, failure: Option<&str>) -> String {
        let test = &self.tests[self.index];
        let success = failure.is_none() && self.commanded && panic == test.should_panic;
        if success {
            self.passed += 1;
        } else {
            self.failed += 1;
        }
        let suffix = failure.map(|s| format!(" ({s})")).unwrap_or_default();
        let line = format!(
            "test {} ... {}{suffix}\n",
            test.name,
            if success { "ok" } else { "FAILED" }
        );
        self.index += 1;
        self.next = true;
        line
    }
    fn summary(&self) -> String {
        format!(
            "test result: {}. {} passed; {} failed; {} ignored\n",
            if self.failed == 0 { "ok" } else { "FAILED" },
            self.passed,
            self.failed,
            self.ignored
        )
    }
}

pub struct SemihostingSource {
    session: Session,
    pending: VecDeque<u8>,
    handles: Vec<bool>,
    suite: Option<Suite>,
    done: bool,
    last_byte: Option<u8>,
}

impl SemihostingSource {
    pub fn attach(
        mut session: Session,
        elf: Option<&str>,
        reset: bool,
        verify_flash: bool,
    ) -> Result<Self, String> {
        let tests = elf
            .map(|path| {
                std::fs::read(path)
                    .with_context(|| format!("reading {path}"))
                    .and_then(|data| tests_from_elf(&data))
            })
            .transpose()
            .map_err(|e| format!("Semihosting ELF: {e:#}"))?
            .flatten();
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
        if reset || tests.is_some() {
            let mut core = session.core(0).map_err(|e| e.to_string())?;
            core.reset_and_halt(Duration::from_millis(500))
                .map_err(|e| e.to_string())?;
            core.run().map_err(|e| e.to_string())?;
        }
        // Test harnesses need a fresh command-line request for every case,
        // including the first: without an attached debugger, ESP semihosting
        // traps can already have become firmware exceptions. Ordinary apps
        // retain attach-only monitor semantics.
        let suite = tests.map(|tests| {
            let mut suite = Suite::new(tests);
            suite.next = false;
            suite
        });
        Ok(Self {
            session,
            pending: VecDeque::new(),
            handles: Vec::new(),
            suite,
            done: false,
            last_byte: None,
        })
    }

    fn emit(&mut self, text: &str) {
        // Harness status lines must not join an unterminated console write.
        if self
            .pending
            .back()
            .copied()
            .or(self.last_byte)
            .is_some_and(|b| b != b'\n')
        {
            self.pending.push_back(b'\n');
        }
        self.pending.extend(text.as_bytes());
    }

    fn poll(&mut self) -> Result<()> {
        if self.done {
            return Ok(());
        }
        if let Some(suite) = &mut self.suite {
            while suite.index < suite.tests.len() && suite.tests[suite.index].ignored {
                self.pending.extend(
                    format!("test {} ... ignored\n", suite.tests[suite.index].name).bytes(),
                );
                suite.ignored += 1;
                suite.index += 1;
            }
            if suite.index == suite.tests.len() {
                let summary = suite.summary();
                self.emit(&summary);
                self.done = true;
                return Ok(());
            }
            if suite.next {
                let mut core = self.session.core(0)?;
                core.reset_and_halt(Duration::from_millis(500))?;
                core.run()?;
                self.handles.clear();
                suite.started = Instant::now();
                suite.commanded = false;
                suite.next = false;
            }
            let timeout =
                Duration::from_secs(suite.tests[suite.index].timeout.unwrap_or(60) as u64);
            if suite.started.elapsed() >= timeout {
                self.session.core(0)?.halt(Duration::from_millis(500))?;
                let line = suite.result(false, Some("test timeout"));
                self.emit(&line);
                return Ok(());
            }
        }
        let mut core = self.session.core(0)?;
        let command = match core.status()? {
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
                drop(core);
                let panic = matches!(command, SemihostingCommand::ExitError(_));
                if let Some(suite) = &mut self.suite {
                    if !suite.commanded {
                        bail!("embedded-test exited before requesting its test command");
                    }
                    let line = suite.result(panic, None);
                    self.emit(&line);
                } else {
                    self.emit(&match command {
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
                self.pending.extend(request.read(&mut core)?.bytes())
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
                    self.pending.extend(request.read(&mut core)?);
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
        if self.pending.is_empty() {
            self.poll()
                .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
        }
        let n = buf.len().min(self.pending.len());
        for byte in &mut buf[..n] {
            *byte = self.pending.pop_front().unwrap();
        }
        if n > 0 {
            self.last_byte = Some(buf[n - 1]);
        }
        Ok(n)
    }
    // There is no target-side ring buffer to flush. Servicing a pending syscall
    // produces fresh output; draining it would also execute tests before capture.
    fn text_only(&self) -> bool {
        true
    }
    fn finished(&self) -> bool {
        self.done && self.pending.is_empty()
    }
    fn idle_nap(&self) -> Duration {
        Duration::from_millis(1)
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
    fn panic_expectations_and_timeouts_affect_summary() {
        let tests = tests_from_elf(&fixture(1, META)).unwrap().unwrap();
        let mut suite = Suite::new(tests);
        suite.commanded = true;
        assert!(suite.result(true, None).contains("... ok"));
        assert_eq!(
            suite.summary(),
            "test result: ok. 1 passed; 0 failed; 0 ignored\n"
        );
        let mut suite = Suite::new(tests_from_elf(&fixture(1, META)).unwrap().unwrap());
        suite.commanded = true;
        assert!(
            suite
                .result(false, Some("test timeout"))
                .contains("FAILED (test timeout)")
        );
        assert!(suite.summary().contains("0 passed; 1 failed"));
    }
}
