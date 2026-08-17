use std::fs;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_model_provider_info::WireApi;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::TempDirExt;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_requests;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use pretty_assertions::assert_eq;
use tokio::time::Duration;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::http::Method;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_switches_to_http_on_upgrade_required_connect() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    Mock::given(method("GET"))
        .and(path_regex(".*/responses$"))
        .respond_with(ResponseTemplate::new(426))
        .mount(&server)
        .await;

    let response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            // If we don't treat 426 specially, the sampling loop would retry the WebSocket
            // handshake before switching to the HTTP transport.
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let test = builder.build(&server).await?;

    test.submit_turn("hello").await?;

    let requests = server.received_requests().await.unwrap_or_default();
    let websocket_attempts = requests
        .iter()
        .filter(|req| req.method == Method::GET && req.url.path().ends_with("/responses"))
        .count();
    let http_attempts = requests
        .iter()
        .filter(|req| req.method == Method::POST && req.url.path().ends_with("/responses"))
        .count();

    // The startup prewarm request sees 426 and immediately switches the session to HTTP fallback,
    // so the first turn goes straight to HTTP with no additional websocket connect attempt.
    assert_eq!(websocket_attempts, 1);
    assert_eq!(http_attempts, 1);
    assert_eq!(response_mock.requests().len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_switches_to_http_after_retries_exhausted() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let test = builder.build(&server).await?;

    test.submit_turn("hello").await?;

    let requests = server.received_requests().await.unwrap_or_default();
    let websocket_attempts = requests
        .iter()
        .filter(|req| req.method == Method::GET && req.url.path().ends_with("/responses"))
        .count();
    let http_attempts = requests
        .iter()
        .filter(|req| req.method == Method::POST && req.url.path().ends_with("/responses"))
        .count();

    // Deferred request prewarm is attempted at startup.
    // The first turn then makes 3 websocket stream attempts (initial try + 2 retries),
    // after which fallback activates and the request is replayed over HTTP.
    assert_eq!(websocket_attempts, 4);
    assert_eq!(http_attempts, 1);
    assert_eq!(response_mock.requests().len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_runs_transport_error_hooks() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex()
        .with_pre_build_hook(write_transport_error_hook)
        .with_config({
            let base_url = format!("{}/v1", server.uri());
            move |config| {
                config.model_provider.base_url = Some(base_url);
                config.model_provider.wire_api = WireApi::Responses;
                config.model_provider.supports_websockets = true;
                config.model_provider.stream_max_retries = Some(2);
                config.model_provider.request_max_retries = Some(0);
                config
                    .features
                    .enable(Feature::CodexHooks)
                    .expect("test config should allow feature update");
                trust_discovered_hooks(config);
            }
        });
    let test = builder.build(&server).await?;

    test.submit_turn("hello").await?;

    let hook_inputs = read_transport_error_hook_inputs(test.codex_home_path())?;
    let phases = hook_inputs
        .iter()
        .map(|input| input["phase"].as_str().expect("phase should be a string"))
        .collect::<Vec<_>>();
    assert_eq!(phases, vec!["retry", "retry", "fallbackToHttp"]);

    for input in &hook_inputs {
        assert_eq!(input["hook_event_name"], "TransportError");
        assert_eq!(input["transport"], "responses_websocket");
        assert_eq!(input["max_retries"], 2);
    }
    assert_eq!(hook_inputs[0]["retry_count"], 1);
    assert_eq!(hook_inputs[0]["message"], "Reconnecting... 1/2");
    assert_eq!(hook_inputs[1]["retry_count"], 2);
    assert_eq!(hook_inputs[1]["message"], "Reconnecting... 2/2");
    assert_eq!(hook_inputs[2]["retry_count"], 2);
    assert_eq!(
        hook_inputs[2]["message"],
        "Falling back from WebSockets to HTTPS transport"
    );
    assert_eq!(response_mock.requests().len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_hides_first_websocket_retry_stream_error() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let TestCodex {
        codex,
        session_configured,
        cwd,
        ..
    } = builder.build(&server).await?;
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd.path());

    codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(local_requests(cwd.abs())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            }),
        )
        .await?;

    let mut stream_error_messages = Vec::new();
    loop {
        let event = timeout(Duration::from_secs(10), codex.next_event())
            .await
            .expect("timeout waiting for event")
            .expect("event stream ended unexpectedly")
            .msg;
        match event {
            EventMsg::StreamError(e) => stream_error_messages.push(e.message),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    let expected_stream_errors = if cfg!(debug_assertions) {
        vec!["Reconnecting... 1/2", "Reconnecting... 2/2"]
    } else {
        vec!["Reconnecting... 2/2"]
    };
    assert_eq!(stream_error_messages, expected_stream_errors);
    assert_eq!(response_mock.requests().len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_is_sticky_across_turns() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
            sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
        ],
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let test = builder.build(&server).await?;

    test.submit_turn("first").await?;
    test.submit_turn("second").await?;

    let requests = server.received_requests().await.unwrap_or_default();
    let websocket_attempts = requests
        .iter()
        .filter(|req| req.method == Method::GET && req.url.path().ends_with("/responses"))
        .count();
    let http_attempts = requests
        .iter()
        .filter(|req| req.method == Method::POST && req.url.path().ends_with("/responses"))
        .count();

    // WebSocket attempts all happen on the first turn:
    // 1 deferred request prewarm attempt (startup) + 3 stream attempts
    // (initial try + 2 retries) before fallback.
    // Fallback is sticky, so the second turn stays on HTTP and adds no websocket attempts.
    assert_eq!(websocket_attempts, 4);
    assert_eq!(http_attempts, 2);
    assert_eq!(response_mock.requests().len(), 2);

    Ok(())
}

fn write_transport_error_hook(home: &Path) {
    let script_path = home.join("transport_error_hook.py");
    let log_path = home.join("transport_error_hook_log.jsonl");
    let script = format!(
        r#"import json
from pathlib import Path
import sys

log_path = Path(r"{log_path}")
payload = json.load(sys.stdin)

with log_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(payload) + "\n")
"#,
        log_path = log_path.display(),
    );
    let python = if cfg!(windows) { "python" } else { "python3" };
    let hooks = serde_json::json!({
        "hooks": {
            "TransportError": [{
                "hooks": [{
                    "type": "command",
                    "command": format!("{python} \"{}\"", script_path.display()),
                }]
            }]
        }
    });

    fs::write(&script_path, script).expect("write transport error hook script");
    fs::write(home.join("hooks.json"), hooks.to_string()).expect("write hooks.json");
}

fn read_transport_error_hook_inputs(home: &Path) -> Result<Vec<serde_json::Value>> {
    let text = fs::read_to_string(home.join("transport_error_hook_log.jsonl"))
        .context("read transport error hook log")?;
    text.lines()
        .map(|line| serde_json::from_str(line).context("parse transport error hook input"))
        .collect()
}
