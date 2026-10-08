//! Terminal capability probing — synchronized output, DECCARA, kitty
//! graphics, cursor position report (CPR), background color query.
//!
//! Probes are sent as escape sequences; replies are read from the terminal
//! and matched against registered [`ProbeOwner`]s.  The design is
//! **fail-closed**: an unrecognised reply leaves the capability `false` and
//! never surfaces as user input (probe bytes are consumed by the probe layer,
//! not the component layer).
//!
//! Contract: `docs/research/tui-renderer/input-capabilities-graphics.md`.

use std::io::{self};
use std::time::Duration;

/// Cursor position in the terminal (1-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPos {
    pub row: u16,
    pub col: u16,
}

/// An RGB colour value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// A capability the terminal may advertise in reply to a probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cap {
    /// Synchronized output mode (`CSI ?2026h`).
    SyncOutput2026,
    /// DECCARA rectangle operations.
    Deccara,
    /// Kitty graphics protocol.
    KittyGraphics,
    /// Cursor position report.
    Cpr(CursorPos),
    /// Background colour (OSC 11 response).
    Bg(Rgb),
}

/// Owner of a probe — knows the sentinel it emitted and how to parse the
/// terminal's reply into a [`Cap`].
pub trait ProbeOwner {
    /// The probe string written to the terminal.
    fn sentinel(&self) -> &'static str;
    /// Parse a reply byte stream into a capability, or `None` if this reply
    /// is not for this owner.
    fn parse(&self, reply: &[u8]) -> Option<Cap>;
}

/// Default probe owners for the built-in probes.
pub fn default_owners() -> Vec<Box<dyn ProbeOwner>> {
    vec![
        Box::new(Da1Owner),
        Box::new(SyncOutputOwner),
        Box::new(CprOwner),
        Box::new(KittyGraphicsOwner),
        Box::new(BgColorOwner),
    ]
}

// ---------------------------------------------------------------------------
// Individual probe owners
// ---------------------------------------------------------------------------

/// DA1 (primary device attributes) — confirms the terminal speaks ANSI.
/// Reply: `CSI ? Pn ; ... c`.  The parameters encode terminal model; we only
/// care that a well-formed reply arrived.
struct Da1Owner;

impl ProbeOwner for Da1Owner {
    fn sentinel(&self) -> &'static str {
        "\x1b[c" // DA1
    }

    fn parse(&self, reply: &[u8]) -> Option<Cap> {
        let s = std::str::from_utf8(reply).ok()?;
        if !s.starts_with("\x1b[?") || !s.ends_with('c') {
            return None;
        }
        // No capability assigned from DA1 alone — just validates terminal.
        None
    }
}

/// Synchronized output — DECRQM for mode 2026.
/// Probe: `CSI ?2026$p`
/// Reply: `CSI ? 2026 ; Pm $ y` where `Pm` = 1 (set) or 2 (reset, recognised).
struct SyncOutputOwner;

impl ProbeOwner for SyncOutputOwner {
    fn sentinel(&self) -> &'static str {
        "\x1b[?2026$p"
    }

    fn parse(&self, reply: &[u8]) -> Option<Cap> {
        let s = std::str::from_utf8(reply).ok()?;
        // Expected format: \x1b[?2026;1$y
        if !s.starts_with("\x1b[?2026;") || !s.ends_with("$y") {
            return None;
        }
        let inner = &s[8..s.len() - 2];
        let state: u8 = inner.parse().ok()?;
        if state == 1 || state == 2 {
            // 1 = set, 2 = reset but recognised
            Some(Cap::SyncOutput2026)
        } else {
            None
        }
    }
}

/// CPR (cursor position report) — DSR.
/// Probe: `CSI 6n`
/// Reply: `CSI row ; col R`
struct CprOwner;

