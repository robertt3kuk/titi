//! Shell commands that do not exit on their own, refused before they run.
//!
//! A dev server, a watcher or `tail -f` run in the foreground holds a `bash`
//! call until its deadline — five minutes by default — and then comes back
//! killed, having told the model nothing it could not have learned in two
//! seconds. Refusing it at once, with the background form to use instead,
//! saves the wait. After omp's bash interceptor (MIT), which redirects these
//! to its managed services; titi has none, so the advice is `&` and a log.
//!
//! The check is deliberately narrow: a command only counts when it runs in
//! the foreground, so `npm run dev > dev.log 2>&1 &` passes, and so does
//! `timeout 30 npm run dev`, the way to check that something starts. Quoted
//! text, comments and heredoc bodies are never read as commands. A miss
//! costs one deadline; a false refusal would cost a working command, so
//! anything unclear passes.

/// Why `command` is refused, written for the model, or `None` to run it.
pub fn refusal(command: &str) -> Option<String> {
    foreground_commands(command)
        .into_iter()
        .find_map(|segment| {
            let words = command_words(&segment);
            holds_on(&words).then(|| {
                let shown: String = segment.chars().take(120).collect();
                let log = format!("/tmp/titi-{}.log", words.first().copied().unwrap_or("job"));
                format!(
                    "refused: `{shown}` keeps running until it is stopped, so in the \
                 foreground it would hold this call until its deadline. Start it in the \
                 background with its output in a file — `{shown} > {log} 2>&1 &` — then \
                 read the log or query it. To check only that it starts, run \
                 `timeout 30 {shown}`."
                )
            })
        })
}

/// The simple commands of `command` that run in the foreground, each as it
/// was written. A trailing `&` puts the whole and/or list before it in the
/// background, as the shell does.
fn foreground_commands(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    // Each finished command, and whether it runs in the background.
    let mut done: Vec<(String, bool)> = Vec::new();
    // Where the list the next `&` would background begins.
    let mut list_start = 0;
    let mut current = String::new();
    let (mut single, mut double) = (false, false);
    let mut i = 0;
    while let Some(&ch) = chars.get(i) {
        i += 1;
        if single {
            current.push(ch);
            single = ch != '\'';
            continue;
        }
        if double {
            current.push(ch);
            if ch == '\\' {
                if let Some(&next) = chars.get(i) {
                    current.push(next);
                    i += 1;
                }
            } else if ch == '"' {
                double = false;
            }
            continue;
        }
        match ch {
            '\\' => {
                current.push(ch);
                if let Some(&next) = chars.get(i) {
                    current.push(next);
                    i += 1;
                }
            }
            '\'' => {
                single = true;
                current.push(ch);
            }
            '"' => {
                double = true;
                current.push(ch);
            }
            '#' if current.chars().last().is_none_or(char::is_whitespace) => {
                while chars.get(i).is_some_and(|&next| next != '\n') {
                    i += 1;
                }
            }
            // A heredoc's body is data. Telling where it ends takes a real
            // parser, so nothing after `<<` is read as a command.
            '<' if chars.get(i) == Some(&'<') => break,
            ';' | '\n' => {
                finish(&mut done, &mut current);
                list_start = done.len();
            }
            '&' if chars.get(i) == Some(&'&') => {
                finish(&mut done, &mut current);
                i += 1;
            }
            // `2>&1`, `>&2` and `&>file` are redirections, not `&`.
            '&' if matches!(current.chars().last(), Some('>' | '<'))
                || chars.get(i) == Some(&'>') =>
            {
                current.push(ch);
            }
            '&' => {
                finish(&mut done, &mut current);
                for (_, background) in &mut done[list_start..] {
                    *background = true;
                }
                list_start = done.len();
            }
            '|' => {
                finish(&mut done, &mut current);
                if matches!(chars.get(i), Some('|' | '&')) {
                    i += 1;
                }
            }
            other => current.push(other),
        }
    }
    finish(&mut done, &mut current);
    done.into_iter()
        .filter(|(_, background)| !background)
        .map(|(text, _)| text)
        .collect()
}

fn finish(done: &mut Vec<(String, bool)>, current: &mut String) {
    let text = current.trim();
    if !text.is_empty() {
        done.push((text.to_owned(), false));
    }
    current.clear();
}

/// The words of one simple command, quotes and grouping stripped, from the
/// program on: leading `NAME=value` assignments and the wrappers that run
/// their argument in the same foreground (`exec`, `nohup`, `env`, `time`,
/// `command`) are dropped. `timeout` is not: it is the escape hatch.
fn command_words(segment: &str) -> Vec<&str> {
    let mut words: Vec<&str> = segment
        .split_whitespace()
        .map(|word| {
            word.trim_start_matches(['(', '{'])
                .trim_end_matches([')', '}'])
                .trim_matches(['\'', '"'])
        })
        .filter(|word| !word.is_empty())
        .collect();
    let program = words
        .iter()
        .position(|word| {
            !is_assignment(word) && !matches!(*word, "exec" | "nohup" | "env" | "time" | "command")
        })
        .unwrap_or(words.len());
    words.drain(..program);
    // `npx -y vite`: the package runner, its flags, then the program.
    if matches!(words.first(), Some(&("npx" | "bunx" | "pnpx"))) {
        let program = words
            .iter()
            .skip(1)
            .position(|word| !word.starts_with('-'))
            .map_or(words.len(), |at| at + 1);
        words.drain(..program);
    }
    words
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    })
}

