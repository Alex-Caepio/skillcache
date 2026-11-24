//! Core SkillCache capsule model, Gate 1 verifier, compiler prompt/rules, sandboxed tool runner, and warm executor.
#![allow(dead_code)]

pub mod candidate;
pub mod capsule;
pub mod cli;
pub mod compile_rules;
pub mod env_filter;
pub mod executor;
pub mod fingerprint;
pub mod gate1;
pub mod package;
pub mod prompt;
pub mod state;
pub mod tool;
pub mod trace;

pub use capsule::{BindingOrigin, Examples, Form, SCHEMA_VERSION, SkillCapsule, Step, Violation};
