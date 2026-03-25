//! Python orchestrator — the self-modifiable execution loop.
//!
//! Replaces the Rust `ExecutionLoop::run()` with versioned Python code
//! executed via Monty. The orchestrator is the "glue layer" between the
//! LLM and tools — tool dispatch, output formatting, state management,
//! truncation — all in Python, patchable by the self-improvement Mission.
//!
//! Host functions exposed to the orchestrator Python:
//! - `__llm_complete__` — make an LLM call
//! - `__execute_code_step__` — run user CodeAct code in a nested Monty VM
//! - `__execute_action__` — execute a single tool action
//! - `__check_signals__` — poll for stop/inject signals
//! - `__emit_event__` — broadcast a ThreadEvent
//! - `__add_message__` — append a message to the thread
//! - `__save_checkpoint__` — persist thread state
//! - `__transition_to__` — change thread state (validated)
//! - `__retrieve_docs__` — query memory docs
//! - `__check_budget__` — remaining tokens/time/USD
//! - `__get_actions__` — available tool definitions

use std::sync::Arc;

use std::collections::HashMap;

use monty::{
    ExtFunctionResult, LimitedTracker, MontyObject, MontyRun, NameLookupResult, PrintWriter,
    ResourceLimits, RunProgress,
};
use tracing::{debug, warn};

use crate::capability::lease::LeaseManager;
use crate::capability::policy::PolicyEngine;
use crate::memory::RetrievalEngine;
use crate::runtime::messaging::{SignalReceiver, ThreadOutcome, ThreadSignal};
use crate::traits::effect::{EffectExecutor, ThreadExecutionContext};
use crate::traits::llm::{LlmBackend, LlmCallConfig};
use crate::traits::store::Store;
use crate::types::error::EngineError;
use crate::types::event::{EventKind, ThreadEvent};
use crate::types::message::ThreadMessage;
use crate::types::project::ProjectId;
use crate::types::step::{StepId, TokenUsage};
use crate::types::thread::Thread;

use super::scripting::{execute_code, json_to_monty, monty_to_json, monty_to_string};

/// The compiled-in default orchestrator (v0).
const DEFAULT_ORCHESTRATOR: &str = include_str!("../../orchestrator/default.py");

/// Well-known title for orchestrator code in the Store.
pub const ORCHESTRATOR_TITLE: &str = "orchestrator:main";

/// Well-known tag for orchestrator code docs.
pub const ORCHESTRATOR_TAG: &str = "orchestrator_code";

/// Result of running the orchestrator.
pub struct OrchestratorResult {
    /// The thread outcome parsed from the orchestrator's return value.
    pub outcome: ThreadOutcome,
    /// Total tokens used by LLM calls within the orchestrator.
    pub tokens_used: TokenUsage,
}

/// Resource limits for the orchestrator VM.
fn orchestrator_limits() -> ResourceLimits {
    ResourceLimits::new()
        .max_duration(std::time::Duration::from_secs(300)) // 5 min (longer than user code)
        .max_allocations(5_000_000)
        .max_memory(128 * 1024 * 1024) // 128 MB
}

/// Load orchestrator code: runtime version from Store, or compiled-in default.
pub async fn load_orchestrator(
    store: Option<&Arc<dyn Store>>,
    project_id: ProjectId,
) -> (String, u64) {
    if let Some(store) = store
        && let Ok(docs) = store.list_memory_docs(project_id).await
        && let Some(doc) = docs
            .iter()
            .filter(|d| {
                d.title == ORCHESTRATOR_TITLE
                    && d.tags.contains(&ORCHESTRATOR_TAG.to_string())
            })
            .max_by_key(|d| {
                d.metadata
                    .get("version")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
            })
    {
        let version = doc
            .metadata
            .get("version")
            .and_then(|v| v.as_u64())
            .unwrap_or(1);
        debug!(version, "loaded runtime orchestrator");
        return (doc.content.clone(), version);
    }
    debug!("using compiled-in default orchestrator (v0)");
    (DEFAULT_ORCHESTRATOR.to_string(), 0)
}

