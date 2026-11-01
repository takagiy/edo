pub mod cli;
pub mod command;
pub mod edit;
pub mod install;
pub mod language;
pub mod matcher;
pub mod mcp;
pub mod outline;
pub mod scope;
pub mod view;

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// Files a glob matched but edo has no language package for, counted by extension.
pub type Skipped = BTreeMap<String, usize>;

/// `{"reason":"unsupported_language","extensions":{...}}`, shared by every place that reports skipped files.
pub fn skipped_json(skipped: &Skipped) -> Value {
    json!({"reason": "unsupported_language", "extensions": skipped})
}

/// An error reported to the agent as `{"ok":false,"error":code,"message":...}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
    pub skipped: Skipped,
}

impl Error {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Error { code, message: message.into(), skipped: Skipped::new() }
    }

    pub fn to_json(&self) -> Value {
        let mut v = json!({"ok": false, "error": self.code, "message": self.message});
        if !self.skipped.is_empty() {
            v["skipped"] = skipped_json(&self.skipped);
        }
        v
    }
}
