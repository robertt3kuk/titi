//! Running a shell command over pipes, bounded the way [`crate::pty`] is.
//!
//! This is `bash`'s default path: output comes back unwrapped and
//! uncoloured. It used to wait on the command with no deadline, so a dev
//! server, a `--watch` or a `sleep 999` held the turn until the user killed
//! titi. Now a run ends on its deadline or on an [`Interrupt`], whichever
//! comes first, and either way its whole process group goes down with it:
//! `sh -c` is only the first process of a pipeline, and killing it alone
//! would leave the rest running.
//!
//! A command still running when the background threshold passes is not
//! killed: it is handed back as a [`Background`], which keeps the group and
//! the readers alive for whoever owns the job. The deadline remains the
//! bound of the *foreground* wait, so a call whose deadline is under the
//! threshold behaves exactly as it always did.

use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smol_str::SmolStr;
use thiserror::Error;

use crate::pty::Interrupt;

/// Bytes kept from the start of each stream, and again from its end. A
/// build log puts its error last and a listing its point first, so both ends
/// survive and only the middle is dropped.
pub const KEEP_EACH_END: usize = 64 * 1024;

/// How long a command may hold the turn before it is handed to the
/// background, where its output reaches the session when it ends instead of
/// dying at the deadline with the output lost. Overridden per process by
/// [`BACKGROUND_ENV`], then by the `bash.autoBackground.thresholdMs` setting
/// (read by the surface and passed through [`background_after_with`]).
pub const BACKGROUND_AFTER: Duration = Duration::from_secs(60);

/// Environment variable holding that threshold in milliseconds, as in
/// `TITI_BASH_BACKGROUND_MS=200`. For tests, and for a machine that wants a
/// different bound without a settings key; it wins over the setting.
pub const BACKGROUND_ENV: &str = "TITI_BASH_BACKGROUND_MS";

/// The threshold this process runs with: [`BACKGROUND_ENV`] in milliseconds
/// when it parses, else the setting, else [`BACKGROUND_AFTER`]. A value that
/// does not parse is skipped, not an error: a typo in the environment must not
/// stop `bash`.
pub fn background_after_with(setting: Option<Duration>) -> Duration {
    background_after_with_of(|key| std::env::var(key).ok(), setting)
}

/// [`background_after_with`] over an injected lookup, so the parse and the
/// env-over-setting precedence are testable without touching the process
/// environment.
fn background_after_with_of(
    lookup: impl Fn(&str) -> Option<String>,
    setting: Option<Duration>,
) -> Duration {
    lookup(BACKGROUND_ENV)
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .or(setting)
        .unwrap_or(BACKGROUND_AFTER)
}

/// The threshold from the environment alone, ignoring any setting: the
/// engine's fallback when no surface passed one.
pub fn background_after() -> Duration {
    background_after_with(None)
}

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

/// What a bounded run produced.
pub enum Outcome {
    /// The command finished, or its deadline or the interrupt stopped it.
    Done(Run),
    /// The command outlived the background threshold and is still running.
    /// The caller owns it now.
    Backgrounded(Background),
}

/// Where a command that outlived the turn's threshold goes.
///
/// The engine installs one on the tools it runs: it mints the job id, records
/// the job so `/jobs` and `/jobs cancel` cover it, and waits for the command
/// off the turn's thread so its output reaches the session when it ends. With
/// none installed a command is bounded by its deadline exactly as before —
/// nothing is ever handed to a session that could not report it.
pub trait BackgroundSink: Send + Sync + 'static {
    /// How long the turn waits before handing a command over.
    fn after(&self) -> Duration;

    /// Takes a command that outlived [`BackgroundSink::after`] and returns the
    /// job id the model is told about. The command keeps running; the sink
    /// reports what it printed, and how it exited, when it ends.
    fn hand_over(&self, command: &str, background: Background) -> SmolStr;
}

/// A command that outlived the turn's threshold, still running with its own
/// process group and its readers attached.
pub struct Background {
    child: Child,
    streams: Streams,
    cancelled: Arc<AtomicBool>,
}

impl Background {
    /// A handle that stops this command's group from wherever the job is
    /// recorded, the way the deadline would have. Idempotent.
    pub fn cancel(&self) -> BackgroundCancel {
        BackgroundCancel {
            cancelled: Arc::clone(&self.cancelled),
        }
    }

