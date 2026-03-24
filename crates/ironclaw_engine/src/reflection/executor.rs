//! Effect executor for reflection threads.
//!
//! Provides read-only tools that let the reflection CodeAct thread
//! introspect the completed thread, query existing knowledge, and
//! verify tool names against the capability registry.

use std::sync::Arc;

use crate::capability::registry::CapabilityRegistry;
use crate::memory::RetrievalEngine;
use crate::traits::effect::{EffectExecutor, ThreadExecutionContext};
use crate::traits::store::Store;
use crate::types::capability::{ActionDef, CapabilityLease, EffectType};
use crate::types::error::EngineError;
use crate::types::project::ProjectId;
use crate::types::step::ActionResult;

/// EffectExecutor that provides reflection-specific read-only tools.
pub struct ReflectionExecutor {
    store: Arc<dyn Store>,
    capabilities: Arc<CapabilityRegistry>,
    transcript: String,
    project_id: ProjectId,
}

impl ReflectionExecutor {
    pub fn new(
        store: Arc<dyn Store>,
        capabilities: Arc<CapabilityRegistry>,
        transcript: String,
        project_id: ProjectId,
    ) -> Self {
        Self {
            store,
            capabilities,
            transcript,
            project_id,
        }
    }

    fn action_defs() -> Vec<ActionDef> {
        vec![
            ActionDef {
                name: "get_transcript".into(),
                description: "Get the full execution transcript of the completed thread, \
                              including messages, tool calls, errors, and outcomes."
                    .into(),
                parameters_schema: serde_json::json!({"type": "object", "properties": {}}),
                effects: vec![EffectType::ReadLocal],
                requires_approval: false,
            },
            ActionDef {
                name: "query_memory".into(),
                description: "Search existing memory docs in this project for prior knowledge. \
                              Use to check if a lesson or issue has already been recorded."
                    .into(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search query"},
                        "max_docs": {"type": "integer", "description": "Max results (default 5)"}
                    },
                    "required": ["query"]
                }),
                effects: vec![EffectType::ReadLocal],
                requires_approval: false,
            },
            ActionDef {
                name: "check_tool_exists".into(),
                description: "Check if a tool/action exists in the capability registry. \
                              Returns whether it exists and lists similar tool names if not found."
                    .into(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Tool name to check"}
                    },
                    "required": ["name"]
                }),
                effects: vec![EffectType::ReadLocal],
                requires_approval: false,
            },
            ActionDef {
                name: "list_tools".into(),
                description: "List all available tools/actions in the capability registry.".into(),
                parameters_schema: serde_json::json!({"type": "object", "properties": {}}),
                effects: vec![EffectType::ReadLocal],
                requires_approval: false,
            },
        ]
    }
}

