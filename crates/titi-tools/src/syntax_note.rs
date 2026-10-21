//! The syntax line a file-changing tool appends to its answer.
//!
//! The model that wrote a stray brace learns of it from the tool that wrote it,
//! one line later in the same answer, instead of from a build several rounds
//! on. The check itself is `titi_atlas::syntax_check`; this only decides what
//! the tool's output looks like.

/// `output` with the syntax line under it when the edit from `before` to
/// `after` changed how many syntax errors `path` has, and `output` untouched
/// when it did not (or when `path` has no grammar, or a text is too large).
pub(crate) fn with_note(output: String, path: &str, before: &str, after: &str) -> String {
    match titi_atlas::syntax_check::delta(path, before, after) {
        Some(delta) => format!("{output}\n{delta}"),
        None => output,
    }
}