    /// Blocks until the command exits or its [`BackgroundCancel`] stops it,
    /// and renders the same [`Run`] the foreground path would have. No
    /// deadline and no [`Interrupt`]: out of the turn, only its own cancel
    /// ends it.
    pub fn wait(mut self) -> Run {
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    return Run {
                        output: self.streams.render(),
                        exit_code: status.code(),
                        success: status.success(),
                    };
                }
                Ok(None) => {}
                Err(error) => {
                    stop(&mut self.child);
                    return Run {
                        output: format!(
                            "could not wait on the command: {error}\n{}",
                            self.streams.render()
                        ),
                        exit_code: None,
                        success: false,
                    };
                }
            }
            if self.cancelled.load(Ordering::SeqCst) {
                stop(&mut self.child);
                return Run {
                    output: self.streams.render(),
                    exit_code: None,
                    success: false,
                };
            }
            std::thread::sleep(POLL);
        }
    }
}

/// Stops a [`Background`] command from outside the thread waiting on it.
#[derive(Clone, Debug, Default)]
pub struct BackgroundCancel {
    cancelled: Arc<AtomicBool>,
}

impl BackgroundCancel {
    /// Stops the command and everything it started. Idempotent.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
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
/// Blocking, like [`crate::pty::run`]: this crate holds no runtime. Bounded by
/// its deadline and by an [`Interrupt`]; nothing is handed to the background,
/// because the caller has nowhere to keep a running command.
/// [`run_with_background`] is the same run with a threshold.
pub fn run(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    interrupt: &Interrupt,
) -> Result<Run, PipeError> {
    match run_with_background(command, cwd, timeout, None, interrupt)? {
        Outcome::Done(run) => Ok(run),
        // No threshold was asked for, so nothing can be handed over.
        Outcome::Backgrounded(background) => Ok(background.wait()),
    }
}

/// [`run`] with a background threshold: a command still running when
/// `background_after` passes is handed back instead of killed.
///
/// The threshold never outranks the interrupt or a deadline under it: a call
/// that would have timed out before the threshold still does, so the deadline
/// stays the outer bound of the foreground wait.
pub fn run_with_background(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    background_after: Option<Duration>,
    interrupt: &Interrupt,
) -> Result<Outcome, PipeError> {
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
    let streams = Streams {
        stdout: Reader::spawn(child.stdout.take()),
        stderr: Reader::spawn(child.stderr.take()),
    };

    let deadline = Instant::now() + timeout;
    let background_at = background_after.map(|after| Instant::now() + after);
    loop {
        match child.try_wait() {
            Err(error) => {
                stop(&mut child);
                return Err(PipeError::Wait {
                    reason: error.to_string(),
                });
            }
            Ok(Some(status)) => {
                return Ok(Outcome::Done(Run {
                    output: streams.render(),
                    exit_code: status.code(),
                    success: status.success(),
                }));
            }
            Ok(None) => {}
        }
        if interrupt.raised_since(mark) {
            stop(&mut child);
            return Err(PipeError::Interrupted {
                output: streams.render(),
            });
        }
        if background_at.is_some_and(|at| Instant::now() >= at) {
            // Hand the live command over instead of stopping it: it keeps its
            // group and its output still goes somewhere.
            return Ok(Outcome::Backgrounded(Background {
                child,
                streams,
                cancelled: Arc::new(AtomicBool::new(false)),
            }));
        }
        if Instant::now() >= deadline {
            stop(&mut child);
            return Err(PipeError::TimedOut {
                after: timeout,
                output: streams.render(),
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

/// The two readers of one run, and the rendering of what they captured. Both
/// the foreground answer and a backgrounded command's answer come from here,
/// so the two cannot drift apart.
struct Streams {
    stdout: Reader,
    stderr: Reader,
}

impl Streams {
    fn render(&self) -> String {
        let deadline = Instant::now() + DRAIN_GRACE;
        while !(self.stdout.drained() && self.stderr.drained()) && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        let mut text = self.stdout.render();
        let errors = self.stderr.render();
        if !text.is_empty() && !errors.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&errors);
        text
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

    /// The threshold is milliseconds in the environment, and a value it cannot
    /// read is the next layer rather than an error.
    #[test]
    fn the_background_threshold_comes_from_the_environment() {
        assert_eq!(background_after_with_of(|_| None, None), BACKGROUND_AFTER);
        assert_eq!(
            background_after_with_of(|_| Some(" 250 ".into()), None),
            Duration::from_millis(250)
        );
        assert_eq!(
            background_after_with_of(|_| Some("soon".into()), None),
            BACKGROUND_AFTER
        );
    }

    /// `bash.autoBackground.thresholdMs` sets the threshold when no env value
    /// is readable, and the env variable still wins when it is: that is the
    /// precedence the engine relies on, so a stale environment cannot be
    /// overridden by a config layer.
    #[test]
    fn the_setting_sets_the_threshold_and_the_environment_still_wins() {
        let setting = Some(Duration::from_millis(250));
        assert_eq!(
            background_after_with_of(|_| None, setting),
            Duration::from_millis(250)
        );
        assert_eq!(
            background_after_with_of(|_| Some("400".into()), setting),
            Duration::from_millis(400)
        );
        // An environment value that does not parse falls through to the
        // setting instead of pinning the default.
        assert_eq!(
            background_after_with_of(|_| Some("soon".into()), setting),
            Duration::from_millis(250)
        );
        assert_eq!(background_after_with_of(|_| None, None), BACKGROUND_AFTER);
    }

    /// A command under the threshold is a [`Run`], byte for byte the one the
    /// plain path returns — the threshold changes nothing for a short command.
    #[test]
    fn a_command_under_the_threshold_is_unchanged() {
        let outcome = run_with_background(
            "echo out; echo err >&2; exit 3",
            Path::new("."),
            Duration::from_secs(20),
            Some(Duration::from_secs(5)),
            &Interrupt::new(),
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let Outcome::Done(run) = outcome else {
            panic!("a command that finishes inside the threshold must not be backgrounded");
        };
        assert_eq!(
            run,
            Run {
                output: "out\nerr\n".to_owned(),
                exit_code: Some(3),
                success: false,
            }
        );
    }

    /// Past the threshold the call comes back at once, with the command still
    /// running; the answer arrives when it finishes, not when the turn gave up
    /// on it.
    #[test]
    fn a_command_past_the_threshold_is_handed_over_running() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let started = Instant::now();
        let outcome = run_with_background(
            "echo starting; sleep 1; echo finished",
            dir.path(),
            Duration::from_secs(30),
            Some(Duration::from_millis(200)),
            &Interrupt::new(),
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let Outcome::Backgrounded(background) = outcome else {
            panic!("a command past the threshold must be handed over");
        };
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the call waited on the command: {:?}",
            started.elapsed()
        );
        let run = background.wait();
        assert!(run.success, "{run:?}");
        assert_eq!(run.output, "starting\nfinished\n");
    }

    /// A cancel is aimed at the turn, and the turn is no longer waiting: the
    /// handed-over command runs to completion and its output still arrives.
    #[test]
    fn a_raised_interrupt_does_not_touch_a_handed_over_command() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let interrupt = Interrupt::new();
        let outcome = run_with_background(
            "sleep 1; echo survived",
            dir.path(),
            Duration::from_secs(30),
            Some(Duration::from_millis(100)),
            &interrupt,
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let Outcome::Backgrounded(background) = outcome else {
            panic!("a command past the threshold must be handed over");
        };
        // What the turn's cancel does, a moment after the hand-over.
        interrupt.raise();
        interrupt.clear();
        let run = background.wait();
        assert!(run.success, "the cancel reached the background: {run:?}");
        assert_eq!(run.output, "survived\n");
    }

    /// The job's own cancel does reach it, group and all: the shell's `TERM`
    /// trap runs, which is the marker the test reads instead of `ps` output.
    #[test]
    fn a_background_cancel_kills_the_group() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let marker = dir.path().join("trapped");
        let command = format!(
            "trap 'touch {}; exit 3' TERM; sleep 30; touch survived",
            marker.display()
        );
        let outcome = run_with_background(
            &command,
            dir.path(),
            Duration::from_secs(30),
            Some(Duration::from_millis(100)),
            &Interrupt::new(),
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let Outcome::Backgrounded(background) = outcome else {
            panic!("a command past the threshold must be handed over");
        };
        let cancel = background.cancel();
        cancel.cancel();
        let started = Instant::now();
        let run = background.wait();
        assert!(
            !run.success,
            "a cancelled command is not a success: {run:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(marker.exists(), "the group never got SIGTERM");
        assert!(!dir.path().join("survived").exists());
    }
}
