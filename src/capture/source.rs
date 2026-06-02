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

    /// How long to sleep when `read` returns 0, to avoid busy-spinning on a
    /// poll-based source. Serial reads already block on a timeout, so its nap is
    /// zero; an RTT source overrides this with a small value.
    fn idle_nap(&self) -> Duration {
        Duration::ZERO
    }
}