/// Execute the orchestrator Python code with host function dispatch.
///
/// This is the core function that replaces `ExecutionLoop::run()`'s inner loop.
/// The orchestrator Python calls host functions via Monty's suspension mechanism,
/// and this function handles each suspension by delegating to the appropriate
/// Rust implementation.
#[allow(clippy::too_many_arguments)]
pub async fn execute_orchestrator(
    code: &str,
    thread: &mut Thread,
    llm: &Arc<dyn LlmBackend>,
    effects: &Arc<dyn EffectExecutor>,
    leases: &Arc<LeaseManager>,
    policy: &Arc<PolicyEngine>,
    signal_rx: &mut SignalReceiver,
    event_tx: Option<&tokio::sync::broadcast::Sender<ThreadEvent>>,
    retrieval: Option<&RetrievalEngine>,
    _store: Option<&Arc<dyn Store>>,
    persisted_state: &serde_json::Value,
) -> Result<OrchestratorResult, EngineError> {
    let mut total_tokens = TokenUsage::default();

    // Build context variables for the orchestrator
    let (input_names, input_values) = build_orchestrator_inputs(thread, persisted_state);

    // Parse and compile
    let runner = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        MontyRun::new(code.to_string(), "orchestrator.py", input_names)
    })) {
        Ok(Ok(runner)) => runner,
        Ok(Err(e)) => {
            return Err(EngineError::Effect {
                reason: format!("Orchestrator parse error: {e}"),
            });
        }
        Err(_) => {
            return Err(EngineError::Effect {
                reason: "Monty VM panicked during orchestrator parsing".into(),
            });
        }
    };

    // Start execution
    let mut stdout = String::new();
    let tracker = LimitedTracker::new(orchestrator_limits());

    let run_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runner.start(input_values, tracker, PrintWriter::Collect(&mut stdout))
    }));

    let mut progress = match run_result {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            return Err(EngineError::Effect {
                reason: format!("Orchestrator runtime error: {e}"),
            });
        }
        Err(_) => {
            return Err(EngineError::Effect {
                reason: "Monty VM panicked during orchestrator start".into(),
            });
        }
    };

    // Drive the orchestrator dispatch loop
    let mut final_result: Option<serde_json::Value> = None;

    loop {
        match progress {
            RunProgress::Complete(obj) => {
                // Orchestrator finished without calling FINAL — shouldn't happen
                // but handle gracefully
                let result = monty_to_json(&obj);
                return Ok(OrchestratorResult {
                    outcome: parse_outcome(&result),
                    tokens_used: total_tokens,
                });
            }

            RunProgress::FunctionCall(call) => {
                let action_name = call.function_name.clone();
                let args = &call.args;
                let kwargs = &call.kwargs;

                debug!(action = %action_name, "orchestrator: host function call");

                let ext_result = match action_name.as_str() {
                    // FINAL(result) — orchestrator returns its outcome
                    "FINAL" => {
                        let val = args.first().map(monty_to_json).unwrap_or_default();
                        final_result = Some(val);
                        ExtFunctionResult::Return(MontyObject::None)
                    }

                    // __llm_complete__(messages, actions, config)
                    "__llm_complete__" => {
                        handle_llm_complete(args, kwargs, thread, llm, effects, leases, &mut total_tokens)
                            .await
                    }

                    // __execute_code_step__(code, state)
                    "__execute_code_step__" => {
                        handle_execute_code_step(args, kwargs, thread, llm, effects, leases, policy)
                            .await
                    }

                    // __execute_action__(name, params)
                    "__execute_action__" => {
                        handle_execute_action(args, kwargs, thread, effects, leases, policy).await
                    }

                    // __check_signals__()
                    "__check_signals__" => handle_check_signals(signal_rx),

                    // __emit_event__(kind, **data)
                    "__emit_event__" => handle_emit_event(args, kwargs, thread, event_tx),

                    // __add_message__(role, content)
                    "__add_message__" => handle_add_message(args, kwargs, thread),

                    // __save_checkpoint__(state, counters)
                    "__save_checkpoint__" => handle_save_checkpoint(args, kwargs, thread),

                    // __transition_to__(state, reason)
                    "__transition_to__" => handle_transition_to(args, kwargs, thread),

                    // __retrieve_docs__(goal, max_docs)
                    "__retrieve_docs__" => {
                        handle_retrieve_docs(args, kwargs, thread, retrieval).await
                    }

                    // __check_budget__()"
                    "__check_budget__" => handle_check_budget(thread),

                    // __get_actions__()
                    "__get_actions__" => handle_get_actions(thread, effects, leases).await,

                    // Unknown — error
                    other => ExtFunctionResult::Error(monty::MontyException::new(
                        monty::ExcType::NameError,
                        Some(format!("Unknown orchestrator host function: {other}")),
                    )),
                };

                // Resume the orchestrator VM
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    call.resume(ext_result, PrintWriter::Collect(&mut stdout))
                })) {
                    Ok(Ok(p)) => progress = p,
                    Ok(Err(e)) => {
                        return Err(EngineError::Effect {
                            reason: format!("Orchestrator error after resume: {e}"),
                        });
                    }
                    Err(_) => {
                        return Err(EngineError::Effect {
                            reason: "Monty VM panicked during orchestrator resume".into(),
                        });
                    }
                }

                // If FINAL was called, the VM should complete on next iteration
                if final_result.is_some() {
                    continue;
                }
            }

            RunProgress::NameLookup(lookup) => {
                // Undefined variable — resume with NameError
                let name = lookup.name.clone();
                debug!(name = %name, "orchestrator: unresolved name");
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    lookup.resume(
                        NameLookupResult::Undefined,
                        PrintWriter::Collect(&mut stdout),
                    )
                })) {
                    Ok(Ok(p)) => progress = p,
                    Ok(Err(e)) => {
                        return Err(EngineError::Effect {
                            reason: format!("Orchestrator NameError '{name}': {e}"),
                        });
                    }
                    Err(_) => {
                        return Err(EngineError::Effect {
                            reason: format!("Monty panic on NameLookup '{name}'"),
                        });
                    }
                }
            }

            RunProgress::OsCall(_) => {
                return Err(EngineError::Effect {
                    reason: "Orchestrator attempted OS call (blocked)".into(),
                });
            }

            RunProgress::ResolveFutures(_) => {
                return Err(EngineError::Effect {
                    reason: "Orchestrator attempted async (not supported)".into(),
                });
            }
        }
    }
}

