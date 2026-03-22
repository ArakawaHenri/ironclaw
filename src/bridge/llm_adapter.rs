//! LLM bridge adapter — wraps `LlmProvider` as `ironclaw_engine::LlmBackend`.

use std::sync::Arc;

use ironclaw_engine::{
    ActionDef, EngineError, LlmBackend, LlmCallConfig, LlmOutput, LlmResponse, ThreadMessage,
    TokenUsage,
};

use crate::llm::{
    ChatMessage, LlmProvider, Role, ToolCall, ToolCompletionRequest, ToolDefinition,
};

/// Wraps an existing `LlmProvider` to implement the engine's `LlmBackend` trait.
pub struct LlmBridgeAdapter {
    provider: Arc<dyn LlmProvider>,
    /// Optional cheaper provider for sub-calls (depth > 0).
    cheap_provider: Option<Arc<dyn LlmProvider>>,
}

impl LlmBridgeAdapter {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        cheap_provider: Option<Arc<dyn LlmProvider>>,
    ) -> Self {
        Self {
            provider,
            cheap_provider,
        }
    }

    fn provider_for_depth(&self, depth: u32) -> &Arc<dyn LlmProvider> {
        if depth > 0 {
            self.cheap_provider.as_ref().unwrap_or(&self.provider)
        } else {
            &self.provider
        }
    }
}

#[async_trait::async_trait]
impl LlmBackend for LlmBridgeAdapter {
    async fn complete(
        &self,
        messages: &[ThreadMessage],
        actions: &[ActionDef],
        config: &LlmCallConfig,
    ) -> Result<LlmOutput, EngineError> {
        let provider = self.provider_for_depth(config.depth);

        // Convert messages
        let chat_messages: Vec<ChatMessage> = messages.iter().map(thread_msg_to_chat).collect();

        // Convert actions to tool definitions
        let tools: Vec<ToolDefinition> = if config.force_text {
            vec![] // No tools when forcing text
        } else {
            actions.iter().map(action_def_to_tool_def).collect()
        };

        // Build request — match the existing Reasoning.respond_with_tools() defaults
        let max_tokens = config.max_tokens.unwrap_or(4096);
        let temperature = config.temperature.unwrap_or(0.7);

        if tools.is_empty() {
            // No tools: use plain completion (matches existing no-tools path)
            let mut request = crate::llm::CompletionRequest::new(chat_messages)
                .with_max_tokens(max_tokens)
                .with_temperature(temperature);
            request.metadata = config.metadata.clone();

            let response = provider
                .complete(request)
                .await
                .map_err(|e| EngineError::Llm {
                    reason: e.to_string(),
                })?;

            return Ok(LlmOutput {
                response: LlmResponse::Text(response.content),
                usage: TokenUsage {
                    input_tokens: u64::from(response.input_tokens),
                    output_tokens: u64::from(response.output_tokens),
                    cache_read_tokens: u64::from(response.cache_read_input_tokens),
                    cache_write_tokens: u64::from(response.cache_creation_input_tokens),
                },
            });
        }

        // With tools: use tool completion (matches existing tools path)
        let mut request = ToolCompletionRequest::new(chat_messages, tools)
            .with_max_tokens(max_tokens)
            .with_temperature(temperature)
            .with_tool_choice("auto");
        request.metadata = config.metadata.clone();

        // Call provider
        let response = provider
            .complete_with_tools(request)
            .await
            .map_err(|e| EngineError::Llm {
                reason: e.to_string(),
            })?;

        // Convert response — check for code blocks (CodeAct/RLM pattern)
        let llm_response = if !response.tool_calls.is_empty() {
            LlmResponse::ActionCalls {
                calls: response
                    .tool_calls
                    .iter()
                    .map(|tc| ironclaw_engine::ActionCall {
                        id: tc.id.clone(),
                        action_name: tc.name.clone(),
                        parameters: tc.arguments.clone(),
                    })
                    .collect(),
                content: response.content.clone(),
            }
        } else {
            let text = response.content.unwrap_or_default();
            // Detect ```repl or ```python fenced code blocks
            match extract_code_block(&text) {
                Some(code) => LlmResponse::Code {
                    code,
                    content: Some(text),
                },
                None => LlmResponse::Text(text),
            }
        };

        Ok(LlmOutput {
            response: llm_response,
            usage: TokenUsage {
                input_tokens: u64::from(response.input_tokens),
                output_tokens: u64::from(response.output_tokens),
                cache_read_tokens: u64::from(response.cache_read_input_tokens),
                cache_write_tokens: u64::from(response.cache_creation_input_tokens),
            },
        })
    }

    fn model_name(&self) -> &str {
        self.provider.model_name()
    }
}

// ── Conversion helpers ──────────────────────────────────────

fn thread_msg_to_chat(msg: &ThreadMessage) -> ChatMessage {
    use ironclaw_engine::MessageRole;

    let role = match msg.role {
        MessageRole::System => Role::System,
        MessageRole::User => Role::User,
        MessageRole::Assistant => Role::Assistant,
        MessageRole::ActionResult => Role::Tool,
    };

    let mut chat = ChatMessage {
        role,
        content: msg.content.clone(),
        content_parts: Vec::new(),
        tool_call_id: msg.action_call_id.clone(),
        name: msg.action_name.clone(),
        tool_calls: None,
    };

    // Convert action calls if present (assistant message with tool calls)
    if let Some(ref calls) = msg.action_calls {
        chat.tool_calls = Some(
            calls
                .iter()
                .map(|c| ToolCall {
                    id: c.id.clone(),
                    name: c.action_name.clone(),
                    arguments: c.parameters.clone(),
                })
                .collect(),
        );
    }

    chat
}

fn action_def_to_tool_def(action: &ActionDef) -> ToolDefinition {
    ToolDefinition {
        name: action.name.clone(),
        description: action.description.clone(),
        parameters: action.parameters_schema.clone(),
    }
}

/// Extract Python code from ```repl or ```python fenced blocks.
///
/// Matches the pattern used by RLM implementations (fast-rlm uses ```repl,
/// official RLM uses ```repl, some models output ```python).
fn extract_code_block(text: &str) -> Option<String> {
    // Try ```repl first (preferred), then ```python
    for marker in ["```repl", "```python"] {
        if let Some(start) = text.find(marker) {
            let code_start = start + marker.len();
            // Skip to next line
            let code_start = text[code_start..]
                .find('\n')
                .map(|i| code_start + i + 1)
                .unwrap_or(code_start);
            // Find closing ```
            if let Some(end) = text[code_start..].find("```") {
                let code = text[code_start..code_start + end].trim();
                if !code.is_empty() {
                    return Some(code.to_string());
                }
            }
        }
    }
    None
}
