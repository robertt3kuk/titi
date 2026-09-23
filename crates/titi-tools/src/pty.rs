//! Running a shell command under a real pty.
//!
//! A pipe makes `isatty` false, and half the toolchain changes shape for it:
//! no colour, no progress, `git`/`less` paging off, some programs buffering
//! differently. Under a pty the command sees a terminal and behaves the way
//! the user would see it in their own shell.
//!
//! The run is bounded on three axes — wall clock, captured bytes, and an
//! [`Interrupt`] the surface can raise — because a tool call that never comes
//! back freezes the whole turn.
//!
//! Spec: `docs/research/tools-core/README.md` (bash, PTY runtime).

use std::fmt;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};

/// Wall clock a command gets before it is killed, when the caller asks for no
/// particular timeout.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Bounds on a caller-supplied timeout. Zero means "no deadline" nowhere in
/// titi: a tool call the turn cannot end is worse than a truncated one.
pub const MIN_TIMEOUT_SECS: u64 = 1;
pub const MAX_TIMEOUT_SECS: u64 = 3_600;

/// Bytes kept from the command's output. A `yes` loop fills memory in seconds,
/// so capture stops here and only counts what it drops.
pub const OUTPUT_CAP: usize = 64 * 1024;

/// How often the run loop looks at the child, the deadline, and the interrupt.
const POLL: Duration = Duration::from_millis(10);

/// How long a killed command gets to die politely after Ctrl+C before SIGKILL.
const SIGINT_GRACE: Duration = Duration::from_millis(200);

/// How long the reader thread gets to finish draining after the child exits.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// `ETX`, what the terminal sends on Ctrl+C. The pty line discipline turns it
/// into `SIGINT` for the foreground process group, so a shell's children go
/// down with it — `Child::kill` alone would only reach the shell.
const CTRL_C: u8 = 0x03;

/// A raised flag interrupts the command a [`run`] is waiting on. Clonable, so
/// the surface that owns the cancel key holds one end and the tool the other.
#[derive(Clone, Debug, Default)]
pub struct Interrupt {
    raised: Arc<AtomicBool>,
}

impl Interrupt {
    pub fn new() -> Self {
        Self::default()
    }

    /// Interrupts the command currently running, if any. Idempotent.
    pub fn raise(&self) {
        self.raised.store(true, Ordering::SeqCst);
    }

    pub fn is_raised(&self) -> bool {
        self.raised.load(Ordering::SeqCst)
    }

    /// Lowers the flag so the next command is not killed on sight. The owner
    /// of the handle decides when a cancel is spent; [`run`] never clears it.
    pub fn clear(&self) {
        self.raised.store(false, Ordering::SeqCst);
    }
}

/// What the command printed, plus how much was thrown away at the cap.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Output {
    pub text: String,
    /// Bytes produced past [`Options::cap`] and dropped.
    pub dropped: usize,
}

impl Output {
    pub fn truncated(&self) -> bool {
        self.dropped > 0
    }
}

impl fmt::Display for Output {
    /// The rendered form carries the truncation marker: whoever reads the
    /// output must see that it is not the whole story.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)?;
        if self.truncated() {
            if !self.text.ends_with('\n') {
                f.write_str("\n")?;
            }
            write!(f, "[output truncated: {} more bytes]", self.dropped)?;
        }
        Ok(())
    }
}

/// A command that ran to completion, however it exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub output: Output,
    pub exit_code: u32,
    pub success: bool,
}

/// Why a pty run produced no exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyError {
    /// The pty pair could not be opened or the shell could not be spawned.
    Spawn { reason: String },
    /// The master side could not be read from or written to.
    Io { reason: String },
    /// The command outlived its deadline and was killed. The output it had
    /// produced by then is kept: a timed-out build still says where it hung.
    TimedOut { after: Duration, output: Output },
    /// An [`Interrupt`] was raised and the command was stopped.
    Interrupted { output: Output },
}

impl fmt::Display for PtyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { reason } => write!(f, "pty spawn failed: {reason}"),
            Self::Io { reason } => write!(f, "pty io failed: {reason}"),
            Self::TimedOut { after, output } => write!(
                f,
                "command timed out after {}s and was killed\n{output}",
                after.as_secs_f32()
            ),
            Self::Interrupted { output } => write!(f, "command interrupted\n{output}"),
        }
    }
}