// ── Host function handlers ──────────────────────────────────

/// Handle `__llm_complete__(messages, actions, config)`.
///
/// Calls the LLM and returns the response as a dict:
/// `{type: "text"|"code"|"actions", content/code/calls: ..., usage: {...}}`
async fn handle_llm_complete(
    _args: &[MontyObject],
    _kwargs: &[(MontyObject, MontyObject)],
    thread: &Thread,
    llm: &Arc<dyn LlmBackend>,
    effects: &Arc<dyn EffectExecutor>,
    leases: &Arc<LeaseManager>,
    total_tokens: &mut TokenUsage,
) -> ExtFunctionResult {
    use crate::types::step::LlmResponse;

    // Build messages from thread (the orchestrator's __add_message__ calls
    // have already populated thread.messages)
    let active_leases = leases.active_for_thread(thread.id).await;
    let actions = effects
        .available_actions(&active_leases)
        .await
        .unwrap_or_default();

    let config = LlmCallConfig {
        max_tokens: None,
        temperature: None,
        force_text: false,
        depth: thread.config.depth,
        metadata: HashMap::new(),
    };

    match llm.complete(&thread.messages, &actions, &config).await {
        Ok(output) => {
            total_tokens.input_tokens += output.usage.input_tokens;
            total_tokens.output_tokens += output.usage.output_tokens;

            let usage = serde_json::json!({
                "input_tokens": output.usage.input_tokens,
                "output_tokens": output.usage.output_tokens,
            });

            let result = match output.response {
                LlmResponse::Text(text) => {
                    serde_json::json!({"type": "text", "content": text, "usage": usage})
                }
                LlmResponse::Code { code, .. } => {
                    serde_json::json!({"type": "code", "code": code, "usage": usage})
                }
                LlmResponse::ActionCalls { calls, .. } => {
                    let calls_json: Vec<serde_json::Value> = calls
                        .iter()
                        .map(|c| {
                            serde_json::json!({
                                "name": c.action_name,
                                "call_id": c.id,
                                "params": c.parameters,
                            })
                        })
                        .collect();
                    serde_json::json!({"type": "actions", "calls": calls_json, "usage": usage})
                }
            };

            ExtFunctionResult::Return(json_to_monty(&result))
        }
        Err(e) => ExtFunctionResult::Error(monty::MontyException::new(
            monty::ExcType::RuntimeError,
            Some(format!("LLM call failed: {e}")),
        )),
    }
}

