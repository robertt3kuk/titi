# `app.rs`: delete, port, or promote — the decision file

HEAD `9db2eb3`. `crates/titi-cli/src/main.rs:334` is the only product entry: `chat::run(...)`.
`crates/titi-cli/src/app.rs` (2492 lines) defines the `App` stack, reachable only from tests.
NB: `app.rs` is not fully dead — `chat.rs` calls its **free helper fns** 86 times; only the
`App` struct/dispatch is unreachable.

## 1. What `app.rs` is

| Piece | Where | Only in App? |
|---|---|---|
| `App` struct + render/key loop (`render`, `plan_frame`, `handle_canonical*`) | app.rs:60, 1259, 1274, 1364 | yes |
| Overlay framework: `OverlayOutcome`, `overlay_input`, compose via `titi_tui::overlay` | app.rs:664; overlay.rs | yes |
| Model / session / approval / recap / help / history-search / hub overlays | app.rs:372–662 | yes (chat.rs has its own pickers) |
| Free helpers used by chat.rs: `checkpoint_session`, `list_checkpoints`, `rewind_session`, `session_history`, `new_session`, `list_sessions_from`, `delete_session_from`, `model_choices`, `default_theme`, `load/save_mouse_preset_from` | app.rs (chat.rs refs at chat.rs:1340–1963, login_oauth.rs:65) | **no — imported 86× by chat.rs**, 1× engine.rs, 1× ompcast.rs |
| STT (`SttState`, hold-space, mic stub) | app.rs:51–53, 1679 | yes; chat.rs has no STT |
| Space-hold (hold-space dictation input) via `titi_tui::space_hold` | app.rs:29 | yes |
| Mouse selection / drag / mouse-preset config | app.rs:775–799; transcript.rs test 138 | yes; chat.rs has zero mouse code |
| Exit double-press window (`EXIT_HINT`, `EXIT_CONFIRM_WINDOW`) | engine_events.rs:170 | chat.rs has own `quit_armed` (chat.rs:466, 3171) |
| Keybindings manager routing (`handle_canonical`) | app.rs:1364; keybindings.rs | yes; chat.rs matches raw `Key`s (chat.rs:678–701) |
| Terminal appearance probe / OSC-11 auto-theme | app.rs:729–757 | chat.rs themes are explicit `/theme` only |

## 2. What the tests exercise

Audit said five files; grep finds **nine** test files constructing `App` (`App::new`): checkpoints,
dispatch, engine_events, overlays_paste, queue, recap, session_log, slash_completion, transcript —
**2543 lines** total (crates/titi-cli/tests). Which assertions would matter after a delete:

- **Product-behaviour (assert chat-reachable semantics, would need rehoming):** `session_log.rs`
  (session_history truncation/round integrity — helpers live in app.rs, used by chat.rs:1724–1963);
  `checkpoints.rs` (checkpoint/rewind fns used by chat.rs:1340–1718); parts of `overlays_paste.rs`
  (model_choices, list/delete_session_from — helpers).
- **Dead-stack-only (assert `App` keys/overlays, die with delete):** `dispatch.rs` (515 L),
  `engine_events.rs` (370 L: hub revive/stop, exit-arm, steer, pause), `queue.rs` (98 L),
  `slash_completion.rs` (170 L), `recap.rs` panel flow, `transcript.rs` (mouse/selection/accordion),
  most of `overlays_paste.rs` (picker keystrokes).

## 3. Overlay stack: who uses what

Used by **App only** (no `chat.rs` import): `titi_tui/src/overlay.rs`, `selection.rs`,
`renderer.rs`, `transcript.rs`, `composer.rs`, `history.rs`, `keybindings.rs`, `slash.rs`,
`space_hold.rs`, `hub.rs` (roster widget), `recap.rs`, `markdown.rs` render path.
Used by **chat.rs too**: `panels.rs` (chat.rs:4811–4822 box drawing), `scrollbar.rs` (4888),
`status_bar.rs` (29), `status.rs` (TurnFooter), `theme/`, `width.rs`, `caps.rs`, `diff.rs`,
`image.rs`. 209 `#[cfg(test)]`-gated inline tests in chat.rs cover its own logic.

## 4. Feature map (the crux)

