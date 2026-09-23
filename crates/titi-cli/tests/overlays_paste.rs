//! Integration tests for overlay panels (model picker, session switcher,
//! approval prompt) and bracketed paste in titi-cli.
//!
//! Contract: `docs/research/agent-ux/README.md` (DoD):
//! - model picker / session switcher (`Ctrl+X`: Enter/Ctrl+D/Ctrl+N/Esc) /
//!   approval prompt are overlays composited over the frame;
//! - Esc is always cancel-without-delete;
//! - bracketed paste inserts multi-line text as one block (never executed
//!   line-by-line), long pastes collapse inline, a `.png` path becomes an
//!   `[Image #N]` attachment.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use titi_cli::app::{App, default_theme, delete_session_from, list_sessions_from, model_choices};
use titi_cli::engine::ModelCatalog;
use titi_engine::{
    EnvCredentialSource, HttpTransportFactory, ProviderDescriptor, ProviderRegistry,
    ProviderRegistryConfig,
};
use titi_providers::ApiKind;
use titi_tui::composer::PASTE_INLINE_MAX_LINES;

fn app() -> App {
    App::new(
        Arc::new(AtomicBool::new(false)),
        vec!["titi".to_owned()],
        default_theme().unwrap(),
    )
}

// ---------------------------------------------------------------------------
// Bracketed paste
// ---------------------------------------------------------------------------

#[test]
fn paste_multiline_is_one_block() {
    let mut app = app();
    let appended = app.paste("line one\nline two\nline three");
    assert_eq!(appended, "line one\nline two\nline three");
    // One paste = one insert; nothing was executed or queued.
    assert_eq!(app.queue_len(), 0);
}

#[test]
fn paste_at_threshold_is_text_beyond_collapses() {
    let mut app = app();
    let six = "a\nb\nc\nd\ne\nf";
    assert_eq!(
        app.paste(six),
        six,
        "≤ PASTE_INLINE_MAX_LINES stays verbatim"
    );

    let seven = "a\nb\nc\nd\ne\nf\ng";
    assert_eq!(app.paste(seven), "a\n… (+1 lines)");
    assert_eq!(PASTE_INLINE_MAX_LINES, 6);
}

#[test]
fn paste_png_path_becomes_image_attachment() {
    let mut app = app();
    assert_eq!(app.paste("/tmp/photo.png"), "[Image #1]");
    assert_eq!(app.paste("shot.JPEG"), "[Image #2]");
    // Non-image pastes do not consume attachment numbers.
    assert_eq!(app.paste("plain note"), "plain note");
    assert_eq!(app.paste("diagram.gif"), "[Image #3]");
}

// ---------------------------------------------------------------------------
// Model picker
// ---------------------------------------------------------------------------

#[test]
fn model_picker_selects_with_enter() {
    let mut app = app();
    app.open_model_picker();
    assert!(app.overlay_open());

    // Down once, Enter → the second model is chosen.
    assert_eq!(app.overlay_input("\x1b[B"), None, "still open after move");
    let outcome = app.overlay_input("\r");
    assert_eq!(
        outcome,
        Some(titi_cli::app::OverlayOutcome::ModelSelected(
            model_choices()[1].clone()
        ))
    );
    assert!(!app.overlay_open(), "panel closed after Enter");
}

#[test]
fn model_picker_esc_cancels_without_effect() {
    let mut app = app();
    app.open_model_picker();
    assert_eq!(app.overlay_input("\x1b"), None, "Esc → no selection");
    assert!(!app.overlay_open());
}

#[test]
fn model_picker_type_to_filter_selects_glm() {
    let mut app = app();
    app.open_model_picker();
    assert_eq!(app.overlay_input("g"), None, "filter stays open");
    assert_eq!(app.overlay_input("l"), None);
    assert_eq!(app.overlay_input("m"), None);
    let outcome = app.overlay_input("\r");
    match outcome {
        Some(titi_cli::app::OverlayOutcome::ModelSelected(id)) => {
            assert!(id.contains("glm"), "filtered confirm: {id}");
        }
        other => panic!("expected glm model, got {other:?}"),
    }
    assert!(!app.overlay_open());
}

#[test]
fn overlay_frame_composites_picker_rows() {
    let mut app = app();
    app.open_model_picker();
    let rows = app.plan_frame("", 20).viewport;
    let last = rows.last().unwrap();
    assert!(
        last.contains('╰') && last.contains('╯'),
        "composer boxRound bottom stays at the frame bottom: {last:?}"
    );
    let joined = rows.join("\n");
    assert!(
        joined.contains("Model"),
        "compact picker title visible: {joined}"
    );
}

