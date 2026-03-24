//! Post-thread reflection pipeline.
//!
//! After a thread completes, [`reflect()`] spawns a CodeAct thread with
//! reflection-specific tools to produce structured knowledge (MemoryDocs):
//! - Summary — what the thread accomplished
//! - Lesson — what was learned from errors/workarounds
//! - Issue — unresolved problems for follow-up
//! - Spec — missing capabilities / tool alias suggestions
//! - Playbook — reusable multi-step procedures from successful threads
//!
//! The reflection thread can introspect the completed thread's transcript,
//! query existing knowledge, and verify tool names against the capability
//! registry. [`reflect_simple()`] is a fallback using direct LLM calls.

pub mod executor;
pub mod pipeline;

pub use pipeline::{reflect, reflect_simple, ReflectionResult};