impl ProbeOwner for CprOwner {
    fn sentinel(&self) -> &'static str {
        "\x1b[6n"
    }

    fn parse(&self, reply: &[u8]) -> Option<Cap> {
        let s = std::str::from_utf8(reply).ok()?;
        if !s.starts_with("\x1b[") || !s.ends_with('R') || s.starts_with("\x1b[<") {
            return None;
        }
        let inner = &s[2..s.len() - 1];
        let mut parts = inner.split(';');
        let row: u16 = parts.next()?.parse().ok()?;
        let col: u16 = parts.next()?.parse().ok()?;
        Some(Cap::Cpr(CursorPos { row, col }))
    }
}

/// Kitty graphics query.
/// Probe: `APC Gq=1 ST`
/// Reply: `DCS Gi=1;OK ST` (contains `OK`).
struct KittyGraphicsOwner;

impl ProbeOwner for KittyGraphicsOwner {
    fn sentinel(&self) -> &'static str {
        "\x1b_Gq=1"
    }

    fn parse(&self, reply: &[u8]) -> Option<Cap> {
        let s = String::from_utf8_lossy(reply);
        if s.contains("OK") {
            Some(Cap::KittyGraphics)
        } else {
            None
        }
    }
}

/// Background color query — OSC 11.
/// Probe: `OSC 11 ; ? ST`
/// Reply: `OSC 11 ; rgb:N/N/N ST` where N = 0000-ffff.
struct BgColorOwner;

impl ProbeOwner for BgColorOwner {
    fn sentinel(&self) -> &'static str {
        "\x1b]11;?\x1b\\"
    }

    fn parse(&self, reply: &[u8]) -> Option<Cap> {
        let s = std::str::from_utf8(reply).ok()?;
        // Expected: \x1b]11;rgb:RRRR/GGGG/BBBB\x1b\ (or BEL-terminated)
        let body = s.strip_prefix("\x1b]11;")?;
        let spec = body
            .strip_suffix("\x1b\\")
            .or_else(|| body.strip_suffix('\x07'))?;
        let spec = spec
            .strip_prefix("rgba:")
            .or_else(|| spec.strip_prefix("rgb:"))
            .unwrap_or(spec);
        let parts: Vec<&str> = spec.split('/').collect();
        if parts.len() != 3 {
            return None;
        }
        let r = parse_xterm_color(parts[0])?;
        let g = parse_xterm_color(parts[1])?;
        let b = parse_xterm_color(parts[2])?;
        Some(Cap::Bg(Rgb { r, g, b }))
    }
}

/// Parse an xterm-style colour component (`0000`-`ffff`) to u8.
fn parse_xterm_color(hex: &str) -> Option<u8> {
    if hex.is_empty() || hex.len() > 4 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    let max = 16u32.saturating_pow(hex.len() as u32).saturating_sub(1);
    if max == 0 {
        return Some(0);
    }
    Some(((u64::from(value) * 255 + u64::from(max) / 2) / u64::from(max)) as u8)
}

/// Parse an OSC 11 background-color reply into RGB.
pub fn parse_osc11(reply: &[u8]) -> Option<Rgb> {
    match BgColorOwner.parse(reply) {
        Some(Cap::Bg(rgb)) => Some(rgb),
        _ => None,
    }
}

/// OSC 11 background-color query (ST-terminated).
pub const OSC11_QUERY: &str = "\x1b]11;?\x1b\\";
/// DEC Mode 2031 — terminal pushes DSR `CSI ? 997 ; 1/2 n` on appearance change.
pub const MODE_2031_ENABLE: &str = "\x1b[?2031h";
/// Disable Mode 2031 notifications.
pub const MODE_2031_DISABLE: &str = "\x1b[?2031l";

// ---------------------------------------------------------------------------
// I/O boundary
// ---------------------------------------------------------------------------

/// I/O boundary for probing — abstracts the real PTY for testability.
pub trait ProbeIo {
    /// Write raw bytes to the terminal.
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Read a reply chunk, blocking up to `timeout`.
    fn read_reply(&mut self, timeout: Duration) -> io::Result<Vec<u8>>;
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// Terminal capabilities discovered by probing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    sync_output: bool,
    deccara: bool,
    kitty: bool,
    bg: Option<Rgb>,
}

