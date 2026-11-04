use crate::{Error, Result};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
pub use unix::capture;
#[cfg(windows)]
pub use windows::capture;

#[derive(Clone, Debug)]
pub struct Signals(Arc<AtomicUsize>);
impl Signals {
    pub fn none() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }
    pub fn install() -> io::Result<Self> {
        let signals = Self::none();
        #[cfg(unix)]
        {
            use signal_hook::consts::{SIGINT, SIGTERM};
            signal_hook::flag::register_usize(SIGINT, signals.0.clone(), SIGINT as usize)?;
            signal_hook::flag::register_usize(SIGTERM, signals.0.clone(), SIGTERM as usize)?;
        }
        #[cfg(windows)]
        {
            let received = signals.0.clone();
            ctrlc::set_handler(move || received.store(2, Ordering::Relaxed))
                .map_err(io::Error::other)?;
        }
        Ok(signals)
    }
    pub fn received(&self) -> Option<i32> {
        let signal = self.0.load(Ordering::Relaxed);
        (signal != 0).then_some(signal as i32)
    }
    pub fn check(&self) -> Result<()> {
        match self.received() {
            Some(signal) => Err(Error::interrupted(signal)),
            None => Ok(()),
        }
    }
    #[cfg(test)]
    pub(crate) fn pending(signal: i32) -> Self {
        Self(Arc::new(AtomicUsize::new(signal as usize)))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CaptureFailure {
    Timeout,
    OutputLimit,
    Interrupted(i32),
}
#[derive(Debug)]
pub struct Captured {
    pub output: Vec<u8>,
    pub stderr: Vec<u8>,
    pub returncode: Option<i32>,
    pub failure: Option<CaptureFailure>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn pending_interrupt_cancels_an_owned_child() {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child.arg("--list");
        let captured = capture(
            &mut child,
            Duration::from_secs(5),
            4096,
            &Signals::pending(2),
            true,
        )
        .unwrap();
        assert_eq!(captured.failure, Some(CaptureFailure::Interrupted(2)));
        assert!(captured.returncode.is_some());
    }
}
