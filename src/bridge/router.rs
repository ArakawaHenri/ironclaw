//! Engine v2 router — handles user messages via the engine when enabled.

use std::sync::{Arc, OnceLock};

use tokio::sync::RwLock;
use tracing::debug;

use ironclaw_engine::{
    Capability, CapabilityRegistry, ConversationManager, LeaseManager, PolicyEngine, Project,
    Store, ThreadConfig, ThreadManager, ThreadOutcome,
};

use ironclaw_common::AppEvent;

use crate::agent::Agent;
use crate::bridge::effect_adapter::EffectBridgeAdapter;
use crate::bridge::llm_adapter::LlmBridgeAdapter;
use crate::bridge::store_adapter::HybridStore;
use crate::channels::web::sse::SseManager;
use crate::channels::{IncomingMessage, StatusUpdate};
use crate::db::Database;
use crate::error::Error;

/// Check if the engine v2 is enabled via `ENGINE_V2=true` environment variable.
pub fn is_engine_v2_enabled() -> bool {
    std::env::var("ENGINE_V2")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
}

/// Pending approval info stored between the NeedApproval outcome and the user's response.
struct PendingApproval {
    action_name: String,
    /// The user message that triggered this (for re-submission after approval).
    original_content: String,
}

/// Persistent engine state that lives across messages.
struct EngineState {
    thread_manager: Arc<ThreadManager>,
    conversation_manager: ConversationManager,
    effect_adapter: Arc<EffectBridgeAdapter>,
    #[allow(dead_code)]
    store: Arc<HybridStore>,
    default_project_id: ironclaw_engine::ProjectId,
    /// Currently pending approval (if any).
    pending_approval: RwLock<Option<PendingApproval>>,
    /// SSE manager for broadcasting AppEvents to the web gateway.
    sse: Option<Arc<SseManager>>,
    /// V1 database for writing conversation messages (gateway reads from here).
    db: Option<Arc<dyn Database>>,
}

/// Global engine state, initialized on first use.
static ENGINE_STATE: OnceLock<RwLock<Option<EngineState>>> = OnceLock::new();

/// Get or initialize the engine state using the agent's dependencies.
async fn get_or_init_engine(agent: &Agent) -> Result<(), Error> {
    let lock = ENGINE_STATE.get_or_init(|| RwLock::new(None));
    let guard = lock.read().await;
    if guard.is_some() {
        return Ok(());
    }
    drop(guard);

    // Initialize
    let mut guard = lock.write().await;
    if guard.is_some() {
        return Ok(()); // double-check after acquiring write lock
    }

    debug!("engine v2: initializing engine state");

    let llm_adapter = Arc::new(LlmBridgeAdapter::new(
        agent.llm().clone(),
        Some(agent.cheap_llm().clone()),
    ));

    let effect_adapter = Arc::new(EffectBridgeAdapter::new(
        agent.tools().clone(),
        agent.safety().clone(),
        agent.hooks().clone(),
    ));

    let store = Arc::new(HybridStore::new(agent.workspace().cloned()));

    // Load existing reflection docs from workspace (lessons from prior sessions)
    store.load_docs_from_workspace().await;

    // Build capability registry from available tools
    let mut capabilities = CapabilityRegistry::new();
    let tool_defs = agent.tools().tool_definitions().await;
    if !tool_defs.is_empty() {
        capabilities.register(Capability {
            name: "tools".into(),
            description: "Available tools".into(),
            actions: tool_defs
                .into_iter()
                .map(|td| ironclaw_engine::ActionDef {
                    name: td.name.replace('-', "_"),
                    description: td.description,
                    parameters_schema: td.parameters,
                    effects: vec![],
                    requires_approval: false,
                })
                .collect(),
            knowledge: vec![],
            policies: vec![],
        });
    }

    let leases = Arc::new(LeaseManager::new());
    let policy = Arc::new(PolicyEngine::new());

    let thread_manager = Arc::new(ThreadManager::new(
        llm_adapter,
        effect_adapter.clone(),
        store.clone(),
        Arc::new(capabilities),
        leases,
        policy,
    ));

    // Create a default project
    let project = Project::new("default", "Default project for engine v2");
    let project_id = project.id;
    store.save_project(&project).await.map_err(|e| {
        crate::error::Error::from(crate::error::JobError::ContextError {
            id: uuid::Uuid::nil(),
            reason: format!("engine v2 store error: {e}"),
        })
    })?;

    let conversation_manager = ConversationManager::new(Arc::clone(&thread_manager));

    *guard = Some(EngineState {
        thread_manager,
        conversation_manager,
        effect_adapter,
        store: store.clone(),
        default_project_id: project_id,
        pending_approval: RwLock::new(None),
        sse: agent.deps.sse_tx.clone(),
        db: agent.deps.store.clone(),
    });

    Ok(())
}