/// Handle `__execute_code_step__(code, state)`.
///
/// Runs user CodeAct code in a nested Monty VM with full tool dispatch.
/// Returns a dict with stdout, return_value, action_results, etc.
async fn handle_execute_code_step(
    args: &[MontyObject],
    _kwargs: &[(MontyObject, MontyObject)],
    thread: &Thread,
    llm: &Arc<dyn LlmBackend>,
    effects: &Arc<dyn EffectExecutor>,
    leases: &Arc<LeaseManager>,
    policy: &Arc<PolicyEngine>,
) -> ExtFunctionResult {
    let code = match args.first() {
        Some(obj) => monty_to_string(obj),
        None => {
            return ExtFunctionResult::Error(monty::MontyException::new(
                monty::ExcType::TypeError,
                Some("__execute_code_step__ requires a code string".into()),
            ))
        }
    };

    let state = args
        .get(1)
        .map(monty_to_json)
        .unwrap_or(serde_json::json!({}));

    let exec_ctx = ThreadExecutionContext {
        thread_id: thread.id,
        thread_type: thread.thread_type,
        project_id: thread.project_id,
        user_id: "orchestrator".into(),
        step_id: StepId::new(),
    };

    // Run user code in a nested Monty VM (same pattern as rlm_query)
    match Box::pin(execute_code(
        &code, thread, llm, effects, leases, policy, &exec_ctx, &[], &state,
    ))
    .await
    {
        Ok(result) => {
            let action_results: Vec<serde_json::Value> = result
                .action_results
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "action_name": r.action_name,
                        "output": r.output,
                        "is_error": r.is_error,
                        "duration_ms": r.duration.as_millis(),
                    })
                })
                .collect();

            let result_json = serde_json::json!({
                "return_value": result.return_value,
                "stdout": result.stdout,
                "action_results": action_results,
                "final_answer": result.final_answer,
                "had_error": result.had_error,
                "need_approval": result.need_approval.as_ref().map(|na| {
                    match na {
                        ThreadOutcome::NeedApproval { action_name, call_id, parameters } => {
                            serde_json::json!({
                                "action_name": action_name,
                                "call_id": call_id,
                                "parameters": parameters,
                            })
                        }
                        _ => serde_json::Value::Null,
                    }
                }),
            });

            ExtFunctionResult::Return(json_to_monty(&result_json))
        }
        Err(e) => ExtFunctionResult::Error(monty::MontyException::new(
            monty::ExcType::RuntimeError,
            Some(format!("Code execution failed: {e}")),
        )),
    }
}