impl Capabilities {
    /// Discover capabilities by sending each probe and matching the reply.
    ///
    /// Fail-closed: an unrecognised or missing reply leaves the capability
    /// `false`.  Timeout bounds the whole probe pass.
    pub fn probe<I: ProbeIo>(
        io: &mut I,
        owners: &mut [Box<dyn ProbeOwner>],
        timeout: Duration,
    ) -> Self {
        let mut caps = Capabilities::default();
        for owner in owners.iter() {
            let sentinel = owner.sentinel();
            if io.write_all(sentinel.as_bytes()).is_err() {
                continue;
            }
            match io.read_reply(timeout) {
                Ok(reply) => {
                    if let Some(cap) = owner.parse(&reply) {
                        match cap {
                            Cap::SyncOutput2026 => caps.sync_output = true,
                            Cap::Deccara => caps.deccara = true,
                            Cap::KittyGraphics => caps.kitty = true,
                            Cap::Cpr(_) => {}
                            Cap::Bg(rgb) => caps.bg = Some(rgb),
                        }
                    }
                }
                Err(_) => continue,
            }
        }
        caps
    }

    pub fn sync_output(&self) -> bool {
        self.sync_output
    }

    pub fn deccara(&self) -> bool {
        self.deccara
    }

    pub fn kitty(&self) -> bool {
        self.kitty
    }

    /// OSC 11 background colour, if the probe returned a parseable reply.
    pub fn bg(&self) -> Option<Rgb> {
        self.bg
    }
}

// ---------------------------------------------------------------------------
// Synchronized output helpers
// ---------------------------------------------------------------------------

/// Begin synchronized output.
pub fn sync_begin() -> &'static str {
    "\x1b[?2026h"
}

/// End synchronized output.
pub fn sync_end() -> &'static str {
    "\x1b[?2026l"
}

/// Wrap a frame in synchronized-output markers (`CSI ?2026h` … `CSI ?2026l`).
pub fn wrap_sync(frame: &str) -> String {
    format!("\x1b[?2026h{frame}\x1b[?2026l")
}

/// OSC 8 hyperlink open sequence (`ESC ] 8 ; ; <uri> ST`), with `ST` = `ESC \`.
///
/// A terminal that does not know OSC 8 consumes the sequence and prints the
/// text that follows unchanged, so the visible text is always the whole fallback.
pub fn osc8_open(uri: &str) -> String {
    format!("\x1b]8;;{uri}\x1b\\")
}

/// OSC 8 hyperlink close sequence (`ESC ] 8 ; ; ST`).
pub const OSC8_CLOSE: &str = "\x1b]8;;\x1b\\";

/// `text` as one clickable link to `uri`; the visible text is untouched.
pub fn osc8_link(uri: &str, text: &str) -> String {
    format!("{}{text}{OSC8_CLOSE}", osc8_open(uri))
}

/// OSC 52 clipboard copy (`ESC ] 52 ; c ; <base64> BEL`).
pub fn osc52_copy(text: &str) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{b64}\x07")
}

// ---------------------------------------------------------------------------
// Desktop notifications and native progress
// ---------------------------------------------------------------------------

/// The plain bell: the notification channel every terminal that can ring has.
pub const BEL: &str = "\x07";

/// The terminal's own name and what sits in front of it.
///
/// The probes above ask a terminal a question and read the reply; OSC 777
/// and OSC 9;4 have no reply to read, so these two decisions are made from
/// the names the terminal gives itself — the same source omp's
/// `terminal-capabilities.ts` reads. The table is deliberately short: a
/// terminal that is not named here gets the BEL, which every one of them
/// understands, rather than a sequence it may print at the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TermEnv {
    /// `TERM_PROGRAM` — set by most modern terminals (`iTerm.app`, `WezTerm`).
    pub term_program: Option<String>,
    /// `TERM`.
    pub term: Option<String>,
    /// `TMUX`, `STY` or `ZELLIJ` is set: a sequence would have to cross it.
    pub multiplexed: bool,
    /// `WT_SESSION` is set — Windows Terminal.
    pub wt_session: bool,
    /// `KONSOLE_VERSION` is set — Konsole.
    pub konsole: bool,
    /// `VTE_VERSION` — the VTE library behind GNOME Terminal and Tilix.
    /// VTE 0.52 (5200) is the release that gained OSC 777.
    pub vte_version: Option<u32>,
}

