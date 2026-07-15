//! esp-template-sdk — the versioned template contract for esp-generate.

pub mod config;
pub mod contract;
pub mod process;
pub mod template;

pub use process::{Facts, ProcessError, is_safe_relative_path, process_file};