/// Whether the command serves, watches or follows until it is stopped.
fn holds_on(words: &[&str]) -> bool {
    let Some((&program, args)) = words.split_first() else {
        return false;
    };
    let first = args.first().copied();
    let has = |flags: &[&str]| args.iter().any(|arg| flags.contains(arg));
    let follows = || {
        args.iter().any(|arg| {
            matches!(*arg, "--follow" | "-F")
                || arg.starts_with("--follow=")
                || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('f'))
        })
    };
    if args
        .iter()
        .any(|arg| matches!(*arg, "--watch" | "--watchAll" | "--watch=true"))
    {
        return true;
    }
    match program {
        "npm" | "pnpm" | "yarn" | "bun" => {
            let script = if first == Some("run") {
                args.get(1).copied()
            } else {
                first
            };
            matches!(script, Some("dev" | "start" | "serve" | "watch"))
        }
        "vite" => match first {
            None | Some("dev" | "serve" | "preview") => true,
            Some(arg) => {
                arg.starts_with('-') && !matches!(arg, "-v" | "--version" | "-h" | "--help")
            }
        },
        "next" | "nuxt" | "nuxi" | "astro" | "remix" | "wrangler" => {
            matches!(first, Some("dev" | "start" | "preview"))
        }
        "ng" | "ember" | "webpack" | "vue-cli-service" => matches!(first, Some("serve")),
        "hugo" => matches!(first, Some("server" | "serve")),
        "jekyll" | "mkdocs" => matches!(first, Some("serve")),
        "gatsby" => matches!(first, Some("develop")),
        "expo" | "react-scripts" | "docusaurus" => matches!(first, Some("start")),
        "flask" => matches!(first, Some("run")),
        "rails" => matches!(first, Some("server" | "s")),
        "php" => first == Some("artisan") && args.get(1) == Some(&"serve"),
        "nodemon" | "webpack-dev-server" | "live-server" | "http-server" | "json-server"
        | "uvicorn" | "gunicorn" | "hypercorn" | "watchexec" | "cargo-watch" | "watch" => true,
        "python" | "python3" => {
            first == Some("-m") && matches!(args.get(1), Some(&("http.server" | "uvicorn")))
        }
        "cargo" => {
            first == Some("watch")
                || (first == Some("leptos") && matches!(args.get(1), Some(&("watch" | "serve"))))
        }
        "tsc" => has(&["-w"]),
        "tail" | "journalctl" => follows(),
        "docker" | "podman" | "kubectl" => {
            if first == Some("compose") && args.get(1) == Some(&"up") {
                !has(&["-d", "--detach"])
            } else {
                args.iter().take(2).any(|arg| *arg == "logs") && follows()
            }
        }
        "docker-compose" => first == Some("up") && !has(&["-d", "--detach"]),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_servers_watchers_and_followers_are_refused() {
        for command in [
            "npm run dev",
            "pnpm dev",
            "yarn start",
            "bun run dev -- --host",
            "PORT=3000 npm run dev",
            "cd web && npm run dev",
            "cd web; npm start",
            "(cd web && npm run dev)",
            "npm install\nnpm run dev",
            "npx vite",
            "npx -y vite --port 4000",
            "vite",
            "next dev",
            "nodemon app.js",
            "python3 -m http.server 8000",
            "flask run --port 5001",
            "tail -f build.log",
            "tail -n 50 -F build.log",
            "journalctl -fu nginx",
            "docker logs -f web",
            "docker compose up",
            "docker-compose up web",
            "cargo watch -x test",
            "jest --watch",
            "tsc -w",
            "npm test -- --watchAll",
            "nohup npm run dev",
            "env NODE_ENV=dev npm run dev",
            "npm run dev | tee dev.log",
            "npm run dev 2>&1 | tee dev.log",
            "echo starting; npm run dev",
        ] {
            let refusal = refusal(command);
            assert!(refusal.is_some(), "{command:?} was let through");
        }
    }

    #[test]
    fn commands_that_exit_or_run_in_the_background_pass() {
        for command in [
            "npm run dev &",
            "npm run dev > /tmp/dev.log 2>&1 &",
            "npm run dev &> dev.log &",
            "(npm run dev &)",
            "npm run dev & sleep 5; curl -s localhost:5173",
            "npm start && echo up &",
            "timeout 30 npm run dev",
            "npm run build",
            "npm test",
            "npm install",
            "vite build",
            "vite --version",
            "next build",
            "cargo test",
            "cargo build --release",
            "docker compose up -d",
            "docker compose logs web",
            "tail -n 50 build.log",
            "git log --follow src/main.rs",
            "python3 -m pytest",
            "echo 'npm run dev'",
            "echo \"run: npm run dev\"",
            "grep -rn 'npm run dev' docs",
            "# npm run dev",
            "ls # then npm run dev",
            "cat <<EOF > notes.md\nnpm run dev\nEOF",
            "echo done >&2",
            "ls -la | grep watch",
            "",
        ] {
            assert_eq!(refusal(command), None, "{command:?} was refused");
        }
    }

    #[test]
    fn the_refusal_names_the_command_and_the_background_form() {
        let refusal = refusal("cd web && npm run dev").unwrap_or_default();
        assert!(refusal.contains("`npm run dev`"), "{refusal}");
        assert!(
            refusal.contains("npm run dev > /tmp/titi-npm.log 2>&1 &"),
            "{refusal}"
        );
        assert!(refusal.contains("timeout 30 npm run dev"), "{refusal}");
    }
}
