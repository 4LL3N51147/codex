//! Shared retry and transport fallback decisions for Responses requests.

use std::sync::Arc;
use std::time::Duration;

use crate::client::ModelClientSession;
use crate::hook_runtime::TransportErrorHookPayload;
use crate::hook_runtime::run_transport_error_hooks;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::util::backoff;
use codex_client::RetryOperation;
use codex_hooks::TransportErrorPhase;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::WarningEvent;
use tracing::warn;

const INITIAL_CONNECTION_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_CONNECTION_RETRY_DELAY: Duration = Duration::from_secs(60);
const MAX_HOOK_RECOVERY_ATTEMPTS: u64 = 3;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ResponsesStreamRequest {
    Sampling,
    RemoteCompactionV2,
}

pub(crate) struct ResponsesStreamRetryState {
    retries: u64,
    connection_retries: u64,
    connection_retry_delay: Duration,
    hook_recovery_attempts: u64,
}

impl Default for ResponsesStreamRetryState {
    fn default() -> Self {
        Self {
            retries: 0,
            connection_retries: 0,
            connection_retry_delay: INITIAL_CONNECTION_RETRY_DELAY,
            hook_recovery_attempts: 0,
        }
    }
}

/// Handles a retryable stream error and returns `Ok(())` when the caller should
/// retry the request loop.
pub(crate) async fn handle_retryable_response_stream_error(
    retry_state: &mut ResponsesStreamRetryState,
    max_retries: u64,
    err: CodexErr,
    client_session: &mut ModelClientSession,
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    request: ResponsesStreamRequest,
) -> Result<(), CodexErr> {
    let operation = match request {
        ResponsesStreamRequest::Sampling => RetryOperation::Sampling,
        ResponsesStreamRequest::RemoteCompactionV2 => RetryOperation::RemoteCompactionV2,
    };

    if matches!(request, ResponsesStreamRequest::Sampling)
        && matches!(err.details(), CodexErrorDetails::ConnectionFailed(_))
        && !turn_context.session_source.is_internal()
        && !turn_context.provider.info().is_amazon_bedrock()
    {
        let retry_delay = retry_state.connection_retry_delay;
        warn!(
            turn_id = %turn_context.sub_id,
            error = %err,
            ?retry_delay,
            "stream connection failed; waiting to retry"
        );
        sess.notify_stream_error(turn_context, "Reconnecting... waiting for network", err)
            .await;
        retry_state.connection_retries = retry_state.connection_retries.saturating_add(1);
        codex_client::record_retry!(retry_state.connection_retries, retry_delay, operation);
        tokio::time::sleep(retry_delay).await;
        retry_state.connection_retry_delay = retry_delay
            .saturating_mul(2)
            .min(MAX_CONNECTION_RETRY_DELAY);
        return Ok(());
    }

    let transport = transport_label(sess);
    if retry_state.retries >= max_retries
        && client_session.try_switch_fallback_transport(
            &turn_context.session_telemetry,
            &turn_context.model_info,
        )
    {
        let _ = run_transport_error_hooks(
            sess,
            turn_context,
            TransportErrorHookPayload {
                transport,
                phase: TransportErrorPhase::FallbackToHttp,
                message: "Falling back from WebSockets to HTTPS transport".to_string(),
                retry_count: retry_state.retries,
                max_retries,
                http_status_code: err.http_status_code_value(),
                details: err.to_string(),
            },
        )
        .await;
        sess.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: format!("Falling back from WebSockets to HTTPS transport. {err:#}"),
            }),
        )
        .await;
        retry_state.retries = 0;
        return Ok(());
    }

    if retry_state.retries < max_retries {
        retry_state.retries += 1;
        let retry_count = retry_state.retries;
        let delay = err.retry_delay().unwrap_or_else(|| backoff(retry_count));
        let message = format!("Reconnecting... {retry_count}/{max_retries}");
        log_retry(request, turn_context, &err, retry_count, max_retries, delay);
        let _ = run_transport_error_hooks(
            sess,
            turn_context,
            TransportErrorHookPayload {
                transport,
                phase: TransportErrorPhase::Retry,
                message: message.clone(),
                retry_count,
                max_retries,
                http_status_code: err.http_status_code_value(),
                details: err.to_string(),
            },
        )
        .await;

        // In release builds, hide the first websocket retry notification to reduce noisy
        // transient reconnect messages. In debug builds, keep full visibility for diagnosis.
        let report_error = retry_count > 1
            || cfg!(debug_assertions)
            || !sess.services.model_client.responses_websocket_enabled();
        if report_error {
            // Surface retry information to any UI/front-end so the user understands what is
            // happening instead of staring at a seemingly frozen screen.
            sess.notify_stream_error(turn_context, message, err).await;
        }
        codex_client::record_retry!(retry_count, delay, operation);
        tokio::time::sleep(delay).await;
        return Ok(());
    }

    let outcome = run_transport_error_hooks(
        sess,
        turn_context,
        TransportErrorHookPayload {
            transport,
            phase: TransportErrorPhase::Exhausted,
            message: "Transport retries exhausted".to_string(),
            retry_count: retry_state.retries,
            max_retries,
            http_status_code: err.http_status_code_value(),
            details: err.to_string(),
        },
    )
    .await;

    if transport_error_hook_completed_successfully(&outcome)
        && retry_state.hook_recovery_attempts < MAX_HOOK_RECOVERY_ATTEMPTS
    {
        retry_state.hook_recovery_attempts += 1;
        retry_state.retries = 0;
        warn!(
            request = ?request,
            hook_recovery_attempts = retry_state.hook_recovery_attempts,
            max_hook_recovery_attempts = MAX_HOOK_RECOVERY_ATTEMPTS,
            "transport error hook completed; retrying request after hook recovery"
        );
        return Ok(());
    }

    Err(err)
}

fn transport_error_hook_completed_successfully(
    outcome: &codex_hooks::TransportErrorOutcome,
) -> bool {
    outcome
        .hook_events
        .iter()
        .any(|event| event.run.status == HookRunStatus::Completed)
}

fn transport_label(sess: &Session) -> &'static str {
    if sess.services.model_client.responses_websocket_enabled() {
        "responses_websocket"
    } else {
        "responses_https"
    }
}

fn log_retry(
    request: ResponsesStreamRequest,
    turn_context: &TurnContext,
    err: &CodexErr,
    retries: u64,
    max_retries: u64,
    delay: Duration,
) {
    match request {
        ResponsesStreamRequest::Sampling => {
            warn!(
                turn_id = %turn_context.sub_id,
                retries,
                max_retries,
                sampling_error = %err,
                "stream disconnected - retrying sampling request ({retries}/{max_retries} in {delay:?})...",
            );
        }
        ResponsesStreamRequest::RemoteCompactionV2 => {
            warn!(
                turn_id = %turn_context.sub_id,
                retries,
                max_retries,
                compact_error = %err,
                "remote compaction v2 stream failed; retrying request after delay"
            );
        }
    }
}

#[cfg(test)]
#[path = "responses_retry_tests.rs"]
mod tests;