impl TermEnv {
    /// Snapshot the process environment.
    pub fn from_env() -> Self {
        let flag = |key: &str| std::env::var_os(key).is_some();
        TermEnv {
            term_program: std::env::var("TERM_PROGRAM").ok(),
            term: std::env::var("TERM").ok(),
            multiplexed: ["TMUX", "STY", "ZELLIJ"].iter().any(|key| flag(key)),
            wt_session: flag("WT_SESSION"),
            konsole: flag("KONSOLE_VERSION"),
            vte_version: std::env::var("VTE_VERSION")
                .ok()
                .and_then(|v| v.parse::<u32>().ok()),
        }
    }

    /// The channel a finished turn reaches the user through.
    ///
    /// Notifications cannot be probed — nothing answers — so this is
    /// [`TermEnv::osc777`]'s name table plus two honest fallbacks. A terminal
    /// that is not
    /// named as an OSC 777 speaker gets the BEL; a run with no terminal at
    /// all (`TERM` unset, empty or `dumb`) gets nothing, because writing an
    /// escape into a pipe is garbage rather than a notification. A
    /// multiplexer forces the BEL too: OSC 777 reaches the outer terminal
    /// only through a passthrough the multiplexer may refuse to forward, and
    /// a swallowed sequence is a notification that never arrives.
    pub fn notification_channel(&self) -> NotifyChannel {
        if !self.is_terminal() {
            return NotifyChannel::None;
        }
        if !self.multiplexed && self.osc777() {
            return NotifyChannel::Osc777;
        }
        NotifyChannel::Bell
    }

    /// Whether the terminal names itself as an OSC 777 speaker.
    fn osc777(&self) -> bool {
        if self.konsole {
            return true;
        }
        let program = self.lower("term_program");
        let term = self.lower("term");
        ["iterm", "wezterm", "ghostty", "konsole"]
            .iter()
            .any(|name| program.contains(name) || term.contains(name))
            || term.contains("rxvt")
            || self.vte_version.is_some_and(|version| version >= 5200)
    }

    /// Whether the terminal draws OSC 9;4 native progress.
    ///
    /// The same kind of name table: Windows Terminal, WezTerm, Ghostty,
    /// Konsole and iTerm2 speak `9;4`; anything else is left quiet rather
    /// than sent a sequence it might print on screen.
    pub fn shows_progress(&self) -> bool {
        if !self.is_terminal() {
            return false;
        }
        let program = self.lower("term_program");
        let term = self.lower("term");
        self.wt_session
            || self.konsole
            || ["iterm", "wezterm", "ghostty"]
                .iter()
                .any(|name| program.contains(name) || term.contains(name))
    }

    /// Whether there is a terminal here at all.
    fn is_terminal(&self) -> bool {
        let term = self.lower("term");
        !term.is_empty() && term != "dumb" && term != "unknown"
    }

    /// One env value, lowercased, `""` when unset.
    fn lower(&self, field: &str) -> String {
        let raw = match field {
            "term" => self.term.as_deref(),
            _ => self.term_program.as_deref(),
        };
        raw.unwrap_or("").to_ascii_lowercase()
    }
}

/// The channel a finished turn's notification takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyChannel {
    /// OSC 777 — the terminal raises its own toast.
    Osc777,
    /// A plain BEL — the terminal rings or flashes.
    Bell,
    /// No terminal to write to: emit nothing.
    None,
}

/// OSC 777 desktop notification:
/// `ESC ] 777 ; notify ; <title> ; <body> BEL`.
///
/// `;` is the payload's field separator, so a semicolon inside the title or
/// the body would be read as the next field, and a control character would
/// end the sequence early (`BEL`) or open another one (`ESC`) — the caller
/// passes a session label and a fact, never file contents, and both are
/// folded here as well as at the call site.
pub fn osc777_notify(title: &str, body: &str) -> String {
    format!(
        "\x1b]777;notify;{};{}\x07",
        notify_field(title),
        notify_field(body)
    )
}

