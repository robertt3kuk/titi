#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! Ruby extraction through [`Genome`]: singleton methods, namespaced
//! constants, `attr_*`, and `require` vs `require_relative`.
//!
//! Every assertion is on a real extracted name or path, so deleting the
//! per-language handling in `src/lang/ruby.rs` fails this file.

use std::fs;
use std::path::Path;

use titi_genome::{Genome, Level};

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

const APP: &str = r#"
require_relative 'helper'
require 'json'

=begin
def block_ghost
end
=end

# def ghost
module Acme::App
  def self.run
    Helper.call
  end

  attr_reader :status

  class Report
    attr_accessor(:label)

    def render
    end
  end
end
"#;

#[test]
fn ruby_exports_singletons_namespaced_types_attrs_and_requires() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "lib/app.rb", APP);
    write(
        root,
        "lib/helper.rb",
        "module Helper\n  def call\n  end\nend\n",
    );
    write(root, "lib/broken.rb", "require_relative 'nope'\n");

    let genome = Genome::index(root).unwrap();
    let app = &genome.files["lib/app.rb"];

    for name in ["App", "run", "status", "label", "Report", "render"] {
        assert!(
            app.exports.contains(&name.to_owned()),
            "missing {name}: {:?}",
            app.exports
        );
    }
    assert!(
        !app.exports.contains(&"self".to_owned()),
        "`def self.run` publishes `run`, not the receiver: {:?}",
        app.exports
    );
    assert!(
        !app.exports.contains(&"ghost".to_owned()),
        "a commented-out def is not an export: {:?}",
        app.exports
    );
    // The pattern masked `#` and nothing else, so it read `block_ghost` out of
    // an `=begin`/`=end` block; the grammar knows a comment from code.
    assert!(
        !app.exports.contains(&"block_ghost".to_owned()),
        "a `=begin` block comment is not a declaration: {:?}",
        app.exports
    );

    assert!(
        app.imports.contains(&"lib/helper.rb".to_owned()),
        "require_relative 'helper' must resolve beside the file: {:?}",
        app.imports
    );

    // `require 'json'` searches the load path — a gem is not a missing file.
    assert!(
        app.imports.iter().all(|path| !path.contains("json")),
        "a plain `require` is not an edge: {:?}",
        app.imports
    );
    assert!(
        app.unresolved_imports.is_empty(),
        "`require 'json'` must not be unresolved: {:?}",
        app.unresolved_imports
    );

    // A `require_relative` naming no file is a real diagnostic.
    let broken = &genome.files["lib/broken.rb"];
    assert!(
        broken
            .unresolved_imports
            .iter()
            .any(|spec| spec.contains("nope")),
        "broken.rb: {:?}",
        broken.unresolved_imports
    );

    let diagnostics = genome.check();
    let unresolved: Vec<_> = diagnostics
        .iter()
        .filter(|item| item.code == "unresolved-import")
        .collect();
    assert!(
        unresolved.iter().any(|item| item.message.contains("nope")),
        "{unresolved:?}"
    );
    assert!(
        unresolved.iter().all(|item| !item.message.contains("json")),
        "{unresolved:?}"
    );

    let ruby = Genome::capabilities()
        .into_iter()
        .find(|capability| capability.language == "ruby")
        .expect("a ruby capability");
    assert_eq!(ruby.level, Level::Full, "{}", ruby.note);
}