/// Handle `__execute_action__(name, params)`.
async fn handle_execute_action(
    args: &[MontyObject],
    kwargs: &[(MontyObject, MontyObject)],
    thread: &Thread,
    effects: &Arc<dyn EffectExecutor>,
    leases: &Arc<LeaseManager>,
    policy: &Arc<PolicyEngine>,
) -> ExtFunctionResult {
    let name = match extract_string_arg(args, kwargs, "name", 0) {
        Some(n) => n,
        None => {
            return ExtFunctionResult::Error(monty::MontyException::new(
                monty::ExcType::TypeError,
                Some("__execute_action__ requires a name argument".into()),
            ))
        }
    };

    let params = args
        .get(1)
        .map(monty_to_json)
        .unwrap_or(serde_json::json!({}));

    let exec_ctx = ThreadExecutionContext {
        thread_id: thread.id,
        thread_type: thread.thread_type,
        project_id: thread.project_id,
        user_id: "orchestrator".into(),
        step_id: StepId::new(),
    };

    // Find lease for this action
    let lease = match leases.find_lease_for_action(thread.id, &name).await {
        Some(l) => l,
        None => {
            let result = serde_json::json!({
                "output": {"error": format!("No lease for action '{name}'")},
                "is_error": true,
            });
            return ExtFunctionResult::Return(json_to_monty(&result));
        }
    };

    // Check policy
    let action_def = effects
        .available_actions(std::slice::from_ref(&lease))
        .await
        .ok()
        .and_then(|actions| actions.into_iter().find(|a| a.name == name));

    if let Some(ref ad) = action_def {
        match policy.evaluate(ad, &lease, &[]) {
            crate::capability::policy::PolicyDecision::Deny { reason } => {
                let result = serde_json::json!({
                    "output": {"error": format!("Denied: {reason}")},
                    "is_error": true,
                });
                return ExtFunctionResult::Return(json_to_monty(&result));
            }
            crate::capability::policy::PolicyDecision::RequireApproval { .. } => {
                let result = serde_json::json!({
                    "need_approval": true,
                    "action_name": name,
                });
                return ExtFunctionResult::Return(json_to_monty(&result));
            }
            crate::capability::policy::PolicyDecision::Allow => {}
        }
    }

    // Execute
    match effects
        .execute_action(&name, params, &lease, &exec_ctx)
        .await
    {
        Ok(r) => {
            let result = serde_json::json!({
                "action_name": r.action_name,
                "output": r.output,
                "is_error": r.is_error,
                "duration_ms": r.duration.as_millis(),
            });
            ExtFunctionResult::Return(json_to_monty(&result))
        }
        Err(e) => {
            let result = serde_json::json!({
                "output": {"error": e.to_string()},
                "is_error": true,
            });
            ExtFunctionResult::Return(json_to_monty(&result))
        }
    }
}

/// Handle `__check_signals__()`.
fn handle_check_signals(signal_rx: &mut SignalReceiver) -> ExtFunctionResult {
    match signal_rx.try_recv() {
        Ok(ThreadSignal::Stop) | Ok(ThreadSignal::Suspend) => {
            ExtFunctionResult::Return(MontyObject::String("stop".into()))
        }
        Ok(ThreadSignal::InjectMessage(msg)) => {
            let result = serde_json::json!({"inject": msg.content});
            ExtFunctionResult::Return(json_to_monty(&result))
        }
        Ok(ThreadSignal::Resume) | Ok(ThreadSignal::ChildCompleted { .. }) => {
            ExtFunctionResult::Return(MontyObject::None)
        }
        Err(_) => ExtFunctionResult::Return(MontyObject::None),
    }
}