#[async_trait::async_trait]
impl EffectExecutor for ReflectionExecutor {
    async fn execute_action(
        &self,
        action_name: &str,
        parameters: serde_json::Value,
        _lease: &CapabilityLease,
        _context: &ThreadExecutionContext,
    ) -> Result<ActionResult, EngineError> {
        let start = std::time::Instant::now();
        let output = match action_name {
            "get_transcript" => serde_json::json!({ "transcript": self.transcript }),

            "query_memory" => {
                let query = parameters["query"].as_str().unwrap_or("");
                let max_docs = parameters["max_docs"].as_u64().unwrap_or(5) as usize;
                let retrieval = RetrievalEngine::new(Arc::clone(&self.store));
                let docs = retrieval
                    .retrieve_context(self.project_id, query, max_docs)
                    .await?;
                let results: Vec<serde_json::Value> = docs
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "type": format!("{:?}", d.doc_type),
                            "title": &d.title,
                            "content": &d.content,
                        })
                    })
                    .collect();
                serde_json::json!({ "docs": results, "count": results.len() })
            }

            "check_tool_exists" => {
                let name = parameters["name"].as_str().unwrap_or("");
                let exists = self.capabilities.find_action(name).is_some();
                let similar: Vec<String> = if exists {
                    vec![]
                } else {
                    // Find tools with similar names (substring or edit-distance-like match)
                    let name_lower = name.to_lowercase();
                    // Normalize: replace hyphens with underscores and vice versa for matching
                    let alt_name = if name.contains('_') {
                        name.replace('_', "-")
                    } else {
                        name.replace('-', "_")
                    };
                    self.capabilities
                        .all_actions()
                        .iter()
                        .filter(|a| {
                            let a_lower = a.name.to_lowercase();
                            a_lower.contains(&name_lower)
                                || name_lower.contains(&a_lower)
                                || a.name == alt_name
                        })
                        .map(|a| a.name.clone())
                        .collect()
                };
                serde_json::json!({ "exists": exists, "similar": similar })
            }

            "list_tools" => {
                let tools: Vec<serde_json::Value> = self
                    .capabilities
                    .all_actions()
                    .iter()
                    .map(|a| {
                        serde_json::json!({
                            "name": &a.name,
                            "description": &a.description,
                        })
                    })
                    .collect();
                serde_json::json!({ "tools": tools, "count": tools.len() })
            }

            _ => {
                return Err(EngineError::Effect {
                    reason: format!("unknown reflection action: {action_name}"),
                });
            }
        };

        Ok(ActionResult {
            call_id: String::new(),
            action_name: action_name.into(),
            output,
            is_error: false,
            duration: start.elapsed(),
        })
    }

    async fn available_actions(
        &self,
        _leases: &[CapabilityLease],
    ) -> Result<Vec<ActionDef>, EngineError> {
        // Reflection tools are always available regardless of leases
        Ok(Self::action_defs())
    }
}

/// Build the system prompt for a reflection CodeAct thread.
pub fn build_reflection_prompt(actions: &[ActionDef], thread_goal: &str) -> String {
    let mut prompt = String::from(REFLECTION_PREAMBLE);

    prompt.push_str("\n## Available tools (call as Python functions)\n\n");
    for action in actions {
        prompt.push_str(&format!("- `{}(", action.name));
        if let Some(props) = action.parameters_schema.get("properties")
            && let Some(obj) = props.as_object()
        {
            let params: Vec<&str> = obj.keys().map(String::as_str).collect();
            prompt.push_str(&params.join(", "));
        }
        prompt.push_str(&format!(")` — {}\n", action.description));
    }

    prompt.push_str(&format!(
        "\n## Thread Under Analysis\n\nGoal: {thread_goal}\n"
    ));

    prompt.push_str(REFLECTION_POSTAMBLE);
    prompt
}

const REFLECTION_PREAMBLE: &str = "\
You are analyzing a completed agent thread to extract structured knowledge. \
You have tools to inspect the thread's execution, check existing knowledge, \
and verify tool names.

Write Python code in ```repl blocks to analyze the thread.";

const REFLECTION_POSTAMBLE: &str = r#"

## Your Task

1. Call `get_transcript()` to read the thread's execution history
2. Analyze the transcript for: successes, failures, tool errors, lessons learned
3. Call `query_memory(query)` to check if similar knowledge already exists
4. For any tool errors with "not found", call `check_tool_exists(name)` to find the correct name
5. Call `FINAL()` with a JSON object containing a `docs` array:

```repl
FINAL({
    "docs": [
        {"type": "summary", "title": "...", "content": "2-4 sentence summary"},
        {"type": "lesson", "title": "...", "content": "what was learned"},
        {"type": "spec", "title": "...", "content": "ALIAS: wrong_name -> correct_name"},
        {"type": "playbook", "title": "...", "content": "1. step one\n2. step two"}
    ]
})
```

Rules:
- Always include a "summary" doc
- Include "lesson" only if there were errors or workarounds
- Include "spec" only if tool-not-found errors occurred (verify with check_tool_exists)
- Include "playbook" only if the thread completed successfully with 2+ tool calls
- Skip docs that duplicate existing knowledge (check with query_memory first)
- Keep content concise — each doc should be a few sentences, not paragraphs"#;
