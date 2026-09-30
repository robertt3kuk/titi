//! The `--theme` flag: a palette for one run, and a refusal by name for a
//! theme this build does not carry.
//!
//! The flag is parsed before the screen opens, so a refusal is answered on
//! stderr with the exit code the other bad-argument paths use — no terminal,
//! no engine, nothing started.

/// A theme this build does not carry is refused by name, with the count of
/// what it does carry, rather than falling back to another palette.
#[test]
fn an_unknown_theme_is_refused_by_name() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_titi"))
        .args(["--theme", "nope"])
        .output()
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(2), "{:?}", output.status);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown theme nope"), "{stderr}");
    assert!(
        stderr.contains("this build carries"),
        "the refusal names what exists: {stderr}"
    );
    assert!(stderr.contains("/theme"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

/// A theme without its name is a usage error, not a silent auto pick.
#[test]
fn a_theme_flag_without_a_name_is_a_usage_error() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_titi"))
        .arg("--theme")
        .output()
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(2), "{:?}", output.status);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--theme <name>"), "{stderr}");
}

/// `--help` documents the flag, so the list of ways to choose a palette is
/// discoverable without the chat.
#[test]
fn help_documents_the_theme_flag() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_titi"))
        .arg("--help")
        .output()
        .expect("the binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--theme <name>"), "{stdout}");
}
