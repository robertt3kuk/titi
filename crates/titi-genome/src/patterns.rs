//! Compiling the patterns written in this crate.
//!
//! Two parts of the index read source text with a regex: the language modules,
//! for the import forms a grammar's tree does not spell out (`from … import`,
//! `require(…)`), and the reference collector, for calls, qualified paths and
//! capitalized type names. Every pattern is a literal in this repository, so a
//! bad one is a bug in this source rather than a condition any caller could
//! handle — which is exactly why they are compiled through one function here
//! instead of each site carrying its own panicking path.

use regex::Regex;

/// Compile a regex from a pattern written in this crate.
///
/// The allow is on this one function, not on a module where it would cover
/// every future `expect` as well. The invariant — every literal compiles — is
/// held by the tests that parse through each module holding one:
/// `lang::tests::the_literal_patterns_compile` for the import forms and
/// `refs::tests::the_call_path_and_type_patterns_compile_and_match` for the
/// three reference passes.
#[allow(clippy::expect_used)] // a literal pattern cannot fail for any input
pub(crate) fn literal_regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a literal regex compiles")
}
