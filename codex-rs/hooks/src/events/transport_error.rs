use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookOutputEntry;
use codex_protocol::protocol::HookOutputEntryKind;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookRunSummary;
use codex_utils_absolute_path::AbsolutePathBuf;

use super::common;
use crate::engine::ClaudeHooksEngine;
use crate::engine::ConfiguredHandler;
use crate::engine::HandlerRunResult;
use crate::engine::dispatcher;
use crate::engine::output_parser;
use crate::schema::NullableString;
use crate::schema::SubagentCommandInputFields;
use crate::schema::TransportErrorCommandInput;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportErrorPhase {
    Retry,
    FallbackToHttp,
    Exhausted,
}

impl TransportErrorPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::FallbackToHttp => "fallbackToHttp",
            Self::Exhausted => "exhausted",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TransportErrorRequest {
    pub session_id: ThreadId,
    pub turn_id: String,
    pub subagent: Option<common::SubagentHookContext>,
    pub cwd: AbsolutePathBuf,
    pub transcript_path: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub transport: String,
    pub phase: TransportErrorPhase,
    pub message: String,
    pub retry_count: u64,
    pub max_retries: u64,
    pub http_status_code: Option<u16>,
    pub details: String,
}

#[derive(Debug)]
pub struct TransportErrorOutcome {
    pub hook_events: Vec<HookCompletedEvent>,
}

#[derive(Debug, PartialEq, Eq)]
struct TransportErrorHandlerData;

pub(crate) fn preview(
    handlers: &[ConfiguredHandler],
    _request: &TransportErrorRequest,
) -> Vec<HookRunSummary> {
    dispatcher::select_handlers(
        handlers,
        HookEventName::TransportError,
        /*matcher_input*/ None,
    )
    .into_iter()
    .map(|handler| dispatcher::running_summary(&handler))
    .collect()
}

pub(crate) async fn run(
    engine: &ClaudeHooksEngine,
    request: TransportErrorRequest,
) -> TransportErrorOutcome {
    let matched = dispatcher::select_handlers(
        &engine.handlers,
        HookEventName::TransportError,
        /*matcher_input*/ None,
    );
    if matched.is_empty() {
        return TransportErrorOutcome {
            hook_events: Vec::new(),
        };
    }

    let subagent = SubagentCommandInputFields::from(request.subagent.as_ref());
    let input_json = match serde_json::to_string(&TransportErrorCommandInput {
        session_id: request.session_id.to_string(),
        turn_id: request.turn_id.clone(),
        agent_id: subagent.agent_id,
        agent_type: subagent.agent_type,
        transcript_path: NullableString::from_path(request.transcript_path.clone()),
        cwd: request.cwd.display().to_string(),
        hook_event_name: "TransportError".to_string(),
        model: request.model.clone(),
        permission_mode: request.permission_mode.clone(),
        transport: request.transport.clone(),
        phase: request.phase.as_str().to_string(),
        message: request.message.clone(),
        retry_count: request.retry_count,
        max_retries: request.max_retries,
        http_status_code: request.http_status_code,
        details: request.details.clone(),
    }) {
        Ok(input_json) => input_json,
        Err(error) => {
            return TransportErrorOutcome {
                hook_events: common::serialization_failure_hook_events(
                    matched,
                    Some(request.turn_id),
                    format!("failed to serialize transport error hook input: {error}"),
                ),
            };
        }
    };

    let results = dispatcher::execute_handlers(
        engine,
        matched,
        input_json,
        request.cwd.as_path(),
        Some(request.turn_id),
        parse_completed,
    )
    .await;

    TransportErrorOutcome {
        hook_events: results.into_iter().map(|result| result.completed).collect(),
    }
}

fn parse_completed(
    handler: &ConfiguredHandler,
    run_result: HandlerRunResult,
    turn_id: Option<String>,
) -> dispatcher::ParsedHandler<TransportErrorHandlerData> {
    let mut entries = Vec::new();
    let mut status = HookRunStatus::Completed;

    match run_result.error.as_deref() {
        Some(error) => {
            status = HookRunStatus::Failed;
            entries.push(HookOutputEntry {
                kind: HookOutputEntryKind::Error,
                text: error.to_string(),
            });
        }
        None => match run_result.exit_code {
            Some(0) => {
                let trimmed_stdout = run_result.stdout.trim();
                if trimmed_stdout.is_empty() {
                } else if let Some(parsed) =
                    output_parser::parse_transport_error(&run_result.stdout)
                {
                    if let Some(system_message) = parsed.universal.system_message {
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Warning,
                            text: system_message,
                        });
                    }
                    if !parsed.universal.continue_processing {
                        status = HookRunStatus::Stopped;
                        if let Some(stop_reason) = parsed.universal.stop_reason {
                            entries.push(HookOutputEntry {
                                kind: HookOutputEntryKind::Stop,
                                text: stop_reason,
                            });
                        }
                    } else if let Some(invalid_reason) = parsed.invalid_reason {
                        status = HookRunStatus::Failed;
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Error,
                            text: invalid_reason,
                        });
                    }
                    let _ = parsed.universal.suppress_output;
                } else if output_parser::looks_like_json(&run_result.stdout) {
                    status = HookRunStatus::Failed;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: "hook returned invalid transport error JSON output".to_string(),
                    });
                } else {
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Feedback,
                        text: trimmed_stdout.to_string(),
                    });
                }
            }
            Some(2) => {
                if let Some(reason) = common::trimmed_non_empty(&run_result.stderr) {
                    status = HookRunStatus::Blocked;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Feedback,
                        text: reason,
                    });
                } else {
                    status = HookRunStatus::Failed;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: "hook exited with code 2 but did not provide a reason".to_string(),
                    });
                }
            }
            Some(code) => {
                status = HookRunStatus::Failed;
                let text = common::trimmed_non_empty(&run_result.stderr)
                    .unwrap_or_else(|| format!("hook exited with code {code}"));
                entries.push(HookOutputEntry {
                    kind: HookOutputEntryKind::Error,
                    text,
                });
            }
            None => {
                status = HookRunStatus::Failed;
                entries.push(HookOutputEntry {
                    kind: HookOutputEntryKind::Error,
                    text: "hook terminated by signal".to_string(),
                });
            }
        },
    }

    dispatcher::ParsedHandler {
        completed: HookCompletedEvent {
            turn_id,
            run: dispatcher::completed_summary(handler, &run_result, status, entries),
        },
        data: TransportErrorHandlerData,
        completion_order: 0,
    }
}
