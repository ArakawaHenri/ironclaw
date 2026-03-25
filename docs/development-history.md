# Development History

Summary of the Claude Code sessions that built the engine v2, self-improvement system, and Python orchestrator. This helps new contributors understand *why* things were designed the way they are.

## Session 1: Engine v2 Foundation (2026-03-20 to 2026-03-22)

Built the core engine crate (`crates/ironclaw_engine/`) from scratch in 6 phases:

- **Phase 1**: Core types (Thread, Step, Capability, MemoryDoc, Project), trait definitions (LlmBackend, Store, EffectExecutor), thread state machine. 32 tests.
- **Phase 2**: Execution engine (Tier 0) — CapabilityRegistry, LeaseManager, PolicyEngine, ThreadManager, ExecutionLoop with structured tool calls. 74 tests.
- **Phase 3**: CodeAct executor (Tier 1) — Monty Python interpreter integration, RLM pattern (context-as-variables, FINAL(), llm_query(), output truncation, Step 0 orientation). 74 tests.
- **Phase 4**: Memory and reflection — RetrievalEngine, reflection pipeline (Summary/Lesson/Issue/Spec/Playbook docs), context compaction, rlm_query() recursive sub-agents, budget controls. 78 tests.
- **Phase 5**: Conversation surface — ConversationManager routing UI messages to threads. 85 tests.
- **Phase 6**: Bridge adapters — LlmBridgeAdapter, EffectBridgeAdapter, HybridStore, EngineRouter. Parallel deployment via `ENGINE_V2=true`. 151 tests.

**Key design decision**: The engine has zero dependency on the main ironclaw crate. All interaction goes through three traits (LlmBackend, Store, EffectExecutor) implemented by bridge adapters.

## Session 2: Debugging via Traces (2026-03-22 to 2026-03-23)

Ran the engine end-to-end with real LLMs and discovered 8 bugs through trace analysis:

1. Tool name hyphens vs underscores (`web-search` vs `web_search`)
2. Double-serialization of JSON tool output
3. UTF-8 byte-index slicing panics on multi-byte characters
4. Code block detection missing in plain completion path
5. Missing system prompt on thread spawn
6. Empty messages sent to LLM
7. `web_fetch` example in prompt (nonexistent tool)
8. False positive `missing_tool_output` trace warning

**Key insight**: Every fix followed the same loop (trace → human reads → human edits Rust → rebuild). This became the motivation for the self-improving engine design.

## Session 3: Mission System (2026-03-24)

Built the Mission system for long-running goals that spawn threads over time:

- `MissionManager` with create/pause/resume/complete lifecycle
- `MissionCadence`: Cron, OnEvent, OnSystemEvent, Webhook, Manual
- `build_meta_prompt()` — assembles mission goal + current focus + approach history + project docs + trigger payload
- `process_mission_outcome()` — extracts next_focus and goal-achieved status from thread responses
- Cron ticker (60s interval)
- 7 E2E mission flow tests

**Key design decision**: Missions evolve their strategy via `current_focus` and `approach_history`. Each thread gets a meta-prompt that includes what was tried before.

## Session 4: Review Fixes + Self-Improvement Foundation (2026-03-25, morning)

Fixed 4 review comments (P1/P2 severity) in the engine v2 bridge:

1. **SSE events scoped to user** — `broadcast_for_user()` instead of `broadcast()`
2. **Per-user pending approvals** — HashMap keyed by user_id instead of global Option
3. **Reset tool-call limit counter** — reset before each thread, not monotonic
4. **Only auto-approve on "always"** — one-off "yes" no longer persists

Then built the self-improvement foundation:

- Runtime prompt overlay via MemoryDoc (prompt builder becomes async + Store-aware)
- `fire_on_system_event()` — wires the previously-unimplemented OnSystemEvent cadence
- `start_event_listener()` — subscribes to thread events, fires matching missions
- `ensure_self_improvement_mission()` — creates the built-in self-improvement Mission
- `process_self_improvement_output()` — saves prompt overlays and fix patterns
- Seed fix pattern database with 8 known patterns

## Session 5: Autoresearch-Inspired Redesign (2026-03-25, afternoon)

