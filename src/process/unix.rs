use super::{CaptureFailure, Captured, Signals};
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, poll};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use rustix::process::{WaitId, WaitIdOptions, waitid};
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct OwnedChild {
    child: Child,
    cleaned: bool,
}
impl OwnedChild {
    fn finished(&self) -> io::Result<bool> {
        // WNOWAIT leaves the leader unreaped: its PID cannot be reused before
        // we signal the group. Never probe with try_wait and then kill an old ID.
        match waitid(
            WaitId::Pid(
                rustix::process::Pid::from_raw(self.child.id() as i32)
                    .ok_or_else(|| io::Error::other("invalid owned child PID"))?,
            ),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ) {
            Ok(None) => Ok(false),
            Ok(Some(_)) => Ok(true),
            Err(rustix::io::Errno::INTR) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
    fn stop(&mut self) -> io::Result<i32> {
        if self.cleaned {
            return Err(io::Error::other("child already cleaned"));
        }
        // Mark before waiting so Drop can never signal a reused process group.
        self.cleaned = true;
        let result = killpg(Pid::from_raw(self.child.id() as i32), Signal::SIGKILL);
        if let Err(error) = result
            && error != Errno::ESRCH
        {
            let _ = self.child.kill();
        }
        let status = self.child.wait()?;
        Ok(status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(1)))
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.stop();
        }
    }
}

fn read_pipe() -> io::Result<(io::PipeReader, io::PipeWriter)> {
    let (reader, writer) = io::pipe()?;
    fcntl(&reader, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
    Ok((reader, writer))
}

/// Capture at most `limit` combined bytes. With merging enabled, both streams
/// share one kernel pipe, preserving write order rather than racing reader threads.
pub fn capture(
    command: &mut Command,
    timeout: Duration,
    limit: usize,
    signals: &Signals,
    merge_stderr: bool,
) -> io::Result<Captured> {
    let (mut output, writer) = read_pipe()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer.try_clone()?))
        .process_group(0);
    let (mut errors, error_writer) = if merge_stderr {
        command.stderr(Stdio::from(writer));
        (None, None)
    } else {
        drop(writer);
        let (reader, writer) = read_pipe()?;
        (Some(reader), Some(writer))
    };
    if let Some(writer) = error_writer {
        command.stderr(Stdio::from(writer));
    }
    let child = command.spawn();
    // Command retains configured Stdio descriptors after spawn. Drop those
    // copies immediately, otherwise EOF cannot be observed on the reader.
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = OwnedChild {
        child: child?,
        cleaned: false,
    };
    let mut captured = Captured {
        output: Vec::new(),
        stderr: Vec::new(),
        returncode: None,
        failure: None,
    };
    let deadline = Instant::now() + timeout;
    let mut output_open = true;
    let mut errors_open = errors.is_some();
    loop {
        if let Some(signal) = signals.received() {
            captured.failure = Some(CaptureFailure::Interrupted(signal));
            break;
        }
        if !child.cleaned && child.finished()? {
            captured.returncode = Some(child.stop()?);
        }
        if child.cleaned && !output_open && !errors_open {
            break;
        }
        if Instant::now() >= deadline {
            captured.failure = Some(CaptureFailure::Timeout);
            break;
        }
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(50));
        let mut pollers = Vec::new();
        if output_open {
            pollers.push(PollFd::new(
                output.as_fd(),
                PollFlags::POLLIN | PollFlags::POLLHUP,
            ));
        }
        if errors_open {
            pollers.push(PollFd::new(
                errors.as_ref().expect("open stderr").as_fd(),
                PollFlags::POLLIN | PollFlags::POLLHUP,
            ));
        }
        if pollers.is_empty() {
            std::thread::sleep(remaining);
            continue;
        }
        match poll(&mut pollers, remaining.as_millis().max(1) as u16) {
            Ok(_) => {}
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(error.into()),
        }
        drop(pollers);
        for (pipe, bytes, open) in [(&mut output, &mut captured.output, &mut output_open)]
            .into_iter()
            .chain(
                errors
                    .as_mut()
                    .map(|pipe| (pipe, &mut captured.stderr, &mut errors_open)),
            )
        {
            if !*open {
                continue;
            }
            let mut buffer = [0_u8; 65536];
            // One read per stream keeps cancellation/deadline checks responsive
            // even when a child writes continuously.
            match pipe.read(&mut buffer) {
                Ok(0) => *open = false,
                Ok(count) => {
                    // The other stream's size is accounted below between reads.
                    let room = limit.saturating_sub(bytes.len());
                    bytes.extend_from_slice(&buffer[..count.min(room)]);
                    if count > room {
                        captured.failure = Some(CaptureFailure::OutputLimit);
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        if captured.output.len() + captured.stderr.len() > limit {
            let available = limit.saturating_sub(captured.output.len());
            captured.stderr.truncate(available);
            captured.failure = Some(CaptureFailure::OutputLimit);
        }
        if captured.failure.is_some() {
            break;
        }
    }
    if !child.cleaned {
        captured.returncode = Some(child.stop()?);
    }
    Ok(captured)
}