impl std::error::Error for PtyError {}

/// Bounds and terminal shape for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub timeout: Duration,
    pub cap: usize,
    pub rows: u16,
    pub cols: u16,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            cap: OUTPUT_CAP,
            // Wide enough that a program which wraps to the window does not
            // fold `cargo` diagnostics into unreadable stubs.
            rows: 40,
            cols: 200,
        }
    }
}

/// Clamps a caller-supplied timeout into [`MIN_TIMEOUT_SECS`]..=[`MAX_TIMEOUT_SECS`].
pub fn clamp_timeout(secs: u64) -> Duration {
    Duration::from_secs(secs.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS))
}

#[derive(Default)]
struct Sink {
    bytes: Vec<u8>,
    dropped: usize,
}

/// Runs `command` through `sh -c` on a pty rooted at `cwd`.
///
/// Blocking: the pty master has no async surface and this crate holds no
/// runtime, the same way [`crate::BashTool`]'s pipe path blocks.
pub fn run(
    command: &str,
    cwd: &Path,
    options: &Options,
    interrupt: &Interrupt,
) -> Result<Run, PtyError> {
    // A cancel that landed while this call was queued still means stop: the
    // command must not start at all.
    if interrupt.is_raised() {
        return Err(PtyError::Interrupted {
            output: Output::default(),
        });
    }

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: options.rows,
            cols: options.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| PtyError::Spawn {
            reason: error.to_string(),
        })?;

    let mut builder = CommandBuilder::new("sh");
    builder.arg("-c");
    builder.arg(command);
    builder.cwd(cwd);

    let mut child = pair
        .slave
        .spawn_command(builder)
        .map_err(|error| PtyError::Spawn {
            reason: error.to_string(),
        })?;
    // The reader sees EOF only once no one holds the slave open any more.
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| PtyError::Io {
            reason: error.to_string(),
        })?;
    let mut writer = pair.master.take_writer().map_err(|error| PtyError::Io {
        reason: error.to_string(),
    })?;

    let sink = Arc::new(Mutex::new(Sink::default()));
    let drained = Arc::new(AtomicBool::new(false));
    let cap = options.cap;
    let reader_sink = Arc::clone(&sink);
    let reader_drained = Arc::clone(&drained);
    // Detached on purpose: it ends on EOF, and a command whose grandchild
    // holds the pty open must not keep the turn hostage.
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        // A pty master reports the slave's last close as `EIO`, so a read
        // error here is the end of the stream, not a fault to report.
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 {
                break;
            }
            let Ok(mut sink) = reader_sink.lock() else {
                break;
            };
            let room = cap.saturating_sub(sink.bytes.len());
            let taken = room.min(read);
            sink.bytes.extend_from_slice(&buffer[..taken]);
            sink.dropped += read - taken;
        }
        reader_drained.store(true, Ordering::SeqCst);
    });

    let deadline = Instant::now() + options.timeout;
    loop {
        match child.try_wait() {
            Err(error) => {
                stop(child.as_mut(), writer.as_mut());
                return Err(PtyError::Io {
                    reason: error.to_string(),
                });
            }
            Ok(Some(status)) => {
                await_drain(&drained);
                return Ok(Run {
                    output: collect(&sink),
                    exit_code: status.exit_code(),
                    success: status.success(),
                });
            }
            Ok(None) => {}
        }
        if interrupt.is_raised() {
            stop(child.as_mut(), writer.as_mut());
            await_drain(&drained);
            return Err(PtyError::Interrupted {
                output: collect(&sink),
            });
        }
        if Instant::now() >= deadline {
            stop(child.as_mut(), writer.as_mut());
            await_drain(&drained);
            return Err(PtyError::TimedOut {
                after: options.timeout,
                output: collect(&sink),
            });
        }
        std::thread::sleep(POLL);
    }
}