| Feature | chat.rs equivalent? |
|---|---|
| Multiline paste | has (`Chat::paste`, chat.rs:1126, incl. queued-paste rule 994–999) |
| Undo / delete-before-cursor / space-hold | **missing** (space_hold in App only) |
| Emoji expansion + picker (new worker work) | **missing** — lives in `titi_tui::composer`, App-only |
| Model picker | has (`ModelPicker`, chat.rs:488, 2014–2291; alt+m, 8334) |
| Theme picker | has (`theme_picker_key`, chat.rs:2070–2132) |
| Login picker / OAuth | has (`login_picker_key`, chat.rs:1870) |
| Approval prompts | has (`approval_key`, chat.rs:1150; PendingApproval 458) — own panel, not `ApprovalPanel` |
| Session switcher | has (Ctrl+X switcher rows at chat.rs:4613; list at 1911, 1963) |
| Hub roster | has (`/hub` panel + `/join`/`/leave`, chat.rs:2962–3021) — **simpler than App**: no HubRevive/HubStop steering, no embedded HubRoster widget |
| Status line | has (status_bar live_snapshot, chat.rs:29, 5139) |
| STT / hold-space | missing either place (App's is a stub, app.rs:51) |
| Mouse drag selection, copy | missing (App-only; preset config round-trips through app.rs helper) |
| Transcript accordion/details toggle | missing as App's (`toggle_all_details`, app.rs:468); chat renders parsed blocks |
| History search overlay (ctrl+r) | missing in chat (App:647) |
| Appearance auto-probe (OSC 11/Mode-2031) | missing in chat (App:729–757) |
| Keybinding remap (`/keys` etc. via manager) | missing in chat (raw key match) |

Features living **only in App**: emoji, space-hold/undo, mouse selection+presets, history search,
hub revive/stop steering, App-style details accordion, appearance auto-probe, keybindings manager → **8**.

## 5. Options and honest costs

| Option | What you keep | Line/test cost | Product behaviour change |
|---|---|---|---|
| (a) Delete `App` stack | chat.rs unchanged; the ~12 free helper fns must **move out of app.rs** first (they're used 88× elsewhere). Kill 9 App-consuming test files or their App-only parts | app.rs 2492 L − ~400 L of helpers; up to ~2543 L of tests − session_log/checkpoints survivors (~600–800 L of assertions to rehome or drop) | Loses today: emoji picker (just built), space-hold/undo, mouse selection, history search, hub revive/stop, OSC-11 auto theme. Status quo otherwise |
| (b) Port missing features into chat.rs | chat.rs + the titi-tui modules (composer, space_hold) it gains imports for | ~8 features × 100–400 L each ≈ 1–2.5k L; chat.rs already 14.4k L, 209 inline tests: each port must be re-tested there | Emoji, undo/hold-space, mouse et al land in the shipped binary; two picker/interaction idioms converge |
| (c) Promote `App` to the real TUI | Delete chat.rs's ~14.4k L or dedicate it to the App loop | Massive cutover; the 9 test files survive unchanged | Loses chat.rs-only features until re-added: inline diff/image rendering (chat.rs titi_tui::diff, image), git view (/git, chat.rs:2700), secrets/provider status (2677–2700), slash/skill list, recap transcript form, 209 tests' behaviours |

## 6. Recommendation

**(b) Port, then delete** — actually two steps: first move the ~12 free helpers out of `app.rs`
into `session_log.rs`/`engine.rs`/`checkpoints.rs` so `App` stops being load-bearing, then port the
features users feel (emoji + space-hold first, both flow through `titi-tui` modules chat.rs can
import), then delete `App` and its dead-only test files in one commit. The stack's value is
behaviour, not architecture; chat.rs is the product and is richer than `App` on 6 of the 9 features
that matter. **Risk to watch**: the emoji/work-in-progress worker output lands only in
`titi_tui::composer` — if any port listens for keys before `Chat::on_key` routes paste correctly
(chat.rs:1126–1131 treats paste as composer input, not picker keystrokes), the picker will swallow
braille/^-encoded paste. **Confidence: 70%.** Would change my mind if the audit's "chat.rs is a
prototype" framing turned out real — i.e. chat.rs's own 209 inline tests cover a narrow slice and
App's engine-event paths (steer, pause, mode badge in dispatch.rs/engine_events.rs) had no
chat.rs twin; I checked steer/pause/mode exist in chat.rs only by grep of pickers, not by running
App-dispatch against chat's loop.

## Not determined here
- Precise line count of app.rs free-helper region (helpers interleave with the `App` impl; expect
  ~300–500 L to relocate).
- Whether `tui-smoke`/`docs/QA_STATUS.md` recorded any manual verification of the App stack in a
  real terminal (its absence would strengthen (a)).
- Whether `hub.rs` HubRevive/HubStop has a live herdr twin in the chat loop (`crate::herdr` exists
  on both sides; steering semantics untested because App never runs).
