//! Signing in to a model provider from the terminal.
//!
//! The protocol lives in `titi_providers::oauth`; this module is the surface.
//! For `titi --login` it prints the authorize URL, opens the browser, reads
//! the pasted code and stores the result. For the chat it carries the same
//! flow on a channel so the render loop never waits on a browser, a socket
//! or a person.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use titi_providers::ReqwestFetch;
use titi_providers::oauth::{self, OAuthTokens, OAuthUi};

pub use titi_providers::oauth::provider::{OAuthProvider, builtin as providers, find};

/// What a login reports while it runs.
pub enum LoginEvent {
    /// Where the user has to go, and what the provider says about it.
    Url { url: String, instructions: String },
    /// A line of progress, already fit to print.
    Progress(String),
    /// The grant, once the provider accepted the code.
    Done(Box<OAuthTokens>),
    /// The flow is over and the user has nothing.
    Failed(String),
}

/// One running login, as a surface sees it.
pub struct LoginFlow {
    /// Progress, then exactly one terminal [`LoginEvent::Done`] or
    /// [`LoginEvent::Failed`].
    pub events: tokio::sync::mpsc::UnboundedReceiver<LoginEvent>,
    /// Where the pasted code goes. Dropping it cancels the wait.
    pub codes: tokio::sync::mpsc::UnboundedSender<String>,
}

/// Starts a login without blocking the caller's loop.
///
/// A surface that must keep painting cannot await the browser, so the flow
/// belongs to a task the driver owns. Tests inject their own driver: nothing
/// here is reachable without one.
pub trait LoginDriver: Send + Sync {
    /// Starts the browser authorization-code flow.
    fn begin(&self, provider: &'static OAuthProvider) -> Result<LoginFlow, String>;

    /// Starts the device-code grant. Only a descriptor that advertises
    /// `supports_device` has one, and the default refuses rather than
    /// pretending: a driver that cannot run it must say so.
    fn begin_device(&self, provider: &'static OAuthProvider) -> Result<LoginFlow, String> {
        Err(format!("{} has no device flow", provider.name))
    }
}

/// Drives `titi_providers::oauth::login` on the runtime the caller is on.
pub struct ChannelDriver {
    fetch: Arc<dyn titi_providers::HttpFetch>,
}

impl ChannelDriver {
    pub fn new() -> Result<Self, String> {
        let fetch = ReqwestFetch::new().map_err(|error| error.to_string())?;
        Ok(Self {
            fetch: Arc::new(fetch),
        })
    }

    /// Spawns one flow on the caller's runtime. The device grant has no
    /// callback and no pasted code, but the choreography around it is the
    /// same: events one way, pasted codes the other, one terminal event.
    fn spawn(&self, provider: &'static OAuthProvider, device: bool) -> Result<LoginFlow, String> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| "login needs a running runtime".to_owned())?;
        let (sender, events) = tokio::sync::mpsc::unbounded_channel();
        let (codes, code_rx) = tokio::sync::mpsc::unbounded_channel();
        let fetch = Arc::clone(&self.fetch);
        handle.spawn(async move {
            let ui = ChannelUi {
                events: sender.clone(),
                codes: Mutex::new(code_rx),
            };
            let outcome = if device {
                oauth::login_device(provider, fetch.as_ref(), &ui).await
            } else {
                oauth::login(provider, fetch.as_ref(), &ui).await
            };
            let _ = sender.send(match outcome {
                Ok(tokens) => LoginEvent::Done(Box::new(tokens)),
                Err(error) => LoginEvent::Failed(error.to_string()),
            });
        });
        Ok(LoginFlow { events, codes })
    }
}

impl LoginDriver for ChannelDriver {
    fn begin(&self, provider: &'static OAuthProvider) -> Result<LoginFlow, String> {
        self.spawn(provider, false)
    }

    fn begin_device(&self, provider: &'static OAuthProvider) -> Result<LoginFlow, String> {
        self.spawn(provider, true)
    }
}

/// The UI of a screen: every callback becomes an event on the channel.
struct ChannelUi {
    events: tokio::sync::mpsc::UnboundedSender<LoginEvent>,
    codes: Mutex<tokio::sync::mpsc::UnboundedReceiver<String>>,
}

impl OAuthUi for ChannelUi {
    fn on_auth(&self, url: &str, instructions: &str) {
        let _ = self.events.send(LoginEvent::Url {
            url: url.to_owned(),
            instructions: instructions.to_owned(),
        });
    }

    fn on_progress(&self, message: &str) {
        let _ = self.events.send(LoginEvent::Progress(message.to_owned()));
    }

