use super::{CaptureFailure, Captured, Signals};
use mio::windows::NamedPipe;
use mio::{Events, Interest, Poll, Token};
use process_wrap::std::{ChildWrapper, CommandWrap, JobObject};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct OwnedChild {
    child: Box<dyn ChildWrapper>,
    cleaned: bool,
}
impl OwnedChild {
    fn finished(&mut self) -> io::Result<bool> {
        // Windows process and job handles remain stable after the leader exits.
        // Probing the leader therefore cannot expose a recycled PID to cleanup.
        // There is exactly one JobObject layer. Probe the native leader below
        // it: the wrapper's wait methods also consume job completion packets.
        self.child
            .inner_mut()
            .try_wait()
            .map(|status| status.is_some())
    }
    fn stop(&mut self) -> io::Result<i32> {
        if self.cleaned {
            return Err(io::Error::other("child already cleaned"));
        }
        // JobObject::start_kill terminates every owned descendant, even when the
        // leader has already exited. A plain Child::kill would miss those children.
        self.child.start_kill()?;
        self.cleaned = true;
        Ok(self.child.inner_mut().wait()?.code().unwrap_or(1))
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.stop();
        }
    }
}

fn read_pipe(poll: &Poll, token: Token) -> io::Result<(NamedPipe, File)> {
    // Reserve a random, per-user temporary name. The pipe is opened with
    // FIRST_PIPE_INSTANCE by Mio, so an existing pipe fails closed. Long random
    // names also keep unrelated clients from discovering these private streams.
    let reservation = tempfile::Builder::new()
        .prefix("project-check-pipe-")
        .rand_bytes(32)
        .tempdir()?;
    let suffix = reservation
        .path()
        .file_name()
        .ok_or_else(|| io::Error::other("pipe reservation has no name"))?
        .to_string_lossy();
    let name = format!(r"\\.\pipe\{}-{}", suffix, std::process::id());
    let mut reader = NamedPipe::new(&name)?;
    poll.registry()
        .register(&mut reader, token, Interest::READABLE)?;
    // The child receives a synchronous writer. Only the parent side performs
    // overlapped I/O; most programs expect ordinary blocking stdout/stderr.
    let writer = OpenOptions::new().read(true).write(true).open(&name)?;
    match reader.connect() {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
        Err(error) => return Err(error),
    }
    Ok((reader, writer))
}

/// Capture bounded output using nonblocking overlapped pipes and a Windows job.
/// A shared stdout/stderr writer preserves kernel write order when merging.
pub fn capture(
    command: &mut Command,
    timeout: Duration,
    limit: usize,
    signals: &Signals,
    merge_stderr: bool,
) -> io::Result<Captured> {
    let mut poll = Poll::new()?;
    let (mut output, writer) = read_pipe(&poll, Token(0))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer.try_clone()?));
    let mut errors = if merge_stderr {
        command.stderr(Stdio::from(writer));
        None
    } else {
        drop(writer);
        let (reader, writer) = read_pipe(&poll, Token(1))?;
        command.stderr(Stdio::from(writer));
        Some(reader)
    };

    // Move the complete Command, preserving env_clear, arguments, cwd and every
    // configured environment change. JobObject starts it suspended, assigns it
    // to the job using its stable handle, and only then resumes execution.
    let replacement = Command::new(command.get_program());
    let mut wrapped = CommandWrap::from(std::mem::replace(command, replacement));
    wrapped.wrap(JobObject);
    let child = wrapped.spawn();
    *command = wrapped.into_command();
    // Command keeps configured handles after spawning; discard its copies so
    // EOF is visible once the child and its descendants close their writers.
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
    let mut events = Events::with_capacity(8);
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

        let mut progress = false;
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
            match pipe.read(&mut buffer) {
                Ok(0) => *open = false,
                Ok(count) => {
                    progress = true;
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
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(50));
        // Poll must also advance IOCP completions when a read just succeeded.
        // A zero timeout drains ready packets without delaying buffered output.
        let wait = if progress { Duration::ZERO } else { remaining };
        match poll.poll(&mut events, Some(wait)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    if !child.cleaned {
        captured.returncode = Some(child.stop()?);
    }
    // NamedPipe::drop cancels pending overlapped reads. No blocking reader
    // thread survives a timeout, even if an external process retained a writer.
    Ok(captured)
}