#[test]
fn model_picker_fits_80x20_with_title_above_composer() {
    let mut app = app();
    app.set_size(80, 20);
    app.open_model_picker();
    let plan = app.plan_frame("", 20);
    assert_eq!(plan.viewport.len(), 20);
    let joined = plan.viewport.join("\n");
    assert!(joined.contains("Model"), "title must not clip: {joined}");
    let last = plan.viewport.last().unwrap();
    assert!(
        last.contains('╰') && last.contains('╯'),
        "composer remains under the picker: {last:?}"
    );
    // Title is above the composer: first overlay row is not the last frame row.
    let title_i = plan
        .viewport
        .iter()
        .position(|r| r.contains("Model"))
        .expect("title");
    assert!(title_i + 1 < plan.viewport.len(), "title above composer");
}

/// A model the registry learns about after the surface is built is
/// selectable in the picker.
///
/// The picker used to be handed the ids once, at startup. A local server
/// finishes listing well after the first frame, so its models stayed
/// unreachable for the rest of the session.
#[tokio::test]
async fn picker_offers_a_model_the_registry_learned_after_startup() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let port = listener.local_addr().expect("bound address").port();
    std::thread::spawn(move || {
        let body = r#"{"object":"list","data":[{"id":"qwen3:8b"}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        for mut stream in listener.incoming().flatten() {
            let _ = std::io::Read::read(&mut stream, &mut [0_u8; 1024]);
            let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
        }
    });

    let registry = Arc::new(
        ProviderRegistry::new(
            ProviderRegistryConfig {
                providers: vec![ProviderDescriptor {
                    id: "ollama".into(),
                    api: ApiKind::OpenAiCompletions,
                    base_url: format!("http://127.0.0.1:{port}/v1").into(),
                    credential_env: None,
                    credential_required: false,
                }],
                models: Vec::new(),
            },
            Arc::new(EnvCredentialSource),
            Arc::new(HttpTransportFactory),
        )
        .expect("registry builds from one keyless provider"),
    );

    let mut app = app();
    app.set_model_catalog(ModelCatalog::new(
        vec!["openai/gpt-4.1".to_owned()],
        Arc::clone(&registry),
    ));

    app.open_model_picker();
    assert_eq!(
        app.overlay_input("\r"),
        Some(titi_cli::app::OverlayOutcome::ModelSelected(
            "openai/gpt-4.1".to_owned()
        )),
        "before the server answers the picker offers the startup list alone"
    );

    registry.spawn_local_discovery();
    for _ in 0..100 {
        if !registry.model_ids().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        !registry.model_ids().is_empty(),
        "the local server listed its model"
    );

    app.open_model_picker();
    for key in ["q", "w", "e", "n"] {
        assert_eq!(app.overlay_input(key), None, "filter stays open");
    }
    match app.overlay_input("\r") {
        Some(titi_cli::app::OverlayOutcome::ModelSelected(id)) => {
            assert_eq!(id, "ollama/qwen3:8b", "the late model is selectable");
        }
        other => panic!("expected the discovered model, got {other:?}"),
    }
}

