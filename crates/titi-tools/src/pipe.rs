//! Running a shell command over pipes, bounded the way [`crate::pty`] is.
//!
//! This is `bash`'s default path: output comes back unwrapped and
//! uncoloured. It used to wait on the command with no deadline, so a dev
//! server, a `--watch` or a `sleep 999` held the turn until the user killed
//! titi. Now a run ends on its deadline or on an [`Interrupt`], whichever
//! comes first, and either way its whole process group goes down with it:
//! `sh -c` is only the first process of a pipeline, and killing it alone
//! would leave the rest running.

use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::pty::Interrupt;

/// Bytes kept from the start of each stream, and again from its end. A
/// build log puts its error last and a listing its point first, so both ends
/// survive and only the middle is dropped.
pub const KEEP_EACH_END: usize = 64 * 1024;

/// How often the run loop looks at the child, the deadline, and the interrupt.
const POLL: Duration = Duration::from_millis(10);

/// How long a stopped command gets after `SIGTERM` before `SIGKILL`.
const TERM_GRACE: Duration = Duration::from_millis(200);

/// How long the readers get to drain after the shell exits. A command that
/// left a server running in the background keeps the pipes open forever, so
/// the wait is bounded and the readers are left behind.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// A command that ran to completion, however it exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// Standard output, then standard error, each cut in the middle past
    /// [`KEEP_EACH_END`] with a note saying how much was left out.
    pub output: String,
    /// `None` when a signal ended the shell.
    pub exit_code: Option<i32>,
    pub success: bool,
}

/// Why a run produced no exit status. Both stops keep what the command had
/// printed: a hung build still says where it hung.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PipeError {
    #[error("could not start sh: {reason}")]
    Spawn { reason: String },
    #[error("could not wait on the command: {reason}")]
    Wait { reason: String },
    #[error("command timed out after {}s and was killed\n{output}", .after.as_secs_f32())]
    TimedOut { after: Duration, output: String },
    #[error("command interrupted\n{output}")]
    Interrupted { output: String },
}

/// Runs `command` through `sh -c` in `cwd`, with stdin closed.
///
/// Blocking, like [`crate::pty::run`]: this crate holds no runtime.
pub fn run(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    interrupt: &Interrupt,
) -> Result<Run, PipeError> {
    let mark = interrupt.mark();
    // A cancel that landed while this call was queued still means stop: the
    // command must not start at all.
    if interrupt.is_raised() {
        return Err(PipeError::Interrupted {
            output: String::new(),
        });
    }
    let mut shell = Command::new("sh");
    shell
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so a stop reaches every process the command
    // started, not just the shell.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut shell, 0);
    let mut child = shell.spawn().map_err(|error| PipeError::Spawn {
        reason: error.to_string(),
    })?;
    let stdout = Reader::spawn(child.stdout.take());
    let stderr = Reader::spawn(child.stderr.take());
    let output = || {
        let deadline = Instant::now() + DRAIN_GRACE;
        while !(stdout.drained() && stderr.drained()) && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        let mut text = stdout.render();
        let errors = stderr.render();
        if !text.is_empty() && !errors.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&errors);
        text
    };

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Err(error) => {
                stop(&mut child);
                return Err(PipeError::Wait {
                    reason: error.to_string(),
                });
            }
            Ok(Some(status)) => {
                return Ok(Run {
                    output: output(),
                    exit_code: status.code(),
                    success: status.success(),
                });
            }
            Ok(None) => {}
        }
        if interrupt.raised_since(mark) {
            stop(&mut child);
            return Err(PipeError::Interrupted { output: output() });
        }
        if Instant::now() >= deadline {
            stop(&mut child);
            return Err(PipeError::TimedOut {
                after: timeout,
                output: output(),
            });
        }
        std::thread::sleep(POLL);
    }
}

/// `SIGTERM` to the group so the command can clean up, then `SIGKILL` to the
/// group for whatever ignored it or is stopped, and to the shell itself.
fn stop(child: &mut Child) {
    signal_group(child, "TERM");
    let grace = Instant::now() + TERM_GRACE;
    while Instant::now() < grace {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(POLL);
    }
    signal_group(child, "KILL");
    let _ = child.kill();
    let _ = child.wait();
}