/// Handle `__emit_event__(kind, **data)`.
fn handle_emit_event(
    args: &[MontyObject],
    kwargs: &[(MontyObject, MontyObject)],
    thread: &mut Thread,
    event_tx: Option<&tokio::sync::broadcast::Sender<ThreadEvent>>,
) -> ExtFunctionResult {
    let kind_str = args.first().map(monty_to_string).unwrap_or_default();

    let kind = match kind_str.as_str() {
        "step_started" => {
            let _step = extract_u64_kwarg(kwargs, "step").unwrap_or(0);
            EventKind::StepStarted {
                step_id: StepId::new(),
            }
        }
        "step_completed" => {
            let input = extract_u64_kwarg(kwargs, "input_tokens").unwrap_or(0);
            let output = extract_u64_kwarg(kwargs, "output_tokens").unwrap_or(0);
            EventKind::StepCompleted {
                step_id: StepId::new(),
                tokens: TokenUsage {
                    input_tokens: input,
                    output_tokens: output,
                    ..Default::default()
                },
            }
        }
        "action_executed" => {
            let action_name = extract_string_kwarg(kwargs, "action_name").unwrap_or_default();
            let call_id = extract_string_kwarg(kwargs, "call_id").unwrap_or_default();
            EventKind::ActionExecuted {
                step_id: StepId::new(),
                action_name,
                call_id,
                duration_ms: 0,
            }
        }
        "action_failed" => {
            let action_name = extract_string_kwarg(kwargs, "action_name").unwrap_or_default();
            let call_id = extract_string_kwarg(kwargs, "call_id").unwrap_or_default();
            let error = extract_string_kwarg(kwargs, "error").unwrap_or_default();
            EventKind::ActionFailed {
                step_id: StepId::new(),
                action_name,
                call_id,
                error,
            }
        }
        _ => {
            debug!(kind = %kind_str, "orchestrator: unknown event kind, skipping");
            return ExtFunctionResult::Return(MontyObject::None);
        }
    };

    let event = ThreadEvent::new(thread.id, kind);
    if let Some(tx) = event_tx {
        let _ = tx.send(event.clone());
    }
    thread.events.push(event);
    thread.updated_at = chrono::Utc::now();

    ExtFunctionResult::Return(MontyObject::None)
}

/// Handle `__add_message__(role, content)`.
fn handle_add_message(
    args: &[MontyObject],
    _kwargs: &[(MontyObject, MontyObject)],
    thread: &mut Thread,
) -> ExtFunctionResult {
    let role = args.first().map(monty_to_string).unwrap_or_default();
    let content = args.get(1).map(monty_to_string).unwrap_or_default();

    match role.as_str() {
        "user" => thread.add_message(ThreadMessage::user(&content)),
        "assistant" | "assistant_actions" => {
            thread.add_message(ThreadMessage::assistant(&content))
        }
        "system" => thread.add_message(ThreadMessage::system(&content)),
        "system_append" => {
            // Append to existing system message (for doc injection)
            if let Some(msg) = thread
                .messages
                .iter_mut()
                .find(|m| m.role == crate::types::message::MessageRole::System)
            {
                msg.content.push_str("\n\n");
                msg.content.push_str(&content);
            }
        }
        "action_result" => {
            thread.add_message(ThreadMessage::action_result("", "", &content));
        }
        _ => {
            thread.add_message(ThreadMessage::user(&content));
        }
    }

    thread.step_count += 0; // Message addition tracked by thread itself
    ExtFunctionResult::Return(MontyObject::None)
}

/// Handle `__save_checkpoint__(state, counters)`.
fn handle_save_checkpoint(
    args: &[MontyObject],
    _kwargs: &[(MontyObject, MontyObject)],
    thread: &mut Thread,
) -> ExtFunctionResult {
    let state = args
        .first()
        .map(monty_to_json)
        .unwrap_or(serde_json::json!({}));
    let counters = args
        .get(1)
        .map(monty_to_json)
        .unwrap_or(serde_json::json!({}));

    if let Some(metadata) = thread.metadata.as_object_mut() {
        metadata.insert(
            "runtime_checkpoint".into(),
            serde_json::json!({
                "persisted_state": state,
                "nudge_count": counters.get("nudge_count").and_then(|v| v.as_u64()).unwrap_or(0),
                "consecutive_errors": counters.get("consecutive_errors").and_then(|v| v.as_u64()).unwrap_or(0),
                "compaction_count": counters.get("compaction_count").and_then(|v| v.as_u64()).unwrap_or(0),
            }),
        );
    }
    thread.updated_at = chrono::Utc::now();

    ExtFunctionResult::Return(MontyObject::None)
}

