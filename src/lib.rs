//! Standalone dual-compatible SkillCache engine: core capsule verifier/executor, sandboxed gate/compiler runner, SQLite registry, and MCP server.
pub mod core;
pub mod custody;
pub mod engine;
pub mod error;
pub mod host;
pub mod mcp;
