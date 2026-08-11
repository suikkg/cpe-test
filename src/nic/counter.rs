//! Injectable network-interface byte-counter readers.
//!
//! The operating-system reader is kept behind this small trait so monitor
//! state transitions can be tested with deterministic failures, recovery, and
//! counter resets without touching a real interface.

/// A reader of cumulative RX/TX byte counters for one interface.
pub trait NicCounterReader: Send + Sync {
    fn read_counters(&self, iface: &str) -> Result<(u64, u64), String>;
}

/// Adapter for a closure, useful for deterministic tests and small callers.
#[cfg(test)]
pub struct FnNicCounterReader<F> {
    read: F,
}

#[cfg(test)]
impl<F> FnNicCounterReader<F> {
    pub fn new(read: F) -> Self {
        Self { read }
    }
}

#[cfg(test)]
impl<F> NicCounterReader for FnNicCounterReader<F>
where
    F: Fn(&str) -> Result<(u64, u64), String> + Send + Sync,
{
    fn read_counters(&self, iface: &str) -> Result<(u64, u64), String> {
        (self.read)(iface)
    }
}

/// The production reader, which delegates to the platform-specific function
/// in `monitor.rs`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemNicCounterReader;

impl NicCounterReader for SystemNicCounterReader {
    fn read_counters(&self, iface: &str) -> Result<(u64, u64), String> {
        super::monitor::read_counters(iface)
    }
}