/// Handle `__transition_to__(state, reason)`.
fn handle_transition_to(
    args: &[MontyObject],
    _kwargs: &[(MontyObject, MontyObject)],
    thread: &mut Thread,
) -> ExtFunctionResult {
    let state_str = args.first().map(monty_to_string).unwrap_or_default();
    let reason = args.get(1).map(monty_to_string);

    let target = match state_str.as_str() {
        "running" => crate::types::thread::ThreadState::Running,
        "completed" => crate::types::thread::ThreadState::Completed,
        "failed" => crate::types::thread::ThreadState::Failed,
        "waiting" => crate::types::thread::ThreadState::Waiting,
        "suspended" => crate::types::thread::ThreadState::Suspended,
        other => {
            return ExtFunctionResult::Error(monty::MontyException::new(
                monty::ExcType::ValueError,
                Some(format!("Unknown thread state: {other}")),
            ))
        }
    };

    match thread.transition_to(target, reason) {
        Ok(()) => ExtFunctionResult::Return(MontyObject::None),
        Err(e) => ExtFunctionResult::Error(monty::MontyException::new(
            monty::ExcType::RuntimeError,
            Some(format!("State transition failed: {e}")),
        )),
    }
}

/// Handle `__retrieve_docs__(goal, max_docs)`.
async fn handle_retrieve_docs(
    args: &[MontyObject],
    _kwargs: &[(MontyObject, MontyObject)],
    thread: &Thread,
    retrieval: Option<&RetrievalEngine>,
) -> ExtFunctionResult {
    let retrieval = match retrieval {
        Some(r) => r,
        None => return ExtFunctionResult::Return(json_to_monty(&serde_json::json!([]))),
    };

    let goal = args.first().map(monty_to_string).unwrap_or_default();
    let max_docs = args
        .get(1)
        .and_then(|v| match v {
            MontyObject::Int(i) => Some(*i as usize),
            _ => None,
        })
        .unwrap_or(5);

    match retrieval
        .retrieve_context(thread.project_id, &goal, max_docs)
        .await
    {
        Ok(docs) => {
            let docs_json: Vec<serde_json::Value> = docs
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "type": format!("{:?}", d.doc_type),
                        "title": d.title,
                        "content": d.content,
                    })
                })
                .collect();
            ExtFunctionResult::Return(json_to_monty(&serde_json::json!(docs_json)))
        }
        Err(e) => {
            warn!("retrieve_docs failed: {e}");
            ExtFunctionResult::Return(json_to_monty(&serde_json::json!([])))
        }
    }
}

/// Handle `__check_budget__()`.
fn handle_check_budget(thread: &Thread) -> ExtFunctionResult {
    let tokens_remaining = thread
        .config
        .max_tokens_total
        .map(|max| max.saturating_sub(thread.total_tokens_used))
        .unwrap_or(u64::MAX);

    let time_remaining_ms = thread
        .config
        .max_duration
        .map(|dur| {
            let elapsed = chrono::Utc::now()
                .signed_duration_since(thread.created_at)
                .num_milliseconds()
                .max(0) as u64;
            dur.as_millis() as u64 - elapsed.min(dur.as_millis() as u64)
        })
        .unwrap_or(u64::MAX);

    let usd_remaining = thread
        .config
        .max_budget_usd
        .map(|max| (max - thread.total_cost_usd).max(0.0));

    let result = serde_json::json!({
        "tokens_remaining": tokens_remaining,
        "time_remaining_ms": time_remaining_ms,
        "usd_remaining": usd_remaining,
    });

    ExtFunctionResult::Return(json_to_monty(&result))
}