/// A gateway that refuses the key contributes no models, which on its own
/// looks exactly like a gateway with nothing to offer. The picker has to say
/// which it was, or the empty row is a dead end the user cannot act on.
#[tokio::test]
async fn picker_says_why_a_provider_refused_to_list() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let port = listener.local_addr().expect("bound address").port();
    std::thread::spawn(move || {
        let body = r#"{"error":{"message":"invalid api key"}}"#;
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        for mut stream in listener.incoming().flatten() {
            let _ = std::io::Read::read(&mut stream, &mut [0_u8; 1024]);
            let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
        }
    });

    let registry = Arc::new(
        ProviderRegistry::new(
            ProviderRegistryConfig {
                providers: vec![ProviderDescriptor {
                    id: "gatewayd".into(),
                    api: ApiKind::OpenAiCompletions,
                    base_url: format!("http://127.0.0.1:{port}/v1").into(),
                    credential_env: None,
                    credential_required: false,
                }],
                models: Vec::new(),
            },
            Arc::new(EnvCredentialSource),
            Arc::new(HttpTransportFactory),
        )
        .expect("registry builds from one keyless provider"),
    );

    let mut app = app();
    app.set_model_catalog(ModelCatalog::new(
        vec!["openai/gpt-4.1".to_owned()],
        Arc::clone(&registry),
    ));

    registry.spawn_local_discovery();
    for _ in 0..100 {
        if !registry.discovery_errors().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        !registry.discovery_errors().is_empty(),
        "the refusal never reached the registry"
    );

    app.open_model_picker();
    let shown = app.render().join("\n");
    assert!(
        shown.contains("gatewayd") && shown.contains("401"),
        "the picker opened without saying why the list is short: {shown}"
    );
    assert!(
        shown.contains("titi --set-key"),
        "the user is not told what to do about it: {shown}"
    );

    // The provider that refused costs only its own models: what the catalog
    // already had is still selectable.
    match app.overlay_input("\r") {
        Some(titi_cli::app::OverlayOutcome::ModelSelected(id)) => {
            assert_eq!(id, "openai/gpt-4.1");
        }
        other => panic!("expected the startup model, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Session switcher
// ---------------------------------------------------------------------------

#[test]
fn session_switcher_enter_switches_esc_cancels() {
    let mut app = app();
    app.open_session_switcher_over(vec!["current".into(), "abc123".into()]);

    // Down once, Enter → switch to "abc123".
    assert_eq!(app.overlay_input("\x1b[B"), None);
    assert_eq!(
        app.overlay_input("\r"),
        Some(titi_cli::app::OverlayOutcome::SessionSwitched(
            "abc123".into()
        ))
    );

    // Esc → cancel, nothing happens.
    app.open_session_switcher_over(vec!["current".into()]);
    assert_eq!(
        app.overlay_input("\x1b"),
        Some(titi_cli::app::OverlayOutcome::SessionCancelled)
    );
    assert!(!app.overlay_open());
}

#[test]
fn session_switcher_new_and_refresh() {
    let mut app = app();
    app.open_session_switcher_over(vec!["current".into()]);

    // Ctrl+N → new session outcome.
    assert_eq!(
        app.overlay_input("\x0e"),
        Some(titi_cli::app::OverlayOutcome::SessionNew)
    );

    // Ctrl+R refreshes in place — the panel stays open.
    app.open_session_switcher_over(vec!["current".into()]);
    assert_eq!(app.overlay_input("\x12"), None, "refresh keeps panel open");
    assert!(app.overlay_open());
}

#[test]
fn session_close_routes_through_approval() {
    let tmp = std::env::temp_dir().join(format!("titi-overlay-close-{}", std::process::id()));
    let agent_dir = tmp.join("agent");
    let sessions = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let session_file = sessions.join("deadbeef.jsonl");
    std::fs::write(&session_file, "").unwrap();
    assert_eq!(list_sessions_from(&agent_dir), vec!["deadbeef".to_owned()]);

    let mut app = app();
    app.open_session_switcher_over(vec!["current".into(), "deadbeef".into()]);

    // Down to "deadbeef", Ctrl+D → approval prompt opens, nothing deleted.
    assert_eq!(app.overlay_input("\x1b[B"), None);
    assert_eq!(app.overlay_input("\x04"), None, "close waits for approval");
    assert!(app.overlay_open(), "approval prompt is shown");

    // Esc on the approval → cancel-without-delete.
    assert_eq!(
        app.overlay_input("\x1b"),
        Some(titi_cli::app::OverlayOutcome::Approval(false))
    );
    assert!(session_file.exists(), "Esc never deletes");

    // Again through the gate, then explicit Yes.
    app.open_session_switcher_over(vec!["current".into(), "deadbeef".into()]);
    app.overlay_input("\x1b[B");
    app.overlay_input("\x04");
    assert_eq!(
        app.overlay_input("\r"),
        Some(titi_cli::app::OverlayOutcome::Approval(true))
    );
    let pending = app.take_pending_close();
    assert_eq!(pending.as_deref(), Some("deadbeef"));

    // The application executes the approved deletion.
    delete_session_from(&agent_dir, pending.unwrap().as_str()).unwrap();
    assert!(!session_file.exists(), "approved close deletes the session");
    assert!(!app.overlay_open());

    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn live_session_close_cannot_delete_current() {
    let mut app = app();
    app.open_session_switcher_over(vec!["current".into()]);

    // Ctrl+D on the live session still gates through approval…
    assert_eq!(app.overlay_input("\x04"), None);
    assert!(app.overlay_open());
    assert_eq!(
        app.overlay_input("\r"),
        Some(titi_cli::app::OverlayOutcome::Approval(true))
    );
    // …but the pending id is "current", which the binary refuses to delete.
    assert_eq!(app.take_pending_close().as_deref(), Some("current"));
}
