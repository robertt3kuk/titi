//! The `project` tool: run the workspace's own test, lint, typecheck, build
//! and format check without the model guessing the command.
//!
//! The ecosystem is read from the files that own a directory (`Cargo.toml`,
//! `package.json`, `go.mod`, ...), the command from what that ecosystem
//! ships, and the run goes through [`BashTool`], so the deadline, the process
//! group kill, the output caps, the ANSI stripping and the interrupt are the
//! ones `bash` already has. `list` resolves everything and runs nothing.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;
use titi_providers::ToolSpec;

use crate::fs::{BashTool, DESCRIBE_MAX, arg_str, describe_line, err, jail_path, ok};
use crate::pty::Interrupt;
use crate::{ApprovalTier, ToolDefinition, ToolHandler, ToolResult};

/// Files whose presence makes a directory a project, in the order they are
/// reported. `list` and the "nothing found" error both print this.
const MARKERS: &str = "Cargo.toml, package.json, go.mod, pyproject.toml, setup.py, setup.cfg, \
                       pytest.ini, requirements.txt, Makefile";

/// Lines of output a failed step keeps from the end, which is where every
/// test runner prints its summary.
const FAIL_TAIL_LINES: usize = 15;
/// Lines of output a passing step keeps from the end.
const OK_TAIL_LINES: usize = 6;
/// Failure-looking lines pulled from the part of the output the tail misses.
const KEY_LINES: usize = 20;
/// Characters of one output line a block shows.
const LINE_CHARS: usize = 200;
/// Characters of output one step's block carries.
const BLOCK_CHARS: usize = 6000;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProjectError {
    #[error("missing action; one of list, check, test, lint, typecheck, build, format")]
    MissingAction,
    #[error("unknown action `{0}`; one of list, check, test, lint, typecheck, build, format")]
    UnknownAction(String),
    #[error("{0}")]
    Path(String),
    #[error("{0} does not exist")]
    Missing(String),
    #[error("invalid filter: {0}")]
    Filter(&'static str),
    #[error(
        "no project found in {searched}; looked for {MARKERS} there and in each parent up to \
         the workspace root"
    )]
    NoProject { searched: String },
}

/// One thing a project can do for the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Test,
    Lint,
    Typecheck,
    Build,
    Format,
}

impl Stage {
    /// The order `list` prints them in.
    const ALL: [Stage; 5] = [
        Stage::Test,
        Stage::Lint,
        Stage::Typecheck,
        Stage::Build,
        Stage::Format,
    ];

    /// What `check` runs, in order: the cheapest signal first.
    const CHECK: [Stage; 3] = [Stage::Typecheck, Stage::Lint, Stage::Test];