/// Handle an approval response (yes/no/always) for engine v2.
///
/// Called from `handle_message` when the user responds to an approval request.
pub async fn handle_approval(
    agent: &Agent,
    message: &IncomingMessage,
    approved: bool,
    always: bool,
) -> Result<Option<String>, Error> {
    get_or_init_engine(agent).await?;

    let lock = ENGINE_STATE.get().expect("engine initialized");
    let guard = lock.read().await;
    let state = guard.as_ref().expect("engine initialized");

    // Take the pending approval
    let pending = state.pending_approval.write().await.take();
    let pending = match pending {
        Some(p) => p,
        None => {
            debug!("engine v2: no pending approval, ignoring");
            return Ok(Some("No pending approval.".into()));
        }
    };

    if !approved {
        let _ = agent
            .channels
            .send_status(
                &message.channel,
                StatusUpdate::Status("Tool call denied.".into()),
                &message.metadata,
            )
            .await;
        return Ok(Some(format!(
            "Denied: tool '{}' was not executed.",
            pending.action_name
        )));
    }

    // Approved — add to auto-approved set
    debug!(
        tool = %pending.action_name,
        always,
        "engine v2: tool approved"
    );

    // Convert Python name back to registry name for auto-approve
    let registry_name = pending.action_name.replace('_', "-");
    state
        .effect_adapter
        .auto_approve_tool(&pending.action_name)
        .await;
    state.effect_adapter.auto_approve_tool(&registry_name).await;

    if always {
        debug!(tool = %pending.action_name, "engine v2: tool auto-approved for session");
    }

    // Re-process the original message — the tool will now pass approval
    let _ = agent
        .channels
        .send_status(
            &message.channel,
            StatusUpdate::Thinking("Re-executing with approval...".into()),
            &message.metadata,
        )
        .await;

    handle_with_engine(agent, message, &pending.original_content).await
}

/// Handle a user message through the engine v2 pipeline.
pub async fn handle_with_engine(
    agent: &Agent,
    message: &IncomingMessage,
    content: &str,
) -> Result<Option<String>, Error> {
    // Ensure engine is initialized
    get_or_init_engine(agent).await?;

    let lock = ENGINE_STATE.get().expect("engine initialized");
    let guard = lock.read().await;
    let state = guard.as_ref().expect("engine initialized");

    debug!(
        user_id = %message.user_id,
        channel = %message.channel,
        "engine v2: handling message"
    );

    // Send "Thinking..." status to the channel
    let _ = agent
        .channels
        .send_status(
            &message.channel,
            StatusUpdate::Thinking("Processing...".into()),
            &message.metadata,
        )
        .await;

    // Get or create conversation for this channel+user
    let conv_id = state
        .conversation_manager
        .get_or_create_conversation(&message.channel, &message.user_id)
        .await;

    // Handle the message — spawns a new thread or injects into active one
    let thread_id = state
        .conversation_manager
        .handle_user_message(
            conv_id,
            content,
            state.default_project_id,
            &message.user_id,
            ThreadConfig {
                enable_reflection: true,
                ..ThreadConfig::default()
            },
        )
        .await
        .map_err(|e| {
            crate::error::Error::from(crate::error::JobError::ContextError {
                id: uuid::Uuid::nil(),
                reason: format!("engine v2 error: {e}"),
            })
        })?;

    debug!(thread_id = %thread_id, "engine v2: thread spawned");

    // Subscribe to live events for progress updates
    let mut event_rx = state.thread_manager.subscribe_events();
    let channels = &agent.channels;
    let channel_name = &message.channel;
    let metadata = &message.metadata;
    let sse = state.sse.as_ref();
    let tid_str = thread_id.to_string();

    // Forward events to both the channel (REPL) and SSE (web gateway)
    loop {
        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Ok(ref evt) if evt.thread_id == thread_id => {
                        forward_event_to_channel(evt, channels, channel_name, metadata).await;
                        if let Some(sse) = sse
                            && let Some(app_event) = thread_event_to_app_event(evt, &tid_str)
                        {
                            sse.broadcast(app_event);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    _ => {}
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                if !state.thread_manager.is_running(thread_id).await {
                    break;
                }
            }
        }
    }

    // Join the thread to get the outcome
    let outcome = state
        .thread_manager
        .join_thread(thread_id)
        .await
        .map_err(|e| {
            crate::error::Error::from(crate::error::JobError::ContextError {
                id: uuid::Uuid::nil(),
                reason: format!("engine v2 join error: {e}"),
            })
        })?;

    // Record outcome in conversation
    state
        .conversation_manager
        .record_thread_outcome(conv_id, thread_id, &outcome)
        .await;

    // Note: trace recording, retrospective analysis, and LLM reflection
    // all run automatically inside ThreadManager after the thread completes.

    // Persist to v1 conversation DB so web gateway can display messages
    if let Some(ref db) = state.db {
        // get_or_create_assistant_conversation gives us a per-user, per-channel conversation
        if let Ok(conv_id_v1) = db
            .get_or_create_assistant_conversation(&message.user_id, &message.channel)
            .await
        {
            // Write user message
            let _ = db
                .add_conversation_message(conv_id_v1, "user", content)
                .await;

            // Write agent response
            if let ThreadOutcome::Completed {
                response: Some(ref text),
            } = outcome
            {
                let _ = db
                    .add_conversation_message(conv_id_v1, "assistant", text)
                    .await;
            }
        }
    }

    // Broadcast final response as AppEvent for web gateway SSE
    if let Some(ref sse) = state.sse
        && let ThreadOutcome::Completed {
            response: Some(ref text),
        } = outcome
    {
        sse.broadcast(AppEvent::Response {
            content: text.clone(),
            thread_id: thread_id.to_string(),
        });
    }

    // Convert outcome to response
    match outcome {
        ThreadOutcome::Completed { response } => {
            debug!(thread_id = %thread_id, "engine v2: completed");
            Ok(response)
        }
        ThreadOutcome::Stopped => Ok(Some("Thread was stopped.".into())),
        ThreadOutcome::MaxIterations => Ok(Some(
            "Reached maximum iterations without completing.".into(),
        )),
        ThreadOutcome::Failed { error } => Ok(Some(format!("Error: {error}"))),
        ThreadOutcome::NeedApproval {
            action_name,
            call_id: _,
            parameters,
        } => {
            // Store pending approval for when the user responds
            *state.pending_approval.write().await = Some(PendingApproval {
                action_name: action_name.clone(),
                original_content: content.to_string(),
            });

            // Send approval request to channel (matches v1 ApprovalNeeded format)
            let _ = agent
                .channels
                .send_status(
                    &message.channel,
                    StatusUpdate::ApprovalNeeded {
                        request_id: uuid::Uuid::new_v4().to_string(),
                        tool_name: action_name.clone(),
                        description: format!(
                            "Tool '{}' requires approval to execute.",
                            action_name
                        ),
                        parameters,
                        allow_always: true,
                    },
                    &message.metadata,
                )
                .await;

            Ok(Some(format!(
                "Tool '{}' requires approval. Reply 'yes' to approve, 'always' to auto-approve, or 'no' to deny.",
                action_name
            )))
        }
    }
}