/// Ctrl+C first so the whole foreground process group gets `SIGINT` and the
/// command can clean up; `SIGKILL` for whatever ignores it.
fn stop(child: &mut (dyn Child + Send + Sync), writer: &mut dyn Write) {
    let _ = writer.write_all(&[CTRL_C]);
    let _ = writer.flush();
    let grace = Instant::now() + SIGINT_GRACE;
    while Instant::now() < grace {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(POLL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Output written just before the child exited is still in flight in the
/// reader thread; this waits a bounded moment for it.
fn await_drain(drained: &AtomicBool) {
    let deadline = Instant::now() + DRAIN_GRACE;
    while !drained.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
}

fn collect(sink: &Mutex<Sink>) -> Output {
    let Ok(sink) = sink.lock() else {
        return Output::default();
    };
    Output {
        // A pty turns every `\n` into `\r\n` on the way out; the carriage
        // returns are terminal plumbing and would only confuse a reader.
        text: String::from_utf8_lossy(&sink.bytes).replace("\r\n", "\n"),
        dropped: sink.dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast(timeout: Duration) -> Options {
        Options {
            timeout,
            ..Options::default()
        }
    }

    #[test]
    fn a_command_returns_its_output() {
        let run = run(
            "echo hello-pty",
            Path::new("."),
            &fast(Duration::from_secs(10)),
            &Interrupt::new(),
        )
        .unwrap_or_else(|error| panic!("run: {error}"));
        assert!(run.success, "exit {}", run.exit_code);
        assert_eq!(run.output.text.trim_end(), "hello-pty");
        assert!(!run.output.truncated());
    }

    #[test]
    fn a_failing_command_keeps_its_exit_code() {
        let run = run(
            "echo nope >&2; exit 3",
            Path::new("."),
            &fast(Duration::from_secs(10)),
            &Interrupt::new(),
        )
        .unwrap_or_else(|error| panic!("run: {error}"));
        assert!(!run.success);
        assert_eq!(run.exit_code, 3);
        assert!(run.output.text.contains("nope"));
    }

    #[test]
    fn the_command_sees_a_terminal() {
        let run = run(
            "test -t 1 && echo tty",
            Path::new("."),
            &fast(Duration::from_secs(10)),
            &Interrupt::new(),
        )
        .unwrap_or_else(|error| panic!("run: {error}"));
        assert_eq!(run.output.text.trim_end(), "tty");
    }

    #[test]
    fn a_command_past_its_deadline_is_killed_and_leaves_no_orphan() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let marker = dir.path().join("survived");
        let error = run(
            "echo starting; sleep 1; touch survived",
            dir.path(),
            &fast(Duration::from_millis(300)),
            &Interrupt::new(),
        )
        .expect_err("a 1s command must not finish inside a 300ms deadline");
        match &error {
            PtyError::TimedOut { after, output } => {
                assert_eq!(*after, Duration::from_millis(300));
                assert!(
                    output.text.contains("starting"),
                    "output before the kill is kept: {output}"
                );
            }
            other => panic!("expected a timeout, got {other}"),
        }
        assert!(error.to_string().contains("timed out"));
        // Long past the sleep: nothing was left running to touch the marker.
        std::thread::sleep(Duration::from_millis(1_200));
        assert!(
            !marker.exists(),
            "the killed command's tail still ran: {}",
            marker.display()
        );
    }

    #[test]
    fn a_raised_interrupt_stops_a_running_command() {
        let interrupt = Interrupt::new();
        let armed = interrupt.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            armed.raise();
        });
        let started = Instant::now();
        let error = run(
            "sleep 30",
            Path::new("."),
            &fast(Duration::from_secs(30)),
            &interrupt,
        )
        .expect_err("the interrupt must cut the command short");
        assert!(matches!(error, PtyError::Interrupted { .. }), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn output_past_the_cap_is_truncated_with_a_marker() {
        let options = Options {
            timeout: Duration::from_secs(20),
            cap: 256,
            ..Options::default()
        };
        let run = run(
            "for i in $(seq 1 400); do echo 0123456789; done",
            Path::new("."),
            &options,
            &Interrupt::new(),
        )
        .unwrap_or_else(|error| panic!("run: {error}"));
        assert!(run.success);
        assert!(run.output.text.len() <= 256);
        assert!(run.output.truncated());
        assert!(
            run.output.to_string().contains("[output truncated:"),
            "rendered: {}",
            run.output
        );
    }
}