/// Handle `__get_actions__()`.
async fn handle_get_actions(
    thread: &Thread,
    effects: &Arc<dyn EffectExecutor>,
    leases: &Arc<LeaseManager>,
) -> ExtFunctionResult {
    let active_leases = leases.active_for_thread(thread.id).await;
    match effects.available_actions(&active_leases).await {
        Ok(actions) => {
            let actions_json: Vec<serde_json::Value> = actions
                .iter()
                .map(|a| {
                    serde_json::json!({
                        "name": a.name,
                        "description": a.description,
                        "params": a.parameters_schema,
                    })
                })
                .collect();
            ExtFunctionResult::Return(json_to_monty(&serde_json::json!(actions_json)))
        }
        Err(e) => {
            warn!("get_actions failed: {e}");
            ExtFunctionResult::Return(json_to_monty(&serde_json::json!([])))
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────

/// Build the context variables injected into the orchestrator Python.
fn build_orchestrator_inputs(
    thread: &Thread,
    persisted_state: &serde_json::Value,
) -> (Vec<String>, Vec<MontyObject>) {
    let names = vec![
        "context".into(),
        "goal".into(),
        "actions".into(),
        "state".into(),
        "config".into(),
    ];

    // Build context (message history)
    let context: Vec<serde_json::Value> = thread
        .messages
        .iter()
        .map(|m| {
            serde_json::json!({
                "role": format!("{:?}", m.role),
                "content": m.content,
            })
        })
        .collect();

    // Build config
    let config = serde_json::json!({
        "max_iterations": thread.config.max_iterations,
        "max_tool_intent_nudges": thread.config.max_tool_intent_nudges,
        "enable_tool_intent_nudge": thread.config.enable_tool_intent_nudge,
        "max_consecutive_errors": thread.config.max_consecutive_errors,
        "max_tokens_total": thread.config.max_tokens_total,
        "max_budget_usd": thread.config.max_budget_usd,
        "model_context_limit": thread.config.model_context_limit,
        "enable_compaction": thread.config.enable_compaction,
        "depth": thread.config.depth,
        "max_depth": thread.config.max_depth,
        "step_count": thread.step_count,
    });

    let values = vec![
        json_to_monty(&serde_json::json!(context)),
        MontyObject::String(thread.goal.clone()),
        json_to_monty(&serde_json::json!([])), // actions loaded dynamically via __get_actions__
        json_to_monty(persisted_state),
        json_to_monty(&config),
    ];

    (names, values)
}

/// Parse the orchestrator's return value into a ThreadOutcome.
fn parse_outcome(result: &serde_json::Value) -> ThreadOutcome {
    let outcome = result
        .get("outcome")
        .and_then(|v| v.as_str())
        .unwrap_or("completed");

    match outcome {
        "completed" => ThreadOutcome::Completed {
            response: result.get("response").and_then(|v| v.as_str()).map(String::from),
        },
        "stopped" => ThreadOutcome::Stopped,
        "max_iterations" => ThreadOutcome::MaxIterations,
        "failed" => ThreadOutcome::Failed {
            error: result
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string(),
        },
        "need_approval" => ThreadOutcome::NeedApproval {
            action_name: result
                .get("action_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            call_id: result
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            parameters: result
                .get("parameters")
                .cloned()
                .unwrap_or(serde_json::json!({})),
        },
        _ => ThreadOutcome::Completed { response: None },
    }
}

fn extract_string_arg(
    args: &[MontyObject],
    kwargs: &[(MontyObject, MontyObject)],
    name: &str,
    position: usize,
) -> Option<String> {
    for (k, v) in kwargs {
        if let MontyObject::String(key) = k
            && key == name
        {
            return Some(monty_to_string(v));
        }
    }
    args.get(position).map(monty_to_string)
}

fn extract_string_kwarg(kwargs: &[(MontyObject, MontyObject)], name: &str) -> Option<String> {
    for (k, v) in kwargs {
        if let MontyObject::String(key) = k
            && key == name
        {
            return Some(monty_to_string(v));
        }
    }
    None
}

fn extract_u64_kwarg(kwargs: &[(MontyObject, MontyObject)], name: &str) -> Option<u64> {
    for (k, v) in kwargs {
        if let MontyObject::String(key) = k
            && key == name
            && let MontyObject::Int(i) = v
        {
            return Some(*i as u64);
        }
    }
    None
}