    fn name(self) -> &'static str {
        match self {
            Stage::Test => "test",
            Stage::Lint => "lint",
            Stage::Typecheck => "typecheck",
            Stage::Build => "build",
            Stage::Format => "format",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    List,
    Check,
    Run(Stage),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Request {
    action: Action,
    path: Option<String>,
    filter: Option<String>,
}

impl Request {
    fn parse(args: &Value) -> Result<Self, ProjectError> {
        let action = match arg_str(args, "action").as_deref() {
            None => return Err(ProjectError::MissingAction),
            Some("list") => Action::List,
            Some("check") => Action::Check,
            Some("test") => Action::Run(Stage::Test),
            Some("lint") => Action::Run(Stage::Lint),
            Some("typecheck") => Action::Run(Stage::Typecheck),
            Some("build") => Action::Run(Stage::Build),
            Some("format") => Action::Run(Stage::Format),
            Some(other) => return Err(ProjectError::UnknownAction(other.to_owned())),
        };
        let filter = arg_str(args, "filter").filter(|filter| !filter.is_empty());
        if let Some(filter) = &filter {
            // A leading `-` would be read as a flag of the test runner, not
            // as the name it narrows to.
            if filter.starts_with('-') {
                return Err(ProjectError::Filter("must not start with `-`"));
            }
            if filter.chars().any(char::is_control) {
                return Err(ProjectError::Filter("must be a single line"));
            }
        }
        Ok(Self {
            action,
            path: arg_str(args, "path").filter(|path| !path.is_empty()),
            filter,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Manager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl Manager {
    fn name(self) -> &'static str {
        match self {
            Manager::Npm => "npm",
            Manager::Pnpm => "pnpm",
            Manager::Yarn => "yarn",
            Manager::Bun => "bun",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ecosystem {
    Cargo {
        /// The manifest is a workspace root, so a bare `cargo test` would
        /// only reach its default members.
        workspace: bool,
    },
    Node {
        manager: Manager,
        scripts: BTreeMap<String, String>,
    },
    Go,
    Python {
        pytest: bool,
        ruff: bool,
        mypy: bool,
    },
    Make {
        targets: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Detected {
    ecosystem: Ecosystem,
    /// The file that made it one.
    marker: String,
}

impl Detected {
    fn label(&self) -> &'static str {
        match &self.ecosystem {
            Ecosystem::Cargo { .. } => "cargo",
            Ecosystem::Node { manager, .. } => manager.name(),
            Ecosystem::Go => "go",
            Ecosystem::Python { .. } => "python",
            Ecosystem::Make { .. } => "make",
        }
    }

    /// The exact command for `stage`, or why this ecosystem has none.
    /// `installed` answers whether a tool is on `PATH`.
    fn command(
        &self,
        stage: Stage,
        filter: Option<&str>,
        installed: &dyn Fn(&str) -> bool,
    ) -> Result<String, String> {
        // Only a test run is narrowed.
        let filter = filter.filter(|_| stage == Stage::Test).map(sh_word);
        match &self.ecosystem {
            Ecosystem::Cargo { workspace } => {
                let scope = if *workspace { " --workspace" } else { "" };
                Ok(match stage {
                    Stage::Test => match filter {
                        Some(filter) => format!("cargo test{scope} {filter}"),
                        None => format!("cargo test{scope}"),
                    },
                    Stage::Lint => format!("cargo clippy{scope} --all-targets"),
                    Stage::Typecheck => format!("cargo check{scope} --all-targets"),
                    Stage::Build => format!("cargo build{scope}"),
                    Stage::Format if *workspace => "cargo fmt --all --check".to_owned(),
                    Stage::Format => "cargo fmt --check".to_owned(),
                })
            }
            Ecosystem::Node { manager, scripts } => {
                let names: &[&str] = match stage {
                    Stage::Test => &["test"],
                    Stage::Lint => &["lint"],
                    Stage::Typecheck => &["typecheck", "type-check", "check-types"],
                    Stage::Build => &["build"],
                    Stage::Format => &["format:check", "format-check", "check-format", "fmt:check"],
                };
                let Some(script) = names.iter().find(|name| {
                    scripts
                        .get(**name)
                        .is_some_and(|body| !is_placeholder_script(body))
                }) else {
                    return Err(format!(
                        "package.json has no {} script",
                        names
                            .iter()
                            .map(|name| format!("`{name}`"))
                            .collect::<Vec<_>>()
                            .join(" or ")
                    ));
                };
                let script = sh_word(script);
                let npm = *manager == Manager::Npm;
                let manager = manager.name();
                Ok(match (filter, npm) {
                    (Some(filter), true) => format!("npm run {script} -- {filter}"),
                    (Some(filter), false) => format!("{manager} run {script} {filter}"),
                    (None, _) => format!("{manager} run {script}"),
                })
            }
            Ecosystem::Go => Ok(match stage {
                Stage::Test => match filter {
                    Some(filter) => format!("go test -run {filter} ./..."),
                    None => "go test ./...".to_owned(),
                },
                Stage::Lint => "go vet ./...".to_owned(),
                Stage::Typecheck | Stage::Build => "go build ./...".to_owned(),
                // `gofmt -l` lists and still exits 0, so the exit code is made.
                Stage::Format => "test -z \"$(gofmt -l .)\" || { gofmt -l .; exit 1; }".to_owned(),
            }),
            Ecosystem::Python { pytest, ruff, mypy } => match stage {
                Stage::Test if *pytest || installed("pytest") => Ok(match filter {
                    Some(filter) => format!("pytest -k {filter}"),
                    None => "pytest".to_owned(),
                }),
                Stage::Test => Err("pytest is neither configured nor installed".to_owned()),
                Stage::Lint if *ruff || installed("ruff") => Ok("ruff check .".to_owned()),
                Stage::Lint => Err("ruff is neither configured nor installed".to_owned()),
                Stage::Format if *ruff || installed("ruff") => {
                    Ok("ruff format --check .".to_owned())
                }
                Stage::Format => Err("ruff is neither configured nor installed".to_owned()),
                Stage::Typecheck if *mypy || installed("mypy") => Ok("mypy .".to_owned()),
                Stage::Typecheck => Err("mypy is neither configured nor installed".to_owned()),
                Stage::Build => Err("python has no build command".to_owned()),
            },
            Ecosystem::Make { targets } => {
                if filter.is_some() {
                    return Err("make targets take no filter".to_owned());
                }
                let names: &[&str] = match stage {
                    Stage::Test => &["test"],
                    Stage::Lint => &["lint"],
                    Stage::Typecheck => &["typecheck", "check"],
                    Stage::Build => &["build"],
                    Stage::Format => &["format-check", "fmt-check", "check-format"],
                };
                match names
                    .iter()
                    .find(|name| targets.iter().any(|target| target == **name))
                {
                    Some(target) => Ok(format!("make {target}")),
                    None => Err(format!(
                        "the Makefile has no {} target",
                        names
                            .iter()
                            .map(|name| format!("`{name}`"))
                            .collect::<Vec<_>>()
                            .join(" or ")
                    )),
                }
            }
        }
    }
}

/// `npm init`'s stand-in for a test script. It exits 1 on purpose, so
/// running it would report a failing suite that does not exist.
fn is_placeholder_script(body: &str) -> bool {
    body.contains("no test specified")
}

/// A shell word for `text`: bare when it needs no quoting, otherwise single
/// quoted. The filter comes from the model, so it must never be shell syntax.
fn sh_word(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "_-./:=@%+,".contains(ch));
    if plain {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// The directory that owns a request and what it is built with.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Project {
    dir: PathBuf,
    /// `dir` relative to the workspace root, `.` for the root itself.
    rel: String,
    found: Vec<Detected>,
}

/// One command a stage resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Step {
    stage: Stage,
    ecosystem: &'static str,
    command: String,
}

/// What a stage resolves to in a project: the commands to run, and why each
/// ecosystem that offered none had none.
#[derive(Debug, Default, PartialEq, Eq)]
struct StagePlan {
    steps: Vec<Step>,
    skips: Vec<String>,
}

impl Project {
    /// Every ecosystem's command for `stage`. A Makefile only fills in what
    /// no other ecosystem supplies: it usually wraps one of them, and running
    /// both would run the same suite twice.
    fn plan(
        &self,
        stage: Stage,
        filter: Option<&str>,
        installed: &dyn Fn(&str) -> bool,
    ) -> StagePlan {
        let mut plan = StagePlan::default();
        for make in [false, true] {
            if make && !plan.steps.is_empty() {
                break;
            }
            for detected in &self.found {
                if matches!(detected.ecosystem, Ecosystem::Make { .. }) != make {
                    continue;
                }
                match detected.command(stage, filter, installed) {
                    Ok(command) => plan.steps.push(Step {
                        stage,
                        ecosystem: detected.label(),
                        command,
                    }),
                    Err(reason) => plan.skips.push(format!("{}: {reason}", detected.label())),
                }
            }
        }
        plan
    }

    fn names(&self) -> String {
        let labels: Vec<String> = self
            .found
            .iter()
            .map(|detected| format!("{} ({})", detected.label(), detected.marker))
            .collect();
        labels.join(", ")
    }
}

/// The directory a request is about: the workspace root, or the nearest
/// directory at or above `path` that holds a project file. A monorepo's
/// package owns its own files; the root's tooling does not.
fn locate(root: &Path, path: Option<&str>) -> Result<Project, ProjectError> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let start = match path {
        None => root.clone(),
        Some(raw) => {
            let resolved = jail_path(&root, raw).map_err(ProjectError::Path)?;
            if !resolved.exists() {
                return Err(ProjectError::Missing(raw.to_owned()));
            }
            if resolved.is_file() {
                resolved.parent().map_or(root.clone(), Path::to_path_buf)
            } else {
                resolved
            }
        }
    };
    let mut dir = start.clone();
    loop {
        let found = detect(&dir, &root);
        if !found.is_empty() {
            return Ok(Project {
                rel: relative(&root, &dir),
                dir,
                found,
            });
        }
        match dir.parent() {
            Some(parent) if dir != root && parent.starts_with(&root) => dir = parent.to_path_buf(),
            _ => {
                return Err(ProjectError::NoProject {
                    searched: relative(&root, &start),
                });
            }
        }
    }
}

fn relative(root: &Path, dir: &Path) -> String {
    let rel = dir.strip_prefix(root).unwrap_or(dir);
    if rel.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        rel.to_string_lossy().replace('\\', "/")
    }
}

/// The ecosystems `dir` itself holds files for. `root` bounds the search for
/// the lockfile a package of a monorepo shares with its root.
fn detect(dir: &Path, root: &Path) -> Vec<Detected> {
    let text = |name: &str| fs::read_to_string(dir.join(name)).ok();
    let mut found = Vec::new();

    if let Some(manifest) = text("Cargo.toml") {
        found.push(Detected {
            ecosystem: Ecosystem::Cargo {
                workspace: manifest.contains("[workspace]"),
            },
            marker: "Cargo.toml".to_owned(),
        });
    }

    if let Some(manifest) = text("package.json") {
        let scripts = serde_json::from_str::<Value>(&manifest)
            .ok()
            .and_then(|manifest| manifest.get("scripts").cloned())
            .and_then(|scripts| scripts.as_object().cloned())
            .map(|scripts| {
                scripts
                    .into_iter()
                    .filter_map(|(name, body)| Some((name, body.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        found.push(Detected {
            ecosystem: Ecosystem::Node {
                manager: package_manager(dir, root),
                scripts,
            },
            marker: "package.json".to_owned(),
        });
    }

    if dir.join("go.mod").is_file() {
        found.push(Detected {
            ecosystem: Ecosystem::Go,
            marker: "go.mod".to_owned(),
        });
    }

    let python_marker = [
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "pytest.ini",
        "requirements.txt",
    ]
    .into_iter()
    .find(|name| dir.join(name).is_file());
    if let Some(marker) = python_marker {
        let pyproject = text("pyproject.toml").unwrap_or_default();
        let setup_cfg = text("setup.cfg").unwrap_or_default();
        let tox = text("tox.ini").unwrap_or_default();
        found.push(Detected {
            ecosystem: Ecosystem::Python {
                pytest: dir.join("pytest.ini").is_file()
                    || pyproject.contains("[tool.pytest")
                    || setup_cfg.contains("[tool:pytest]")
                    || tox.contains("[pytest]"),
                ruff: dir.join("ruff.toml").is_file()
                    || dir.join(".ruff.toml").is_file()
                    || pyproject.contains("[tool.ruff"),
                mypy: dir.join("mypy.ini").is_file()
                    || dir.join(".mypy.ini").is_file()
                    || pyproject.contains("[tool.mypy")
                    || setup_cfg.contains("[mypy"),
            },
            marker: marker.to_owned(),
        });
    }

    if let Some(name) = ["Makefile", "makefile", "GNUmakefile"]
        .into_iter()
        .find(|name| dir.join(name).is_file())
    {
        found.push(Detected {
            ecosystem: Ecosystem::Make {
                targets: make_targets(&text(name).unwrap_or_default()),
            },
            marker: name.to_owned(),
        });
    }
    found
}

/// The package manager whose lockfile is nearest, looking from `dir` up to
/// `root`: a workspace package has no lockfile of its own. Without any, npm,
/// which every Node install carries.
fn package_manager(dir: &Path, root: &Path) -> Manager {
    const LOCKFILES: [(&str, Manager); 5] = [
        ("bun.lockb", Manager::Bun),
        ("bun.lock", Manager::Bun),
        ("pnpm-lock.yaml", Manager::Pnpm),
        ("yarn.lock", Manager::Yarn),
        ("package-lock.json", Manager::Npm),
    ];
    let mut at = Some(dir);
    while let Some(current) = at {
        if let Some((_, manager)) = LOCKFILES
            .iter()
            .find(|(name, _)| current.join(name).is_file())
        {
            return *manager;
        }
        at = if current == root {
            None
        } else {
            current.parent().filter(|parent| parent.starts_with(root))
        };
    }
    Manager::Npm
}

/// The targets a Makefile declares: `name:` and `a b:` at the start of a
/// line. Variable assignments (`X := 1`), recipes (indented), special
/// targets (`.PHONY`) and pattern rules (`%.o`) are not targets to run.
fn make_targets(makefile: &str) -> Vec<String> {
    let mut targets = Vec::new();
    for line in makefile.lines() {
        if line.starts_with(['\t', ' ', '#', '.']) {
            continue;
        }
        let Some((head, rest)) = line.split_once(':') else {
            continue;
        };
        if rest.starts_with('=') || head.contains(['=', '$', '%']) {
            continue;
        }
        targets.extend(
            head.split_whitespace()
                .filter(|name| {
                    name.chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || "_-./".contains(ch))
                })
                .map(str::to_owned),
        );
    }
    targets
}

/// Whether `name` is an executable file in a `PATH` directory.
fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

/// What `list` prints: the owner, and every stage's exact command.
fn list(project: &Project, filter: Option<&str>, installed: &dyn Fn(&str) -> bool) -> String {
    let mut out = format!("project in {}: {}\n", project.rel, project.names());
    for stage in Stage::ALL {
        let plan = project.plan(stage, filter, installed);
        if plan.steps.is_empty() {
            out.push_str(&format!(
                "{:<10} – {}\n",
                stage.name(),
                plan.skips.join("; ")
            ));
        }
        for step in &plan.steps {
            out.push_str(&format!(
                "{:<10} {}  [{}]\n",
                stage.name(),
                step.command,
                step.ecosystem
            ));
        }
    }
    out.push_str("check      typecheck, lint, test — in that order, each reported on its own");
    out
}

/// How one command ended.
struct Ran {
    passed: bool,
    block: String,
}

/// One step's result block: `✓ test · cargo test · 12.3s` or `✕ …`, then
/// what the output says about it.
fn block(project: &Project, step: &Step, result: &ToolResult, seconds: f64) -> Ran {
    let passed = !result.is_error;
    let output: &str = &result.output;
    // A failure leads with its own line: `exit 101`, or the reason the
    // command was killed.
    let (status, body) = if passed {
        (None, output)
    } else {
        match output.split_once('\n') {
            Some((first, rest)) => (Some(first), rest),
            None => (Some(output), ""),
        }
    };
    let mut head = format!(
        "{} {} · {}",
        if passed { '✓' } else { '✕' },
        step.stage.name(),
        step.command
    );
    if project.rel != "." {
        head.push_str(&format!(" · {}", project.rel));
    }
    head.push_str(&format!(" · {seconds:.1}s"));
    if let Some(status) = status {
        head.push_str(&format!(" · {status}"));
    }
    let tail = if passed {
        summarize_ok(body)
    } else {
        summarize_failure(body)
    };
    let block = if tail.is_empty() {
        head
    } else {
        format!("{head}\n{tail}")
    };
    Ran { passed, block }
}

fn summarize_ok(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.len().saturating_sub(OK_TAIL_LINES);
    clip_block(lines[start..].iter().map(|line| clip_line(line)))
}

/// The failure first: the lines that name what broke, then the end of the
/// output, where runners print their totals.
fn summarize_failure(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.len().saturating_sub(FAIL_TAIL_LINES);

    let mut seen = BTreeSet::new();
    let mut keys: Vec<String> = Vec::new();
    let mut follow = false;
    for line in &lines[..start] {
        let key = is_failure_line(line) || (follow && line.trim_start().starts_with("-->"));
        follow = key && line.trim_start().starts_with("error");
        if key && seen.insert(line.trim()) {
            keys.push(clip_line(line));
        }
    }
    let more = keys.len().saturating_sub(KEY_LINES);
    keys.truncate(KEY_LINES);

    let mut out: Vec<String> = Vec::new();
    if !keys.is_empty() {
        out.push("errors:".to_owned());
        out.extend(keys);
        if more > 0 {
            out.push(format!("… {more} more"));
        }
        out.push("output tail:".to_owned());
    } else if start > 0 {
        out.push(format!("… {start} earlier lines"));
    }
    out.extend(lines[start..].iter().map(|line| clip_line(line)));
    clip_block(out.into_iter())
}

/// A line that says something failed, in the words the common runners use.
fn is_failure_line(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("error")
        || line.starts_with("Error")
        || line.starts_with("FAIL")
        || line.starts_with("--- FAIL")
        || line.starts_with("not ok")
        || line.starts_with("Diff in")
        || line.starts_with("E   ")
        || line.starts_with(['✕', '×', '●'])
        || line.contains(" FAILED")
        || line.contains("panicked at")
        || line.contains("AssertionError")
}

fn clip_line(line: &str) -> String {
    let line = line.trim_end();
    if line.chars().count() <= LINE_CHARS {
        return line.to_owned();
    }
    let mut clipped: String = line.chars().take(LINE_CHARS).collect();
    clipped.push('…');
    clipped
}

/// The lines joined, cut to [`BLOCK_CHARS`].
fn clip_block(lines: impl Iterator<Item = String>) -> String {
    let joined = lines.collect::<Vec<_>>().join("\n");
    let joined = joined.trim_matches('\n');
    if joined.chars().count() <= BLOCK_CHARS {
        return joined.to_owned();
    }
    let mut clipped: String = joined.chars().take(BLOCK_CHARS).collect();
    clipped.push('…');
    clipped
}

pub struct ProjectTool {
    pub root: PathBuf,
    /// Shared with `bash`, so the cancel key stops a build the same way.
    pub interrupt: Interrupt,
}

impl ProjectTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            interrupt: Interrupt::new(),
        }
    }

    fn request(&self, args: &Value) -> Result<(Request, Project), ProjectError> {
        let request = Request::parse(args)?;
        let project = locate(&self.root, request.path.as_deref())?;
        Ok((request, project))
    }

    /// The command goes to a `bash` rooted in the package directory, which
    /// is why a nested package needs no `cd` to quote.
    async fn run(&self, project: &Project, step: &Step) -> Ran {
        let mut bash = BashTool::new(&project.dir);
        bash.interrupt = self.interrupt.clone();
        let started = Instant::now();
        let result = bash
            .invoke(serde_json::json!({ "command": step.command }))
            .await;
        block(project, step, &result, started.elapsed().as_secs_f64())
    }
}

#[async_trait]
impl ToolHandler for ProjectTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "project".into(),
                description: "Run the project's test, lint, typecheck, build or format check \
                              with the command its files imply: Cargo, npm/pnpm/yarn/bun \
                              (scripts that exist), Go, Python, Makefile. `list` shows what was \
                              detected and each action's command, running nothing; `check` \
                              runs typecheck, lint, then test, each reported on its own. A \
                              failure leads with the errors."
                    .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": [
                                "list", "check", "test", "lint", "typecheck", "build", "format"
                            ]
                        },
                        "path": {
                            "type": "string",
                            "description": "File or directory; the nearest package above it \
                                            runs. Default: the workspace root."
                        },
                        "filter": {
                            "type": "string",
                            "description": "Narrows `test` (and `check`'s test step) to \
                                            matching test names (`cargo test <filter>`, \
                                            `pytest -k`, `go test -run`, the script's pattern \
                                            argument)."
                        }
                    },
                    "required": ["action"]
                }),
            },
            approval: ApprovalTier::Exec,
        }
    }

    fn describe(&self, args: &Value) -> Option<String> {
        let (request, project) = self.request(args).ok()?;
        let stages: &[Stage] = match request.action {
            Action::List => return Some("project list".to_owned()),
            Action::Check => &Stage::CHECK,
            Action::Run(stage) => &[stage],
        };
        let commands: Vec<String> = stages
            .iter()
            .flat_map(|stage| {
                project
                    .plan(*stage, request.filter.as_deref(), &on_path)
                    .steps
            })
            .map(|step| step.command)
            .collect();
        Some(describe_line(
            &format!("project {}", commands.join(" · ")),
            DESCRIBE_MAX,
        ))
    }

    fn refusal(&self, args: &Value) -> Option<String> {
        self.request(args).err().map(|error| error.to_string())
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let (request, project) = match self.request(&args) {
            Ok(found) => found,
            Err(error) => return err(error.to_string()),
        };
        let filter = request.filter.as_deref();
        let stages: &[Stage] = match request.action {
            Action::List => return ok(list(&project, filter, &on_path)),
            Action::Check => &Stage::CHECK,
            Action::Run(stage) => &[stage],
        };

        let mut blocks: Vec<String> = Vec::new();
        let (mut passed, mut failed) = (0usize, 0usize);
        let mut interrupted = false;
        let mut skips: Vec<String> = Vec::new();
        for stage in stages {
            let plan = project.plan(*stage, filter, &on_path);
            if plan.steps.is_empty() {
                skips.push(format!("– {} · {}", stage.name(), plan.skips.join("; ")));
                continue;
            }
            for step in &plan.steps {
                // A cancel stops the sequence, not just the step it hit.
                if self.interrupt.is_raised() {
                    interrupted = true;
                    break;
                }
                let ran = self.run(&project, step).await;
                if ran.passed {
                    passed += 1;
                } else {
                    failed += 1;
                }
                blocks.push(ran.block);
            }
        }

        if blocks.is_empty() && !interrupted {
            return err(format!(
                "nothing to run in {} ({}):\n{}",
                project.rel,
                project.names(),
                skips.join("\n")
            ));
        }
        blocks.extend(skips);
        if interrupted {
            blocks.push("– interrupted · the remaining steps did not run".to_owned());
        }
        if passed + failed > 1 {
            blocks.push(format!("{passed} passed · {failed} failed"));
        }
        ToolResult {
            output: blocks.join("\n\n").into(),
            is_error: failed > 0 || interrupted,
            detail: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn put(root: &Path, name: &str, body: &str) {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn nothing_installed(_: &str) -> bool {
        false
    }

    fn everything_installed(_: &str) -> bool {
        true
    }

    fn command(project: &Project, stage: Stage, filter: Option<&str>) -> Option<String> {
        project
            .plan(stage, filter, &nothing_installed)
            .steps
            .into_iter()
            .next()
            .map(|step| step.command)
    }

    fn labels(project: &Project) -> Vec<&'static str> {
        project.found.iter().map(Detected::label).collect()
    }

    #[test]
    fn cargo_resolves_every_stage() {
        let root = dir();
        put(root.path(), "Cargo.toml", "[package]\nname = \"foo\"\n");
        let project = locate(root.path(), None).unwrap();
        assert_eq!(labels(&project), ["cargo"]);
        assert_eq!(project.rel, ".");
        let expected = [
            (Stage::Test, "cargo test"),
            (Stage::Lint, "cargo clippy --all-targets"),
            (Stage::Typecheck, "cargo check --all-targets"),
            (Stage::Build, "cargo build"),
            (Stage::Format, "cargo fmt --check"),
        ];
        for (stage, want) in expected {
            assert_eq!(command(&project, stage, None).as_deref(), Some(want));
        }
    }

    #[test]
    fn a_cargo_workspace_root_runs_every_member() {
        let root = dir();
        put(
            root.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"a\"]\n",
        );
        let project = locate(root.path(), None).unwrap();
        assert_eq!(
            command(&project, Stage::Test, None).as_deref(),
            Some("cargo test --workspace")
        );
        assert_eq!(
            command(&project, Stage::Format, None).as_deref(),
            Some("cargo fmt --all --check")
        );
    }

    #[test]
    fn node_uses_the_lockfile_and_only_scripts_that_exist() {
        let root = dir();
        put(
            root.path(),
            "package.json",
            r#"{"scripts":{"test":"vitest run","lint":"eslint .","type-check":"tsc --noEmit"}}"#,
        );
        for (lockfile, manager) in [
            ("pnpm-lock.yaml", "pnpm"),
            ("yarn.lock", "yarn"),
            ("bun.lockb", "bun"),
            ("package-lock.json", "npm"),
        ] {
            put(root.path(), lockfile, "");
            let project = locate(root.path(), None).unwrap();
            assert_eq!(labels(&project), [manager], "{lockfile}");
            assert_eq!(
                command(&project, Stage::Lint, None),
                Some(format!("{manager} run lint"))
            );
            fs::remove_file(root.path().join(lockfile)).unwrap();
        }

        let project = locate(root.path(), None).unwrap();
        assert_eq!(labels(&project), ["npm"], "no lockfile means npm");
        assert_eq!(
            command(&project, Stage::Typecheck, None).as_deref(),
            Some("npm run type-check")
        );
        assert_eq!(command(&project, Stage::Build, None), None);
        let plan = project.plan(Stage::Build, None, &nothing_installed);
        assert_eq!(plan.skips, ["npm: package.json has no `build` script"]);
    }

    #[test]
    fn npms_placeholder_test_script_is_not_a_test_script() {
        let root = dir();
        put(
            root.path(),
            "package.json",
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        );
        let project = locate(root.path(), None).unwrap();
        assert_eq!(command(&project, Stage::Test, None), None);
    }

    #[test]
    fn a_package_json_that_does_not_parse_has_no_scripts() {
        let root = dir();
        put(root.path(), "package.json", "{ not json");
        let project = locate(root.path(), None).unwrap();
        assert_eq!(labels(&project), ["npm"]);
        assert_eq!(command(&project, Stage::Test, None), None);
    }

    #[test]
    fn go_resolves_every_stage() {
        let root = dir();
        put(root.path(), "go.mod", "module example.invalid/m\n");
        let project = locate(root.path(), None).unwrap();
        assert_eq!(labels(&project), ["go"]);
        assert_eq!(
            command(&project, Stage::Test, None).as_deref(),
            Some("go test ./...")
        );
        assert_eq!(
            command(&project, Stage::Lint, None).as_deref(),
            Some("go vet ./...")
        );
        assert_eq!(
            command(&project, Stage::Build, None).as_deref(),
            Some("go build ./...")
        );
        assert!(
            command(&project, Stage::Format, None)
                .unwrap()
                .contains("gofmt -l")
        );
    }

    #[test]
    fn python_tools_need_configuration_or_an_install() {
        let root = dir();
        put(root.path(), "pyproject.toml", "[project]\nname = \"p\"\n");
        let project = locate(root.path(), None).unwrap();
        assert_eq!(labels(&project), ["python"]);
        for stage in Stage::ALL {
            assert_eq!(command(&project, stage, None), None, "{stage:?}");
        }
        assert_eq!(
            project
                .plan(Stage::Test, None, &nothing_installed)
                .skips
                .len(),
            1
        );

        let installed = |stage| {
            project
                .plan(stage, None, &everything_installed)
                .steps
                .into_iter()
                .next()
                .map(|step| step.command)
        };
        assert_eq!(installed(Stage::Test).as_deref(), Some("pytest"));
        assert_eq!(installed(Stage::Lint).as_deref(), Some("ruff check ."));
        assert_eq!(installed(Stage::Typecheck).as_deref(), Some("mypy ."));
        assert_eq!(
            installed(Stage::Format).as_deref(),
            Some("ruff format --check .")
        );
        assert_eq!(installed(Stage::Build), None, "python has no build");

        put(
            root.path(),
            "pyproject.toml",
            "[tool.pytest.ini_options]\n[tool.ruff]\n[tool.mypy]\n",
        );
        let project = locate(root.path(), None).unwrap();
        for (stage, want) in [
            (Stage::Test, "pytest"),
            (Stage::Lint, "ruff check ."),
            (Stage::Typecheck, "mypy ."),
        ] {
            assert_eq!(
                command(&project, stage, None).as_deref(),
                Some(want),
                "configured, not installed"
            );
        }
    }

    #[test]
    fn make_targets_fill_in_what_no_other_ecosystem_has() {
        let root = dir();
        put(
            root.path(),
            "Makefile",
            "CC := cc\n.PHONY: test lint\ntest lint:\n\t@echo hi\nbuild: deps\n\techo b\ncheck:\n\ttrue\n%.o: %.c\n\tcc\n",
        );
        let project = locate(root.path(), None).unwrap();
        assert_eq!(labels(&project), ["make"]);
        assert_eq!(
            command(&project, Stage::Test, None).as_deref(),
            Some("make test")
        );
        assert_eq!(
            command(&project, Stage::Lint, None).as_deref(),
            Some("make lint")
        );
        assert_eq!(
            command(&project, Stage::Build, None).as_deref(),
            Some("make build")
        );
        assert_eq!(
            command(&project, Stage::Typecheck, None).as_deref(),
            Some("make check")
        );
        assert_eq!(command(&project, Stage::Format, None), None);

        put(root.path(), "Cargo.toml", "[package]\nname = \"foo\"\n");
        let project = locate(root.path(), None).unwrap();
        assert_eq!(labels(&project), ["cargo", "make"]);
        let plan = project.plan(Stage::Test, None, &nothing_installed);
        assert_eq!(plan.steps.len(), 1, "make does not repeat cargo's suite");
        assert_eq!(plan.steps[0].command, "cargo test");
    }

    #[test]
    fn a_monorepo_package_owns_its_own_files() {
        let root = dir();
        put(
            root.path(),
            "package.json",
            r#"{"scripts":{"build":"tsc"}}"#,
        );
        put(root.path(), "pnpm-lock.yaml", "");
        put(
            root.path(),
            "packages/a/package.json",
            r#"{"scripts":{"test":"jest"}}"#,
        );
        put(root.path(), "packages/a/src/index.ts", "");
        put(root.path(), "packages/b/src/index.ts", "");

        let at_root = locate(root.path(), None).unwrap();
        assert_eq!(at_root.rel, ".");
        assert_eq!(
            command(&at_root, Stage::Build, None).as_deref(),
            Some("pnpm run build")
        );

        for path in ["packages/a", "packages/a/src", "packages/a/src/index.ts"] {
            let package = locate(root.path(), Some(path)).unwrap();
            assert_eq!(package.rel, "packages/a", "{path}");
            assert_eq!(
                command(&package, Stage::Test, None).as_deref(),
                Some("pnpm run test"),
                "{path}: the root's lockfile decides the manager"
            );
            assert_eq!(
                command(&package, Stage::Build, None),
                None,
                "{path}: the root's scripts are not the package's"
            );
        }

        // `b` owns nothing, so the root does.
        let orphan = locate(root.path(), Some("packages/b/src/index.ts")).unwrap();
        assert_eq!(orphan.rel, ".");
    }

    #[test]
    fn filters_map_to_each_runners_own_flag() {
        let root = dir();
        put(root.path(), "Cargo.toml", "[package]\n");
        put(root.path(), "go.mod", "module m\n");
        put(root.path(), "pytest.ini", "[pytest]\n");
        put(
            root.path(),
            "package.json",
            r#"{"scripts":{"test":"jest"}}"#,
        );
        let project = locate(root.path(), None).unwrap();
        let steps = project
            .plan(Stage::Test, Some("parser"), &nothing_installed)
            .steps;
        let commands: Vec<&str> = steps.iter().map(|step| step.command.as_str()).collect();
        assert_eq!(
            commands,
            [
                "cargo test parser",
                "npm run test -- parser",
                "go test -run parser ./...",
                "pytest -k parser"
            ]
        );

        for (lockfile, want) in [
            ("pnpm-lock.yaml", "pnpm run test parser"),
            ("yarn.lock", "yarn run test parser"),
            ("bun.lock", "bun run test parser"),
        ] {
            put(root.path(), lockfile, "");
            let project = locate(root.path(), None).unwrap();
            let steps = project
                .plan(Stage::Test, Some("parser"), &nothing_installed)
                .steps;
            assert!(
                steps.iter().any(|step| step.command == want),
                "{lockfile}: {steps:?}"
            );
            fs::remove_file(root.path().join(lockfile)).unwrap();
        }
    }

    #[test]
    fn a_filter_is_one_shell_word_and_only_narrows_tests() {
        let root = dir();
        put(root.path(), "Cargo.toml", "[package]\n");
        let project = locate(root.path(), None).unwrap();
        assert_eq!(
            command(&project, Stage::Test, Some("a b; rm -rf /")).as_deref(),
            Some("cargo test 'a b; rm -rf /'")
        );
        assert_eq!(
            command(&project, Stage::Test, Some("it's")).as_deref(),
            Some(r"cargo test 'it'\''s'")
        );
        assert_eq!(
            command(&project, Stage::Lint, Some("parser")).as_deref(),
            Some("cargo clippy --all-targets")
        );
    }

    #[test]
    fn a_makefile_cannot_take_a_filter() {
        let root = dir();
        put(root.path(), "Makefile", "test:\n\ttrue\n");
        let project = locate(root.path(), None).unwrap();
        let plan = project.plan(Stage::Test, Some("x"), &nothing_installed);
        assert!(plan.steps.is_empty());
        assert_eq!(plan.skips, ["make: make targets take no filter"]);
    }

    #[test]
    fn requests_are_validated() {
        let parse = |args: Value| Request::parse(&args);
        assert_eq!(
            parse(serde_json::json!({})).unwrap_err(),
            ProjectError::MissingAction
        );
        assert_eq!(
            parse(serde_json::json!({"action": "deploy"})).unwrap_err(),
            ProjectError::UnknownAction("deploy".to_owned())
        );
        assert!(matches!(
            parse(serde_json::json!({"action": "test", "filter": "--nocapture"})),
            Err(ProjectError::Filter(_))
        ));
        assert!(matches!(
            parse(serde_json::json!({"action": "test", "filter": "a\nb"})),
            Err(ProjectError::Filter(_))
        ));
        let request = parse(serde_json::json!({"action": "test", "filter": ""})).unwrap();
        assert_eq!(request.filter, None, "an empty filter narrows nothing");
    }

    #[test]
    fn no_project_names_what_was_looked_for() {
        let root = dir();
        let error = locate(root.path(), None).unwrap_err().to_string();
        for marker in [
            "Cargo.toml",
            "package.json",
            "go.mod",
            "pyproject.toml",
            "Makefile",
        ] {
            assert!(error.contains(marker), "{error}");
        }
        assert!(error.contains("workspace root"), "{error}");
    }

    #[tokio::test]
    async fn paths_outside_the_workspace_are_refused() {
        let outer = dir();
        let root = outer.path().join("ws");
        put(&root, "Cargo.toml", "[package]\n");
        put(outer.path(), "other/Makefile", "test:\n\ttouch ran\n");
        let tool = ProjectTool::new(&root);

        for path in [
            "../other",
            outer.path().join("other").to_str().unwrap(),
            "..",
        ] {
            let args = serde_json::json!({"action": "test", "path": path});
            let refusal = tool.refusal(&args).unwrap_or_default();
            assert!(
                refusal.contains("outside the workspace"),
                "{path}: {refusal}"
            );
            let result = tool.invoke(args).await;
            assert!(result.is_error, "{path}");
            assert!(result.output.contains("outside the workspace"), "{path}");
        }
        assert!(
            !outer.path().join("other/ran").exists(),
            "nothing ran outside the workspace"
        );
        let missing = tool
            .invoke(serde_json::json!({"action": "test", "path": "nope"}))
            .await;
        assert!(missing.is_error && missing.output.contains("does not exist"));
    }

    #[tokio::test]
    async fn an_unknown_ecosystem_is_an_error_listing_what_was_looked_for() {
        let root = dir();
        let tool = ProjectTool::new(root.path());
        let result = tool.invoke(serde_json::json!({"action": "list"})).await;
        assert!(result.is_error);
        assert!(result.output.contains("Cargo.toml"), "{}", result.output);
    }

    #[tokio::test]
    async fn list_prints_the_commands_and_runs_nothing() {
        let root = dir();
        put(
            root.path(),
            "Makefile",
            "test:\n\ttouch ran-test\nlint:\n\ttouch ran-lint\n",
        );
        put(
            root.path(),
            "package.json",
            r#"{"scripts":{"build":"touch ran-build"}}"#,
        );
        let tool = ProjectTool::new(root.path());
        let result = tool.invoke(serde_json::json!({"action": "list"})).await;
        assert!(!result.is_error, "{}", result.output);
        let out = result.output.as_str();
        assert!(out.contains("npm (package.json), make (Makefile)"), "{out}");
        assert!(out.contains("make test"), "{out}");
        assert!(out.contains("make lint"), "{out}");
        assert!(out.contains("npm run build"), "{out}");
        assert!(
            out.contains("format     –"),
            "a stage with no command says why: {out}"
        );
        for ran in ["ran-test", "ran-lint", "ran-build"] {
            assert!(!root.path().join(ran).exists(), "{ran} ran");
        }
    }

    #[tokio::test]
    async fn a_passing_step_and_a_failing_one_each_get_a_block() {
        let root = dir();
        put(
            root.path(),
            "Makefile",
            "typecheck:\n\t@echo types fine\n\
             lint:\n\t@echo 'error: unused import `x`' >&2\n\t@echo '  --> src/a.rs:1:5' >&2\n\t@exit 2\n\
             test:\n\t@echo 'test result: ok. 12 passed'\n",
        );
        let tool = ProjectTool::new(root.path());
        let result = tool.invoke(serde_json::json!({"action": "check"})).await;
        let out = result.output.as_str();
        assert!(result.is_error, "a failed step fails the check: {out}");
        assert!(out.contains("✓ typecheck · make typecheck · "), "{out}");
        assert!(out.contains("✕ lint · make lint · "), "{out}");
        assert!(out.contains("· exit 2"), "{out}");
        assert!(out.contains("error: unused import `x`"), "{out}");
        assert!(out.contains("✓ test · make test · "), "{out}");
        assert!(out.contains("test result: ok. 12 passed"), "{out}");
        assert!(out.contains("2 passed · 1 failed"), "{out}");
        let order = |needle: &str| out.find(needle).unwrap();
        assert!(order("✓ typecheck") < order("✕ lint") && order("✕ lint") < order("✓ test"));
    }

    #[tokio::test]
    async fn a_single_stage_with_no_command_is_an_error_that_says_why() {
        let root = dir();
        put(
            root.path(),
            "package.json",
            r#"{"scripts":{"build":"true"}}"#,
        );
        let tool = ProjectTool::new(root.path());
        let result = tool.invoke(serde_json::json!({"action": "lint"})).await;
        assert!(result.is_error);
        assert!(
            result.output.contains("package.json has no `lint` script"),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn check_skips_a_stage_a_project_does_not_have() {
        let root = dir();
        put(root.path(), "Makefile", "test:\n\t@echo ok\n");
        let tool = ProjectTool::new(root.path());
        let result = tool.invoke(serde_json::json!({"action": "check"})).await;
        let out = result.output.as_str();
        assert!(!result.is_error, "{out}");
        assert!(out.contains("✓ test · make test"), "{out}");
        assert!(
            out.contains("– lint · make: the Makefile has no `lint` target"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn a_nested_package_runs_in_its_own_directory() {
        let root = dir();
        put(root.path(), "Makefile", "test:\n\t@echo root\n");
        put(root.path(), "svc/Makefile", "test:\n\t@pwd\n");
        let tool = ProjectTool::new(root.path());
        let result = tool
            .invoke(serde_json::json!({"action": "test", "path": "svc"}))
            .await;
        let out = result.output.as_str();
        assert!(!result.is_error, "{out}");
        assert!(out.contains(" · svc · "), "{out}");
        assert!(out.lines().any(|line| line.ends_with("/svc")), "{out}");
    }

    #[tokio::test]
    async fn a_raised_interrupt_stops_the_sequence() {
        let root = dir();
        put(
            root.path(),
            "Makefile",
            "typecheck:\n\t@true\nlint:\n\t@true\n",
        );
        let mut tool = ProjectTool::new(root.path());
        let interrupt = Interrupt::new();
        tool.interrupt = interrupt.clone();
        interrupt.raise();
        let result = tool.invoke(serde_json::json!({"action": "check"})).await;
        assert!(result.is_error);
        assert!(result.output.contains("interrupted"), "{}", result.output);
        assert!(!result.output.contains('✓'), "{}", result.output);
    }

    #[test]
    fn describe_names_the_resolved_command() {
        let root = dir();
        put(root.path(), "Cargo.toml", "[package]\n");
        let tool = ProjectTool::new(root.path());
        assert_eq!(
            tool.describe(&serde_json::json!({"action": "test", "filter": "parser"}))
                .as_deref(),
            Some("project cargo test parser")
        );
        assert_eq!(
            tool.describe(&serde_json::json!({"action": "list"}))
                .as_deref(),
            Some("project list")
        );
        let check = tool
            .describe(&serde_json::json!({"action": "check"}))
            .unwrap();
        assert!(check.starts_with("project cargo check"), "{check}");
        assert!(
            check.chars().count() <= "project ".len() + DESCRIBE_MAX,
            "{check}"
        );
    }

    #[test]
    fn it_is_an_exec_tool_in_the_workspace_set() {
        let tools = crate::fs::workspace_tools(".");
        let project = tools
            .iter()
            .map(|tool| tool.definition())
            .find(|definition| definition.spec.name == "project")
            .expect("project is registered with the workspace tools");
        assert_eq!(project.approval, ApprovalTier::Exec);
    }

    fn failed(command: &str, body: &str) -> String {
        let project = Project {
            dir: PathBuf::from("."),
            rel: ".".to_owned(),
            found: Vec::new(),
        };
        let step = Step {
            stage: Stage::Test,
            ecosystem: "cargo",
            command: command.to_owned(),
        };
        let result = err(format!("exit 101\n{body}"));
        block(&project, &step, &result, 12.34).block
    }

    #[test]
    fn a_failure_leads_with_what_failed_before_the_tail() {
        let mut output = String::from("   Compiling foo v0.1.0\n");
        output.push_str("test parser::empty ... ok\n");
        output.push_str("test parser::nested ... FAILED\n");
        for i in 0..40 {
            output.push_str(&format!("test filler::case_{i} ... ok\n"));
        }
        output.push_str("failures:\n    parser::nested\n\n");
        output.push_str("test result: FAILED. 41 passed; 1 failed\n");
        let block = failed("cargo test", &output);

        assert!(
            block.starts_with("✕ test · cargo test · 12.3s · exit 101\nerrors:\n"),
            "{block}"
        );
        assert!(block.contains("test parser::nested ... FAILED"), "{block}");
        let named = block.find("test parser::nested ... FAILED").unwrap();
        let tail = block.find("output tail:").unwrap();
        assert!(
            named < tail,
            "the failing test is named before the tail: {block}"
        );
        assert!(
            block.ends_with("test result: FAILED. 41 passed; 1 failed"),
            "{block}"
        );
        assert!(!block.contains("Compiling"), "noise stays out: {block}");
        assert!(!block.contains("filler::case_3 "), "{block}");
    }

    #[test]
    fn a_compiler_error_keeps_its_location() {
        let mut output = String::new();
        for i in 0..30 {
            output.push_str(&format!("   Compiling dep{i} v1.0.0\n"));
        }
        output.push_str("warning: unused variable: `y`\n");
        output.push_str(" --> src/lib.rs:2:9\n");
        output.push_str("error[E0308]: mismatched types\n");
        output.push_str("  --> src/lib.rs:7:5\n");
        output.push_str("   |\n");
        for _ in 0..20 {
            output.push_str("   | …\n");
        }
        output.push_str("error: could not compile `foo` (lib) due to 1 previous error\n");
        let block = failed("cargo check", &output);
        assert!(
            block.contains("error[E0308]: mismatched types\n  --> src/lib.rs:7:5"),
            "{block}"
        );
        assert!(
            !block.contains("src/lib.rs:2:9"),
            "a warning's location is noise: {block}"
        );
    }

    #[test]
    fn a_short_failure_is_shown_whole() {
        let block = failed("go test ./...", "--- FAIL: TestA (0.00s)\nFAIL\n");
        assert_eq!(
            block,
            "✕ test · go test ./... · 12.3s · exit 101\n--- FAIL: TestA (0.00s)\nFAIL"
        );
    }

    #[test]
    fn a_failure_block_is_bounded() {
        let mut output = String::new();
        for i in 0..500 {
            output.push_str(&format!("FAIL case_{i} {}\n", "x".repeat(400)));
        }
        let block = failed("npm run test", &output);
        let lines: Vec<&str> = block.lines().collect();
        assert!(
            lines.len() <= 1 + 1 + KEY_LINES + 1 + 1 + FAIL_TAIL_LINES,
            "{}",
            lines.len()
        );
        assert!(
            block.chars().count() <= BLOCK_CHARS + 200,
            "{}",
            block.chars().count()
        );
        assert!(block.contains("… "), "{block}");
        assert!(
            lines
                .iter()
                .all(|line| line.chars().count() <= LINE_CHARS + 1 + 60),
            "every line is clipped"
        );
    }

    #[test]
    fn a_pass_shows_only_the_end_of_the_output() {
        let project = Project {
            dir: PathBuf::from("."),
            rel: "svc".to_owned(),
            found: Vec::new(),
        };
        let step = Step {
            stage: Stage::Test,
            ecosystem: "cargo",
            command: "cargo test -p foo".to_owned(),
        };
        let body: String = (0..30).map(|i| format!("line {i}\n")).collect();
        let result = ok(format!("{body}test result: ok. 12 passed\n"));
        let ran = block(&project, &step, &result, 3.0);
        assert!(ran.passed);
        assert!(
            ran.block
                .starts_with("✓ test · cargo test -p foo · svc · 3.0s\n"),
            "{}",
            ran.block
        );
        assert!(
            ran.block.ends_with("test result: ok. 12 passed"),
            "{}",
            ran.block
        );
        assert!(!ran.block.contains("line 0"), "{}", ran.block);
    }

    #[test]
    fn a_killed_command_says_why_in_its_header() {
        let project = Project {
            dir: PathBuf::from("."),
            rel: ".".to_owned(),
            found: Vec::new(),
        };
        let step = Step {
            stage: Stage::Build,
            ecosystem: "cargo",
            command: "cargo build".to_owned(),
        };
        let result = err("command timed out after 300s and was killed\nstill linking");
        let ran = block(&project, &step, &result, 300.0);
        assert!(!ran.passed);
        assert!(
            ran.block.starts_with(
                "✕ build · cargo build · 300.0s · command timed out after 300s and was killed\n"
            ),
            "{}",
            ran.block
        );
        assert!(ran.block.ends_with("still linking"), "{}", ran.block);
    }
}
