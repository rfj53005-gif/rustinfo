//! Interruptible waits let CSV recording flush cleanly on Ctrl-C and SIGTERM.
use anyhow::Result;
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    SigId,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

pub struct Shutdown {
    flag: Arc<AtomicBool>,
    handlers: Vec<SigId>,
}
impl Shutdown {
    pub fn install() -> Result<Self> {
        let mut shutdown = Self {
            flag: Arc::new(AtomicBool::new(false)),
            handlers: Vec::new(),
        };
        for signal in [SIGINT, SIGTERM] {
            shutdown
                .handlers
                .push(signal_hook::flag::register(signal, shutdown.flag.clone())?);
        }
        Ok(shutdown)
    }
    /// Returns true when a shutdown signal interrupted the wait.
    pub fn wait_until(&self, deadline: Instant) -> bool {
        loop {
            if self.requested() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(100)));
        }
    }
    pub fn requested(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}
impl Drop for Shutdown {
    fn drop(&mut self) {
        for handler in self.handlers.drain(..) {
            signal_hook::low_level::unregister(handler);
        }
    }
}
