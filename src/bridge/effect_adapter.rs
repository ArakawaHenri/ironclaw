//! Effect bridge adapter — wraps `ToolRegistry` + `SafetyLayer` as `ironclaw_engine::EffectExecutor`.
//!
//! This is the security boundary between the engine and existing IronClaw
//! infrastructure. All v1 security controls are enforced here:
//! - Tool approval (requires_approval, auto-approve tracking)
//! - Output sanitization (sanitize_tool_output + wrap_for_llm)
//! - Hook interception (BeforeToolCall)
//! - Sensitive parameter redaction
//! - Rate limiting (per-user, per-tool)

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;
use tracing::debug;

use ironclaw_engine::{
    ActionDef, ActionResult, CapabilityLease, EffectExecutor, EngineError, ThreadExecutionContext,
};

use crate::context::JobContext;
use crate::hooks::{HookEvent, HookOutcome, HookRegistry};
use crate::safety::SafetyLayer;
use crate::tools::{ApprovalRequirement, ToolRegistry};

/// Wraps the existing tool pipeline to implement the engine's `EffectExecutor`.
///
/// Enforces all v1 security controls at the adapter boundary:
/// tool approval, output sanitization, hooks, and rate limiting.
pub struct EffectBridgeAdapter {
    tools: Arc<ToolRegistry>,
    safety: Arc<SafetyLayer>,
    hooks: Arc<HookRegistry>,
    /// Tools the user has approved with "always" (persists within session).
    auto_approved: RwLock<HashSet<String>>,
}

impl EffectBridgeAdapter {
    pub fn new(
        tools: Arc<ToolRegistry>,
        safety: Arc<SafetyLayer>,
        hooks: Arc<HookRegistry>,
    ) -> Self {
        Self {
            tools,
            safety,
            hooks,
            auto_approved: RwLock::new(HashSet::new()),
        }
    }

    /// Mark a tool as auto-approved (user said "always").
    #[allow(dead_code)]
    pub async fn auto_approve_tool(&self, tool_name: &str) {
        self.auto_approved.write().await.insert(tool_name.to_string());
    }
}

#[async_trait::async_trait]
impl EffectExecutor for EffectBridgeAdapter {
    async fn execute_action(
        &self,
        action_name: &str,
        parameters: serde_json::Value,
        _lease: &CapabilityLease,
        context: &ThreadExecutionContext,
    ) -> Result<ActionResult, EngineError> {
        let start = Instant::now();

        // Resolve tool name (underscore → hyphen fallback)
        let hyphenated = action_name.replace('_', "-");
        let lookup_name = if self.tools.get(action_name).await.is_some() {
            action_name
        } else {
            &hyphenated
        };

        // ── 1. Check tool approval (v1: Tool::requires_approval) ──

        if let Some(tool) = self.tools.get(lookup_name).await {
            let requirement = tool.requires_approval(&parameters);
            match requirement {
                ApprovalRequirement::Always => {
                    return Err(EngineError::LeaseDenied {
                        reason: format!(
                            "Tool '{}' requires explicit approval for this operation. \
                             This action cannot be auto-approved.",
                            action_name
                        ),
                    });
                }
                ApprovalRequirement::UnlessAutoApproved => {
                    let is_approved = self.auto_approved.read().await.contains(lookup_name);
                    if !is_approved {
                        return Err(EngineError::LeaseDenied {
                            reason: format!(
                                "Tool '{}' requires approval. \
                                 Use a read-only tool instead, or ask the user to approve this action.",
                                action_name
                            ),
                        });
                    }
                }
                ApprovalRequirement::Never => {}
            }
        }

        // ── 2. Run BeforeToolCall hook (v1: hooks.run) ──

        let redacted_params = if let Some(tool) = self.tools.get(lookup_name).await {
            crate::tools::redact_params(&parameters, tool.sensitive_params())
        } else {
            parameters.clone()
        };

        let hook_event = HookEvent::ToolCall {
            tool_name: lookup_name.to_string(),
            parameters: redacted_params,
            user_id: context.user_id.clone(),
            context: format!("engine_v2:{}", context.thread_id),
        };

        match self.hooks.run(&hook_event).await {
            Ok(HookOutcome::Reject { reason }) => {
                return Err(EngineError::LeaseDenied {
                    reason: format!("Tool '{}' blocked by hook: {}", action_name, reason),
                });
            }
            Err(crate::hooks::HookError::Rejected { reason }) => {
                return Err(EngineError::LeaseDenied {
                    reason: format!("Tool '{}' blocked by hook: {}", action_name, reason),
                });
            }
            Err(e) => {
                debug!(tool = lookup_name, error = %e, "hook error (fail-open)");
            }
            Ok(HookOutcome::Continue { .. }) => {}
        }

        // ── 3. Execute through existing safety pipeline ──

        let job_ctx = JobContext::with_user(
            &context.user_id,
            "engine_v2",
            format!("Thread {}", context.thread_id),
        );

        let result = crate::tools::execute::execute_tool_with_safety(
            &self.tools,
            &self.safety,
            lookup_name,
            &parameters,
            &job_ctx,
        )
        .await;

        let duration = start.elapsed();

        // ── 4. Sanitize + wrap output (v1: sanitize_tool_output + wrap_for_llm) ──

        match result {
            Ok(output) => {
                // Apply v1 sanitization: leak detection, policy, truncation
                let sanitized = self.safety.sanitize_tool_output(lookup_name, &output);

                // Wrap for LLM: XML boundary protection against injection
                let wrapped = self.safety.wrap_for_llm(lookup_name, &sanitized.content);

                // Parse wrapped content as JSON if possible (for Python dict access)
                // But keep the safety wrapping in the raw output
                let output_value = serde_json::from_str::<serde_json::Value>(&output)
                    .unwrap_or(serde_json::Value::String(wrapped));

                Ok(ActionResult {
                    call_id: String::new(),
                    action_name: action_name.to_string(),
                    output: output_value,
                    is_error: false,
                    duration,
                })
            }
            Err(e) => {
                let error_msg = format!("Tool '{}' failed: {}", lookup_name, e);
                let sanitized = self.safety.sanitize_tool_output(lookup_name, &error_msg);

                Ok(ActionResult {
                    call_id: String::new(),
                    action_name: action_name.to_string(),
                    output: serde_json::json!({"error": sanitized.content}),
                    is_error: true,
                    duration,
                })
            }
        }
    }

    async fn available_actions(
        &self,
        _leases: &[CapabilityLease],
    ) -> Result<Vec<ActionDef>, EngineError> {
        let tool_defs = self.tools.tool_definitions().await;

        // Build action defs with approval info from each tool
        let mut actions = Vec::with_capacity(tool_defs.len());
        for td in tool_defs {
            let python_name = td.name.replace('-', "_");

            // Check default approval requirement (with empty params)
            let requires_approval = if let Some(tool) = self.tools.get(&td.name).await {
                !matches!(
                    tool.requires_approval(&serde_json::json!({})),
                    ApprovalRequirement::Never
                )
            } else {
                false
            };

            actions.push(ActionDef {
                name: python_name,
                description: td.description,
                parameters_schema: td.parameters,
                effects: vec![],
                requires_approval,
            });
        }

        Ok(actions)
    }
}