/// One field of the OSC 777 payload, with the characters that would split the
/// payload or escape it removed.
fn notify_field(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .map(|ch| if ch == ';' { ',' } else { ch })
        .collect()
}

// ---------------------------------------------------------------------------
// Mouse presets
// ---------------------------------------------------------------------------

/// Mouse tracking presets (mirror Hermes / omp).
///
/// - `Wheel`: button-event tracking + SGR (1000 + 1006)
/// - `Buttons`: + button motion (1002)
/// - `All`: + any-event motion (1003)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MousePreset {
    Off,
    Wheel,
    Buttons,
    All,
}

impl MousePreset {
    /// The CSI enable sequence for this preset.
    pub fn enable(&self) -> &'static str {
        match self {
            MousePreset::Off => "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l",
            MousePreset::Wheel => "\x1b[?1000h\x1b[?1006h",
            MousePreset::Buttons => "\x1b[?1000h\x1b[?1002h\x1b[?1006h",
            MousePreset::All => "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h",
        }
    }

    /// The CSI disable sequence for this preset.
    pub fn disable(&self) -> &'static str {
        match self {
            MousePreset::Off => "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l",
            MousePreset::Wheel => "\x1b[?1000l\x1b[?1006l",
            MousePreset::Buttons => "\x1b[?1000l\x1b[?1002l\x1b[?1006l",
            MousePreset::All => "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l",
        }
    }
    /// Parse a `/mouse` argument: `off`, `wheel`, `buttons`, `all`.
    pub fn parse(arg: &str) -> Option<Self> {
        match arg.trim().to_lowercase().as_str() {
            "off" => Some(MousePreset::Off),
            "on" => Some(MousePreset::All),
            "wheel" => Some(MousePreset::Wheel),
            "buttons" => Some(MousePreset::Buttons),
            "all" => Some(MousePreset::All),
            _ => None,
        }
    }

    /// The argument name used by `/mouse`.
    pub fn name(&self) -> &'static str {
        match self {
            MousePreset::Off => "off",
            MousePreset::Wheel => "wheel",
            MousePreset::Buttons => "buttons",
            MousePreset::All => "all",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory `ProbeIo`: returns scripted replies in order.
    struct ScriptedIo {
        replies: std::collections::VecDeque<Vec<u8>>,
        #[allow(dead_code)]
        written: Vec<u8>,
    }

    impl ScriptedIo {
        fn new(replies: Vec<Vec<u8>>) -> Self {
            ScriptedIo {
                replies: replies.into_iter().collect(),
                written: Vec::new(),
            }
        }
    }

    impl ProbeIo for ScriptedIo {
        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.written.extend_from_slice(bytes);
            Ok(())
        }

        fn read_reply(&mut self, _timeout: Duration) -> io::Result<Vec<u8>> {
            Ok(self.replies.pop_front().unwrap_or_default())
        }
    }

    // ---- Fail-closed ------------------------------------------------------

    #[test]
    fn unrecognized_reply_fails_closed() {
        let mut io = ScriptedIo::new(vec![
            b"garbage".to_vec(),
            b"\x1b[?7;X".to_vec(),
            b"".to_vec(),
        ]);
        let mut owners = default_owners();
        let caps = Capabilities::probe(&mut io, &mut owners, Duration::from_millis(5));
        assert!(!caps.sync_output());
        assert!(!caps.kitty());
    }

    #[test]
    fn no_reply_fails_closed() {
        let mut io = ScriptedIo::new(vec![]);
        let mut owners = default_owners();
        let caps = Capabilities::probe(&mut io, &mut owners, Duration::from_millis(5));
        assert!(!caps.sync_output());
        assert!(!caps.kitty());
    }

    // ---- Sync output detection -------------------------------------------

    #[test]
    fn sync_output_detected_via_decrqm() {
        let reply = b"\x1b[?2026;1$y";
        let mut io = ScriptedIo::new(vec![
            b"\x1b[?1;2c".to_vec(), // DA1
            reply.to_vec(),         // DECRQM for 2026
            b"".to_vec(),           // CPR — no reply
            b"".to_vec(),           // kitty — no reply
            b"".to_vec(),           // bg — no reply
        ]);
        let mut owners = default_owners();
        let caps = Capabilities::probe(&mut io, &mut owners, Duration::from_millis(5));
        assert!(caps.sync_output());
    }

    #[test]
    fn sync_output_mode_2_also_detects() {
        // Mode 2 = reset but recognised.
        let reply = b"\x1b[?2026;2$y";
        let mut io = ScriptedIo::new(vec![
            b"\x1b[?1;2c".to_vec(),
            reply.to_vec(),
            b"".to_vec(),
            b"".to_vec(),
            b"".to_vec(),
        ]);
        let mut owners = default_owners();
        let caps = Capabilities::probe(&mut io, &mut owners, Duration::from_millis(5));
        assert!(caps.sync_output());
    }

    #[test]
    fn sync_output_mode_3_not_detected() {
        // Mode 3 = permanently set; still recognised, but we only claim
        // 1 or 2 to keep the contract tight.
        let reply = b"\x1b[?2026;3$y";
        let mut io = ScriptedIo::new(vec![
            b"\x1b[?1;2c".to_vec(),
            reply.to_vec(),
            b"".to_vec(),
            b"".to_vec(),
            b"".to_vec(),
        ]);
        let mut owners = default_owners();
        let caps = Capabilities::probe(&mut io, &mut owners, Duration::from_millis(5));
        assert!(!caps.sync_output());
    }

    // ---- Kitty -----------------------------------------------------------

    #[test]
    fn kitty_reply_detected() {
        let mut io = ScriptedIo::new(vec![
            b"".to_vec(),                   // DA1 — no reply
            b"".to_vec(),                   // sync output — no reply
            b"".to_vec(),                   // CPR — no reply
            b"\x1b_Gi=1;OK\x1b\\".to_vec(), // kitty
            b"".to_vec(),                   // bg — no reply
        ]);
        let mut owners = default_owners();
        let caps = Capabilities::probe(&mut io, &mut owners, Duration::from_millis(5));
        assert!(caps.kitty());
        assert!(!caps.sync_output());
    }

    #[test]
    fn kitty_reply_missing_ok_fails_closed() {
        let owner = KittyGraphicsOwner;
        assert_eq!(owner.parse(b"\x1b_Gi=1;BAD"), None);
    }

    // ---- CPR --------------------------------------------------------------

    #[test]
    fn cpr_parse() {
        let owner = CprOwner;
        let cap = owner.parse(b"\x1b[42;8R");
        assert_eq!(cap, Some(Cap::Cpr(CursorPos { row: 42, col: 8 })));
    }

    #[test]
    fn cpr_parse_garbage() {
        let owner = CprOwner;
        assert_eq!(owner.parse(b"\x1b[42;xR"), None);
    }

    // ---- Background colour ------------------------------------------------

    #[test]
    fn bg_color_parse_black() {
        let owner = BgColorOwner;
        let reply = b"\x1b]11;rgb:0000/0000/0000\x1b\\";
        assert_eq!(owner.parse(reply), Some(Cap::Bg(Rgb { r: 0, g: 0, b: 0 })));
    }

    #[test]
    fn bg_color_parse_white() {
        let owner = BgColorOwner;
        let reply = b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\";
        assert_eq!(
            owner.parse(reply),
            Some(Cap::Bg(Rgb {
                r: 255,
                g: 255,
                b: 255,
            }))
        );
    }

    #[test]
    fn bg_color_bel_terminated() {
        let owner = BgColorOwner;
        let reply = b"\x1b]11;rgb:1234/5678/9abc\x07";
        assert_eq!(
            owner.parse(reply),
            Some(Cap::Bg(Rgb {
                r: 0x12,
                g: 0x56,
                b: 0x9a,
            }))
        );
    }

    // ---- Sync output wrap ------------------------------------------------

    #[test]
    fn sync_output_wrap_golden() {
        // Golden ANSI: frame wrapped; cursor writes inside the markers.
        let frame = "\x1b[H\x1b[2Jcontent";
        let wrapped = wrap_sync(frame);
        assert_eq!(wrapped, "\x1b[?2026h\x1b[H\x1b[2Jcontent\x1b[?2026l");
    }

    #[test]
    fn sync_output_markers_exact() {
        assert_eq!(sync_begin(), "\x1b[?2026h");
        assert_eq!(sync_end(), "\x1b[?2026l");
    }

    #[test]
    fn osc52_copy_is_base64_bel() {
        let seq = osc52_copy("hi");
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\u{07}'));
        assert!(seq.contains("aGk="), "base64(hi)=aGk=: {seq}");
    }

    #[test]
    fn osc8_link_wraps_the_text_with_the_uri_as_the_target() {
        let url = "https://example.invalid/a?b=c%20d";
        assert_eq!(
            osc8_open(url),
            "\x1b]8;;https://example.invalid/a?b=c%20d\x1b\\"
        );
        assert_eq!(OSC8_CLOSE, "\x1b]8;;\x1b\\");
        assert_eq!(
            osc8_link(url, "click"),
            "\x1b]8;;https://example.invalid/a?b=c%20d\x1b\\click\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn mouse_preset_off() {
        let preset = MousePreset::Off;
        assert_eq!(
            preset.enable(),
            "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l"
        );
        assert_eq!(
            preset.disable(),
            "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l"
        );
    }

    #[test]
    fn mouse_preset_wheel() {
        let preset = MousePreset::Wheel;
        assert_eq!(preset.enable(), "\x1b[?1000h\x1b[?1006h");
        assert_eq!(preset.disable(), "\x1b[?1000l\x1b[?1006l");
    }

    #[test]
    fn mouse_preset_buttons() {
        let preset = MousePreset::Buttons;
        assert_eq!(preset.enable(), "\x1b[?1000h\x1b[?1002h\x1b[?1006h");
        assert_eq!(preset.disable(), "\x1b[?1000l\x1b[?1002l\x1b[?1006l");
    }

    #[test]
    fn mouse_preset_parse_and_name() {
        assert_eq!(MousePreset::parse("off"), Some(MousePreset::Off));
        assert_eq!(MousePreset::parse("wheel"), Some(MousePreset::Wheel));
        assert_eq!(MousePreset::parse("buttons"), Some(MousePreset::Buttons));
        assert_eq!(MousePreset::parse("all"), Some(MousePreset::All));
        assert_eq!(MousePreset::parse("ALL"), Some(MousePreset::All));
        assert_eq!(MousePreset::parse("nope"), None);
        assert_eq!(MousePreset::Off.name(), "off");
        assert_eq!(MousePreset::Wheel.name(), "wheel");
        assert_eq!(MousePreset::Buttons.name(), "buttons");
        assert_eq!(MousePreset::All.name(), "all");
    }

    #[test]
    fn mouse_preset_all() {
        let preset = MousePreset::All;
        assert_eq!(
            preset.enable(),
            "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h"
        );
        assert_eq!(
            preset.disable(),
            "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l"
        );
    }

    // ---- Multi-cap probe --------------------------------------------------

    #[test]
    fn scripted_probe_sets_multiple_caps() {
        let mut io = ScriptedIo::new(vec![
            b"\x1b[?1;2c".to_vec(),                       // DA1
            b"\x1b[?2026;1$y".to_vec(),                   // sync output
            b"\x1b[5;10R".to_vec(),                       // CPR
            b"\x1b_Gi=1;OK\x1b\\".to_vec(),               // kitty
            b"\x1b]11;rgb:1234/5678/9abc\x1b\\".to_vec(), // bg
        ]);
        let mut owners = default_owners();
        let caps = Capabilities::probe(&mut io, &mut owners, Duration::from_millis(5));
        assert!(caps.sync_output());
        assert!(caps.kitty());
        assert!(!caps.deccara());
        assert_eq!(
            caps.bg(),
            Some(Rgb {
                r: 0x12,
                g: 0x56,
                b: 0x9a
            })
        );
    }

    // ---- Notifications and native progress --------------------------------

    /// A terminal named by the table either as `TERM_PROGRAM` or as `TERM`.
    fn env(program: Option<&str>, term: &str) -> TermEnv {
        TermEnv {
            term_program: program.map(str::to_owned),
            term: Some(term.to_owned()),
            ..TermEnv::default()
        }
    }

    #[test]
    fn osc777_terminals_are_named_not_guessed() {
        for terminal in [
            env(Some("iTerm.app"), "xterm-256color"),
            env(Some("WezTerm"), "xterm-256color"),
            env(Some("ghostty"), "xterm-ghostty"),
            env(None, "xterm-kitty"),
        ] {
            // kitty is not named by the table for OSC 777 — it is not known to
            // speak it — so it lands on the BEL with every other unknown.
            let expected = if terminal.term.as_deref() == Some("xterm-kitty") {
                NotifyChannel::Bell
            } else {
                NotifyChannel::Osc777
            };
            assert_eq!(terminal.notification_channel(), expected, "{terminal:?}");
        }
        // Konsole and a VTE new enough for OSC 777 (0.52) are named too.
        let konsole = TermEnv {
            konsole: true,
            ..env(None, "xterm-256color")
        };
        assert_eq!(konsole.notification_channel(), NotifyChannel::Osc777);
        let vte_new = TermEnv {
            vte_version: Some(5200),
            ..env(None, "xterm-256color")
        };
        assert_eq!(vte_new.notification_channel(), NotifyChannel::Osc777);
        let vte_old = TermEnv {
            vte_version: Some(4402),
            ..env(None, "xterm-256color")
        };
        assert_eq!(vte_old.notification_channel(), NotifyChannel::Bell);
    }

    #[test]
    fn a_multiplexer_and_an_unknown_terminal_fall_back_to_the_bell() {
        let tmux = TermEnv {
            multiplexed: true,
            ..env(Some("WezTerm"), "tmux-256color")
        };
        assert_eq!(tmux.notification_channel(), NotifyChannel::Bell);
        assert_eq!(
            env(Some("Apple_Terminal"), "xterm-256color").notification_channel(),
            NotifyChannel::Bell
        );
    }

    #[test]
    fn no_terminal_emits_nothing() {
        for term in [None, Some(""), Some("dumb"), Some("unknown")] {
            let bare = TermEnv {
                term: term.map(str::to_owned),
                ..TermEnv::default()
            };
            assert_eq!(bare.notification_channel(), NotifyChannel::None, "{term:?}");
            assert!(!bare.shows_progress(), "{term:?}");
        }
    }

    #[test]
    fn osc777_payload_is_bel_terminated_and_folds_its_separators() {
        assert_eq!(
            osc777_notify("titi", "blue-otter · turn finished"),
            "\x1b]777;notify;titi;blue-otter · turn finished\x07"
        );
        // A semicolon would be read as the next field; a control character
        // would close the sequence. Both are folded out, so neither can travel.
        assert_eq!(
            osc777_notify("ti;ti", "a\x07b\nc"),
            "\x1b]777;notify;ti,ti;abc\x07"
        );
        assert_eq!(BEL, "\x07");
    }

    #[test]
    fn progress_is_only_claimed_where_it_is_known() {
        let wt = TermEnv {
            wt_session: true,
            ..env(None, "xterm-256color")
        };
        assert!(wt.shows_progress());
        assert!(env(Some("WezTerm"), "xterm-256color").shows_progress());
        assert!(env(Some("ghostty"), "xterm-ghostty").shows_progress());
        assert!(env(Some("iTerm.app"), "xterm-256color").shows_progress());
        // An unnamed terminal is left quiet rather than sent a sequence it
        // may print on screen.
        assert!(!env(Some("Apple_Terminal"), "xterm-256color").shows_progress());
        assert!(!env(None, "xterm-kitty").shows_progress());
    }
}
