use std::path::PathBuf;

use crate::model::{AgentKind, PreviewMsg, Session};

pub trait Adapter: Send + Sync {
    fn kind(&self) -> AgentKind;
    /// Directories this agent stores data in.
    fn roots(&self) -> Vec<PathBuf>;
    fn detect(&self) -> bool {
        self.roots().iter().any(|p| p.exists())
    }
    fn scan(&self) -> anyhow::Result<Vec<Session>>;
    fn preview(&self, s: &Session, max: usize) -> anyhow::Result<Vec<PreviewMsg>>;
    /// Process names used to detect a running agent.
    fn process_names(&self) -> &'static [&'static str];
    /// Ids of sessions that are open in this agent right now. Trash refuses to delete them unless
    /// forced. Adapters that cannot tell return an empty list.
    fn live_session_ids(&self) -> Vec<String> {
        vec![]
    }
}
