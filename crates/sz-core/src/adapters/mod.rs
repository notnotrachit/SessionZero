//! One adapter per agent. Scan and preview bodies are stubs until filled in.

pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod gemini;
pub mod grok;
pub mod kiro_cli;
pub mod kiro_crew;
pub mod opencode;
pub mod pi;
pub mod vscode;

use std::path::Path;

use crate::adapter::Adapter;

pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use copilot::CopilotAdapter;
pub use cursor::CursorAdapter;
pub use gemini::GeminiAdapter;
pub use grok::GrokAdapter;
pub use kiro_cli::KiroCliAdapter;
pub use kiro_crew::KiroCrewAdapter;
pub use opencode::OpenCodeAdapter;
pub use pi::PiAdapter;
pub use vscode::VscodeAdapter;

pub fn all_adapters(home: &Path) -> Vec<Box<dyn Adapter>> {
    vec![
        Box::new(CodexAdapter::new(home)),
        Box::new(ClaudeAdapter::new(home)),
        Box::new(KiroCliAdapter::new(home)),
        Box::new(KiroCrewAdapter::new(home)),
        Box::new(OpenCodeAdapter::new(home)),
        Box::new(GrokAdapter::new(home)),
        Box::new(GeminiAdapter::new(home)),
        Box::new(CursorAdapter::new(home)),
        Box::new(VscodeAdapter::new(home)),
        Box::new(CopilotAdapter::new(home)),
        Box::new(PiAdapter::new(home)),
    ]
}