Studied [karpathy/autoresearch](https://github.com/karpathy/autoresearch) and redesigned the self-improvement approach:

**Before**: Vague goal prompt, structured JSON output, reactive only.
**After**: Concrete `program.md`-style prompt with exact loop steps, plain text + tool-use (agent uses tools directly like autoresearch), enriched trigger payload with actual error messages.

Key takeaways applied from autoresearch:
- The entire "research org" is a markdown prompt with an explicit loop
- The agent uses tools directly (shell, grep, git) rather than emitting structured output
- Results tracked in a simple append-only log
- "NEVER STOP" — the agent is autonomous within constraints

## Session 6: Python Orchestrator (2026-03-25, evening)

The pivotal architectural change. Motivated by the question: *"What if we move some part of the engine inside CodeAct itself?"*

**The realization**: All the bugs from Session 2 were in the "glue" between the LLM and tools — output formatting, tool dispatch, state management, truncation. These functions are Python-natural. If they were Python, the self-improvement Mission could fix them without a Rust rebuild.

**Research**: Verified that Monty supports nested VM execution (`rlm_query()` already does exactly this — suspends parent VM, runs child ExecutionLoop, resumes parent). No shared state, ~50KB per suspended VM.

**Implementation** (4 commits):

1. **Host function module** (`executor/orchestrator.rs`) — 11 host functions exposed to Python via Monty suspension: `__llm_complete__`, `__execute_code_step__`, `__execute_action__`, `__check_signals__`, `__emit_event__`, `__add_message__`, `__save_checkpoint__`, `__transition_to__`, `__retrieve_docs__`, `__check_budget__`, `__get_actions__`.

2. **Default orchestrator** (`orchestrator/default.py`) — The v0 Python orchestrator that replicates the Rust loop logic. Helper functions (extract_final, format_output, signals_tool_intent) defined before run_loop for Monty scoping.

3. **Switchover** — Replaced the 900-line `ExecutionLoop::run()` with an 80-line bootstrap. Key debugging: Monty's `ExtFunctionResult::NotFound` (not `Error`) for user-defined functions, FINAL result propagation, step_count tracking via `__emit_event__("step_completed")`.

4. **Versioning + rollback** — Failure tracking via MemoryDoc, auto-rollback after 3 consecutive failures, `OrchestratorRollback` event. Self-improvement Mission goal updated with Level 1.5 orchestrator patch instructions.

**Key debugging moment**: The orchestrator's helper functions (`extract_final`, `format_output`) were defined after `run_loop` in the Python file. Monty couldn't find them because the default `FunctionCall` handler returned `ExtFunctionResult::Error` instead of `ExtFunctionResult::NotFound`. The fix: return `NotFound` for unknown functions so Monty falls through to its own namespace resolution. Then move helpers above `run_loop` to avoid any ordering issues.

**Final state**: 189 tests pass, zero clippy warnings. The Python orchestrator is the execution engine. The Rust layer is the kernel.

## Architecture Evolution

```
Session 1-2:  Rust loop (900 lines) → works but bugs in glue layer
Session 3:    + Missions (long-running goals, evolving strategy)
Session 4:    + Self-improvement Mission (fires on issues, fixes prompts)
Session 5:    + Autoresearch-style goal prompt (concrete, not vague)
Session 6:    Rust loop → Python orchestrator (self-modifiable)
              900 lines Rust → 80 lines Rust bootstrap + 230 lines Python
```

## Key Commits

| Commit | Description |
|--------|-------------|
| `8be19a4` | Phase 1: Foundation types + traits |
| `bf7dfb8` | Phase 2: Tier 0 execution engine |
| `b59a0b9` | Phase 3: CodeAct (Monty + RLM) |
| `4bc7ffd` | Phase 4: Memory + reflection + budgets |
| `0827235` | Phase 5: Conversation surface |
| `ac4ced0` | Phase 6: Bridge adapters (parallel deploy) |
| `8180a417` | Self-improving engine via Mission system |
| `cfe856da` | Python orchestrator module + host functions |
| `63756039` | Switch ExecutionLoop to Python orchestrator |
| `080317aa` | All 177 tests pass with orchestrator |
| `46fd2b5d` | Versioning, auto-rollback, 189 tests |
