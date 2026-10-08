//! PHP: comment masking, grouped/`function`/`const` `use` forms and
//! namespace-rooted import placement, through the [`Genome`] a user gets.

#![allow(clippy::unwrap_used, clippy::expect_used)]

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

fn app(root: &Path) {
    write(
        root,
        "app/App.php",
        r#"<?php
namespace Acme;

use Acme\Util;
use Acme\{Hash, Cache};
use function Acme\helper;
use Psr\Log\LoggerInterface;

final class App {
    public function run(): void {}
    private function secret(): void {}
}

function topLevel(): void {}

$doc = <<<DOC
class Quoted {}
DOC;

// class Ghost {}
"#,
    );
    for name in ["Util", "Hash", "Cache"] {
        write(
            root,
            &format!("app/Acme/{name}.php"),
            &format!("<?php\nnamespace Acme;\n\nclass {name} {{}}\n"),
        );
    }
}

/// A file whose `use` names a workspace vendor namespace with no file behind
/// it.
fn missing(root: &Path) {
    write(
        root,
        "app/Legacy.php",
        "<?php\nnamespace Acme;\n\nuse Acme\\Missing\\Thing;\n\nclass Legacy {}\n",
    );
}

#[test]
fn php_exports_skip_comments_and_private_methods() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    app(root);

    let genome = Genome::index(root).unwrap();
    let app = &genome.files["app/App.php"];
    for name in ["App", "run", "topLevel"] {
        assert!(
            app.exports.iter().any(|export| export == name),
            "missing export {name}: {:?}",
            app.exports
        );
    }
    assert!(
        !app.exports.iter().any(|export| export == "secret"),
        "a private method is not exported: {:?}",
        app.exports
    );
    assert!(
        !app.exports.iter().any(|export| export == "Ghost"),
        "a commented-out declaration is not an export: {:?}",
        app.exports
    );
    // A `class` written inside a heredoc is string content: the pattern read
    // it (only `//` and `/* */` were masked), the grammar does not.
    assert!(
        !app.exports.iter().any(|export| export == "Quoted"),
        "a heredoc's content is not a declaration: {:?}",
        app.exports
    );
    for (path, name) in [
        ("app/Acme/Util.php", "Util"),
        ("app/Acme/Hash.php", "Hash"),
        ("app/Acme/Cache.php", "Cache"),
    ] {
        assert!(
            genome.files[path].exports.contains(&name.to_owned()),
            "{path} should export {name}: {:?}",
            genome.files[path].exports
        );
    }
}

#[test]
fn php_use_specifiers_resolve_to_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    app(root);

    let genome = Genome::index(root).unwrap();
    let app = &genome.files["app/App.php"];
    for path in [
        "app/Acme/Util.php",
        "app/Acme/Hash.php",
        "app/Acme/Cache.php",
    ] {
        assert!(
            app.imports.contains(&path.to_owned()),
            "{path} not an edge: {:?}",
            app.imports
        );
        assert_eq!(genome.dependents[path], 1);
    }
}

#[test]
fn php_splits_vendor_imports_from_missing_workspace_ones() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    app(root);
    missing(root);

    let genome = Genome::index(root).unwrap();
    let app = &genome.files["app/App.php"];
    assert!(
        !app.unresolved_imports
            .iter()
            .any(|spec| spec.contains("Psr")),
        "a vendor import is not a missing file: {:?}",
        app.unresolved_imports
    );
    assert!(
        !app.imports.iter().any(|path| path.contains("Psr")),
        "a vendor import is not an edge: {:?}",
        app.imports
    );
    assert!(
        genome.files["app/Legacy.php"]
            .unresolved_imports
            .iter()
            .any(|spec| spec == "Acme\\Missing\\Thing"),
        "a workspace-shaped specifier with no file is reported: {:?}",
        genome.files["app/Legacy.php"].unresolved_imports
    );
    let warning = genome
        .check()
        .into_iter()
        .find(|item| {
            item.code == "unresolved-import" && item.message.contains("Acme\\Missing\\Thing")
        })
        .expect("unresolved-import diagnostic");
    assert!(
        warning.message.contains("Acme\\Missing\\Thing"),
        "message: {}",
        warning.message
    );
}

#[test]
fn php_is_a_parsed_language_in_the_capability_roster() {
    let php = Genome::capabilities()
        .into_iter()
        .find(|capability| capability.language == "php")
        .expect("a php capability");
    assert_eq!(php.level, Level::Full, "{}", php.note);
    assert!(php.note.contains("syntax tree"), "{}", php.note);
    assert_eq!(php.extensions, vec!["php"]);
}