/// Signals the group the shell leads. Safe Rust has no `killpg`, and the
/// workspace forbids `unsafe`, so this asks `sh`, which `run` needs anyway.
#[cfg(unix)]
fn signal_group(child: &Child, signal: &str) {
    let _ = Command::new("sh")
        .arg("-c")
        .arg(format!("kill -s {signal} -- -{}", child.id()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn signal_group(_child: &Child, _signal: &str) {}

/// One stream, read on its own thread into a [`Capture`].
struct Reader {
    capture: Arc<Mutex<Capture>>,
    drained: Arc<AtomicBool>,
}

impl Reader {
    fn spawn(stream: Option<impl Read + Send + 'static>) -> Self {
        let capture = Arc::new(Mutex::new(Capture::default()));
        let drained = Arc::new(AtomicBool::new(stream.is_none()));
        if let Some(mut stream) = stream {
            let capture = Arc::clone(&capture);
            let drained = Arc::clone(&drained);
            // Detached on purpose: it ends on EOF, and a grandchild that
            // holds the pipe open must not keep the turn hostage.
            std::thread::spawn(move || {
                let mut buffer = [0u8; 8192];
                while let Ok(read) = stream.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    let Ok(mut capture) = capture.lock() else {
                        break;
                    };
                    capture.push(&buffer[..read]);
                }
                drained.store(true, Ordering::SeqCst);
            });
        }
        Self { capture, drained }
    }

    fn drained(&self) -> bool {
        self.drained.load(Ordering::SeqCst)
    }

    fn render(&self) -> String {
        self.capture
            .lock()
            .map(|capture| capture.render())
            .unwrap_or_default()
    }
}

/// The first and the last [`KEEP_EACH_END`] bytes of a stream, and a count of
/// the bytes between them that were dropped.
#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    dropped: usize,
}

impl Capture {
    fn push(&mut self, bytes: &[u8]) {
        let room = KEEP_EACH_END.saturating_sub(self.head.len());
        let (head, rest) = bytes.split_at(room.min(bytes.len()));
        self.head.extend_from_slice(head);
        self.tail.extend(rest);
        let excess = self.tail.len().saturating_sub(KEEP_EACH_END);
        if excess > 0 {
            self.tail.drain(..excess);
            self.dropped += excess;
        }
    }

    fn render(&self) -> String {
        let mut text = String::from_utf8_lossy(&self.head).into_owned();
        if self.dropped > 0 {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!("… [{} bytes left out] …\n", self.dropped));
        }
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        text.push_str(&String::from_utf8_lossy(&tail));
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick(command: &str, cwd: &Path) -> Result<Run, PipeError> {
        run(command, cwd, Duration::from_secs(20), &Interrupt::new())
    }

    #[test]
    fn output_is_stdout_then_stderr_with_the_exit_code() {
        let run = quick("echo out; echo err >&2; exit 3", Path::new("."))
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(run.output, "out\nerr\n");
        assert_eq!(run.exit_code, Some(3));
        assert!(!run.success);

        let run = quick("printf done", Path::new(".")).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(run.output, "done");
        assert!(run.success);
    }

    #[test]
    fn the_command_reads_no_input() {
        let run =
            quick("cat; echo after", Path::new(".")).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(run.output, "after\n");
    }

    #[test]
    fn a_command_past_its_deadline_is_killed_with_everything_it_started() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let started = Instant::now();
        let error = run(
            "echo starting; (sleep 1; touch survived) & sleep 30",
            dir.path(),
            Duration::from_millis(300),
            &Interrupt::new(),
        )
        .expect_err("a 30s command must not finish inside a 300ms deadline");
        assert!(started.elapsed() < Duration::from_secs(5));
        match &error {
            PipeError::TimedOut { after, output } => {
                assert_eq!(*after, Duration::from_millis(300));
                assert!(output.contains("starting"), "{output}");
            }
            other => panic!("expected a timeout, got {other}"),
        }
        assert!(error.to_string().contains("timed out"), "{error}");
        // Long past the background sleep: it was killed with the shell.
        std::thread::sleep(Duration::from_millis(1_200));
        assert!(!dir.path().join("survived").exists());
    }

    #[test]
    fn a_raised_interrupt_stops_the_command() {
        let interrupt = Interrupt::new();
        let armed = interrupt.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            armed.raise();
            // The engine lowers it again when the next turn starts.
            armed.clear();
        });
        let started = Instant::now();
        let error = run(
            "echo begun; sleep 30",
            Path::new("."),
            Duration::from_secs(30),
            &interrupt,
        )
        .expect_err("the interrupt must cut the command short");
        assert!(started.elapsed() < Duration::from_secs(5));
        match error {
            PipeError::Interrupted { output } => assert_eq!(output, "begun\n"),
            other => panic!("expected an interrupt, got {other}"),
        }
    }

    #[test]
    fn a_raise_before_the_start_runs_nothing() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let interrupt = Interrupt::new();
        interrupt.raise();
        let error = run("touch ran", dir.path(), Duration::from_secs(5), &interrupt)
            .expect_err("a raised interrupt must stop the command before it starts");
        assert!(matches!(error, PipeError::Interrupted { .. }), "{error}");
        assert!(!dir.path().join("ran").exists());
    }

    /// `server &` leaves a process holding the pipes open after the shell
    /// exits. The answer comes when the shell is done, not when the server is.
    #[test]
    fn a_background_process_does_not_hold_the_answer() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let started = Instant::now();
        let run =
            quick("sleep 3 & echo launched", dir.path()).unwrap_or_else(|error| panic!("{error}"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "took {:?}",
            started.elapsed()
        );
        assert!(run.success);
        assert_eq!(run.output, "launched\n");
    }

    #[test]
    fn a_long_stream_keeps_its_start_and_its_end() {
        let run = quick("seq 1 200000", Path::new(".")).unwrap_or_else(|error| panic!("{error}"));
        assert!(run.success);
        assert!(run.output.starts_with("1\n2\n3\n"), "{}", &run.output[..20]);
        assert!(run.output.ends_with("199999\n200000\n"));
        assert!(run.output.contains(" bytes left out] …\n"));
        assert!(run.output.len() < 3 * KEEP_EACH_END, "{}", run.output.len());
    }
}