/// Forward an engine ThreadEvent to the channel as a StatusUpdate.
async fn forward_event_to_channel(
    event: &ironclaw_engine::ThreadEvent,
    channels: &std::sync::Arc<crate::channels::ChannelManager>,
    channel_name: &str,
    metadata: &serde_json::Value,
) {
    use ironclaw_engine::EventKind;

    match &event.kind {
        EventKind::StepStarted { .. } => {
            let _ = channels
                .send_status(
                    channel_name,
                    StatusUpdate::Thinking("Thinking...".into()),
                    metadata,
                )
                .await;
        }
        EventKind::ActionExecuted { action_name, .. } => {
            let _ = channels
                .send_status(
                    channel_name,
                    StatusUpdate::ToolCompleted {
                        name: action_name.clone(),
                        success: true,
                        error: None,
                        parameters: None,
                    },
                    metadata,
                )
                .await;
        }
        EventKind::ActionFailed {
            action_name, error, ..
        } => {
            let _ = channels
                .send_status(
                    channel_name,
                    StatusUpdate::ToolCompleted {
                        name: action_name.clone(),
                        success: false,
                        error: Some(error.clone()),
                        parameters: None,
                    },
                    metadata,
                )
                .await;
        }
        EventKind::StepCompleted { .. } => {
            let _ = channels
                .send_status(
                    channel_name,
                    StatusUpdate::Thinking("Processing results...".into()),
                    metadata,
                )
                .await;
        }
        _ => {}
    }
}

/// Convert a ThreadEvent to an AppEvent for the web gateway SSE stream.
fn thread_event_to_app_event(
    event: &ironclaw_engine::ThreadEvent,
    thread_id: &str,
) -> Option<AppEvent> {
    use ironclaw_engine::EventKind;

    match &event.kind {
        EventKind::StepStarted { .. } => Some(AppEvent::Thinking {
            message: "Thinking...".into(),
            thread_id: Some(thread_id.into()),
        }),
        EventKind::ActionExecuted { action_name, .. } => Some(AppEvent::ToolCompleted {
            name: action_name.clone(),
            success: true,
            error: None,
            parameters: None,
            thread_id: Some(thread_id.into()),
        }),
        EventKind::ActionFailed {
            action_name, error, ..
        } => Some(AppEvent::ToolCompleted {
            name: action_name.clone(),
            success: false,
            error: Some(error.clone()),
            parameters: None,
            thread_id: Some(thread_id.into()),
        }),
        EventKind::StepCompleted { .. } => Some(AppEvent::Status {
            message: "Processing results...".into(),
            thread_id: Some(thread_id.into()),
        }),
        _ => None,
    }
}
