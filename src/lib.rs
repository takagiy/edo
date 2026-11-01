pub mod language;
pub mod outline;
pub mod scope;
pub mod view;
pub mod matcher;
pub mod edit;
pub mod command;
pub mod cli;
pub mod mcp;
pub mod install;

/// An error reported to the agent as `{"ok":false,"error":code,"message":...}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
}

impl Error {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Error { code, message: message.into() }
    }
}
