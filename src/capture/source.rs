//! The one seam that differs between backends: a synchronous "give me more
//! bytes" source. `serialport` (espflash) and `probe-rs` RTT are both blocking
//! libraries, so this stays synchronous and runs inside `spawn_blocking`; an
//! async trait here would only wrap blocking calls.

use std::time::Duration;

pub trait ByteSource {
    /// Read whatever bytes are available right now into `buf`. Returns `Ok(0)`
    /// when nothing is ready yet (the loop treats 0 as "no data this tick", not
    /// EOF, and paces itself with [`ByteSource::idle_nap`]). `Err` only on a real
    /// source failure.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;

    /// Discard already-buffered input (the `flush` option). Default: no-op.
    fn flush_input(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    /// Send bytes host → target (RTT down-channel, or serial TX). Returns how
    /// many bytes were accepted, which may be fewer than `buf` — an RTT
    /// down-channel is a small ring buffer drained by the firmware at its own
    /// pace, so a short write is normal backpressure, not an error. Use
    /// [`send_all`] rather than calling this directly.
    ///
    /// Default: unsupported, so a backend that cannot send says so explicitly
    /// instead of silently dropping the data.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = buf;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this backend does not support sending data to the target",
        ))
    }

    /// How long to sleep when `read` returns 0, to avoid busy-spinning on a
    /// poll-based source. Serial reads already block on a timeout, so its nap is
    /// zero; an RTT source overrides this with a small value.
    fn idle_nap(&self) -> Duration {
        Duration::ZERO
    }
}

/// How long [`send_all`] keeps retrying a backpressured write before giving up.
/// An RTT down-channel that never drains means the firmware is not reading it,
/// which is a firmware bug worth surfacing rather than blocking the tool call.
const SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Write every byte of `data`, retrying short writes until the target drains
/// enough of its buffer. A zero-byte write means the ring buffer is currently
/// full, so back off briefly instead of spinning.
pub fn send_all(source: &mut dyn ByteSource, data: &[u8]) -> Result<(), String> {
    let deadline = std::time::Instant::now() + SEND_TIMEOUT;
    let mut sent = 0usize;

    while sent < data.len() {
        match source.write(&data[sent..]) {
            Ok(0) => {
                if std::time::Instant::now() >= deadline {
                    return Err(format!(
                        "Timed out sending to the target: {sent}/{} bytes written. The buffer is \
                         full — the firmware is not draining it.",
                        data.len()
                    ));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(n) => sent += n,
            Err(e) => return Err(format!("Failed to send to the target: {e}")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Accepts at most `chunk` bytes per call, like an RTT down-channel whose
    /// ring buffer only has so much room right now.
    struct PartialSink {
        written: Vec<u8>,
        chunk: usize,
    }

    impl ByteSource for PartialSink {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.chunk);
            self.written.extend_from_slice(&buf[..n]);
            Ok(n)
        }
    }

    #[test]
    fn send_all_reassembles_across_partial_writes() {
        let mut sink = PartialSink {
            written: Vec::new(),
            chunk: 3,
        };
        send_all(&mut sink, b"hello target\n").unwrap();
        assert_eq!(sink.written, b"hello target\n");
    }

    /// A backend that never drains must fail with a diagnosis, not hang the
    /// tool call forever.
    struct FullSink;
    impl ByteSource for FullSink {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }

    #[test]
    fn send_all_gives_up_when_the_buffer_never_drains() {
        let err = send_all(&mut FullSink, b"stuck").unwrap_err();
        assert!(err.contains("0/5 bytes"), "unhelpful message: {err}");
    }

    /// A source that does not override `write` must say so rather than silently
    /// swallowing the payload.
    struct ReadOnly;
    impl ByteSource for ReadOnly {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }

    #[test]
    fn default_write_is_unsupported() {
        let err = send_all(&mut ReadOnly, b"x").unwrap_err();
        assert!(err.contains("does not support sending"), "got: {err}");
    }
}