    fn manual_code(&self) -> Option<String> {
        self.codes.lock().ok()?.try_recv().ok()
    }
}

/// The terminal UI: prints, opens the browser, reads one line from stdin.
pub struct TerminalUi {
    /// Lines the reader thread took from stdin and nobody consumed. Reading
    /// on a thread keeps the callback server observable while the user is
    /// still deciding whether to paste anything at all.
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl TerminalUi {
    pub fn new() -> Self {
        let lines: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let sink = Arc::clone(&lines);
        std::thread::spawn(move || {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            let mut stdin = stdin.lock();
            loop {
                let mut line = String::new();
                match stdin.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match sink.lock() {
                    Ok(mut queue) => queue.push_back(line.to_owned()),
                    Err(_) => break,
                }
            }
        });
        Self { lines }
    }
}

impl Default for TerminalUi {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthUi for TerminalUi {
    fn on_auth(&self, url: &str, instructions: &str) {
        eprintln!("{instructions}");
        eprintln!();
        eprintln!("Open this URL in your browser:\n{url}");
        eprintln!();
        eprintln!("Paste the authorization code (or the full redirect URL):");
        open_browser(url);
    }

    fn on_progress(&self, message: &str) {
        eprintln!("{message}");
    }

    fn manual_code(&self) -> Option<String> {
        self.lines.lock().ok()?.pop_front()
    }
}

/// Hands the URL to the desktop's opener. A browser that does not open costs
/// the user one copy-paste, never the login, so every failure is silent.
fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "linux") {
        "xdg-open"
    } else {
        return;
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// `titi --login <id>`: the whole flow, then the store.
pub fn run_login(provider_id: &str, agent_dir: &Path) -> Result<(), String> {
    let provider = find(provider_id).ok_or_else(|| unknown(provider_id))?;
    let runtime = runtime()?;
    let fetch = ReqwestFetch::new().map_err(|error| error.to_string())?;
    let tokens = runtime.block_on(async {
        let ui = TerminalUi::new();
        oauth::login(provider, &fetch, &ui)
            .await
            .map_err(|error| error.to_string())
    })?;
    finish(agent_dir, provider, &tokens)
}

/// `titi --login --device <id>`: a code on another device, no callback.
pub fn run_login_device(provider_id: &str, agent_dir: &Path) -> Result<(), String> {
    let provider = find(provider_id).ok_or_else(|| unknown(provider_id))?;
    if !provider.supports_device {
        return Err(format!("{} has no device flow", provider.name));
    }
    let runtime = runtime()?;
    let fetch = ReqwestFetch::new().map_err(|error| error.to_string())?;
    let tokens = runtime.block_on(async {
        let ui = TerminalUi::new();
        oauth::login_device(provider, &fetch, &ui)
            .await
            .map_err(|error| error.to_string())
    })?;
    finish(agent_dir, provider, &tokens)
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
}

fn unknown(provider_id: &str) -> String {
    format!("unknown oauth provider {provider_id}")
}

/// Stores the grant and says who was signed in.
fn finish(agent_dir: &Path, provider: &OAuthProvider, tokens: &OAuthTokens) -> Result<(), String> {
    crate::secrets::store_oauth(agent_dir, provider.store_as, tokens)?;
    eprintln!("{}", identity_line(provider, tokens));
    Ok(())
}

/// `logged in to Anthropic (Claude Pro/Max) as user@example.invalid (Pro)`.
/// An account the provider did not name is simply not named.
pub fn identity_line(provider: &OAuthProvider, tokens: &OAuthTokens) -> String {
    let mut line = format!("logged in to {}", provider.name);
    if let Some(who) = tokens.email.as_deref().or(tokens.account_id.as_deref()) {
        line.push_str(&format!(" as {who}"));
    }
    if let Some(org) = tokens.org_name.as_deref() {
        line.push_str(&format!(" ({org})"));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens() -> OAuthTokens {
        OAuthTokens {
            access: "sk-test-access".into(),
            refresh: None,
            expires_at: None,
            account_id: Some("acct-1".into()),
            email: Some("user@example.invalid".into()),
            org_id: None,
            org_name: Some("Pro".into()),
        }
    }

    #[test]
    fn the_builtin_table_offers_both_providers() {
        let ids: Vec<&str> = providers().iter().map(|provider| provider.id).collect();
        assert_eq!(ids, ["anthropic", "openai-codex"]);
        for id in &ids {
            assert!(find(id).is_some(), "{id} is not findable");
        }
        assert!(find("nope").is_none());
    }

    #[test]
    fn the_identity_line_names_who_signed_in() {
        let provider = find("anthropic").expect("anthropic");
        let line = identity_line(provider, &tokens());
        assert!(
            line.starts_with(&format!("logged in to {}", provider.name)),
            "{line}"
        );
        assert!(line.contains("as user@example.invalid (Pro)"), "{line}");

        let mut anonymous = tokens();
        anonymous.email = None;
        anonymous.account_id = None;
        anonymous.org_name = None;
        assert_eq!(
            identity_line(provider, &anonymous),
            format!("logged in to {}", provider.name)
        );
    }

    #[test]
    fn an_unknown_provider_is_named_in_the_error() {
        let Err(reason) = run_login("nope", Path::new(".")) else {
            panic!("an unknown provider must not run a flow");
        };
        assert_eq!(reason, "unknown oauth provider nope");
    }
}
