use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::util::path_size;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AgentKind {
    Codex,
    Claude,
    KiroCli,
    KiroCrew,
    OpenCode,
    Grok,
    Gemini,
    Cursor,
    Vscode,
    Copilot,
    Pi,
}

impl AgentKind {
    pub fn label(&self) -> &'static str {
        match self {
            AgentKind::Codex => "codex",
            AgentKind::Claude => "claude",
            AgentKind::KiroCli => "kiro",
            AgentKind::KiroCrew => "kiro-crew",
            AgentKind::OpenCode => "opencode",
            AgentKind::Grok => "grok",
            AgentKind::Gemini => "gemini",
            AgentKind::Cursor => "cursor",
            AgentKind::Vscode => "vscode",
            AgentKind::Copilot => "copilot",
            AgentKind::Pi => "pi",
        }
    }

    pub fn all() -> &'static [AgentKind] {
        &[
            AgentKind::Codex,
            AgentKind::Claude,
            AgentKind::KiroCli,
            AgentKind::KiroCrew,
            AgentKind::OpenCode,
            AgentKind::Grok,
            AgentKind::Gemini,
            AgentKind::Cursor,
            AgentKind::Vscode,
            AgentKind::Copilot,
            AgentKind::Pi,
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbDelete {
    pub table: String,
    pub where_col: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Artifact {
    File(PathBuf),
    Dir(PathBuf),
    DbRows {
        db: PathBuf,
        deletes: Vec<DbDelete>,
        bytes: u64,
    },
    /// Rows whose `col` starts with literal `prefix` (no LIKE wildcards). Delete via substr(col,1,length(?))=?.
    DbPrefix {
        db: PathBuf,
        table: String,
        col: String,
        prefix: String,
        bytes: u64,
    },
    /// Lines of a JSONL file where the JSON value at dotted `field` path (e.g. "session_id" or "payload.id") equals `value` as a string. File is shared: only those lines are removed.
    JsonlLines {
        file: PathBuf,
        field: String,
        value: String,
    },
    /// Elements of a JSON array inside a JSON file where element[`field`] == `value`. `array_path` is a dotted path to the array ("" = the root is the array, "entries" = root.entries).
    JsonArrayEntry {
        file: PathBuf,
        array_path: String,
        field: String,
        value: String,
    },
    /// A key of a JSON object inside a JSON file. `object_path` dotted ("" = root object).
    JsonObjectKey {
        file: PathBuf,
        object_path: String,
        key: String,
    },
}

impl Artifact {
    /// The file, directory or database path this artifact refers to.
    pub fn primary_path(&self) -> &Path {
        match self {
            Artifact::File(p) | Artifact::Dir(p) => p,
            Artifact::DbRows { db, .. } | Artifact::DbPrefix { db, .. } => db,
            Artifact::JsonlLines { file, .. }
            | Artifact::JsonArrayEntry { file, .. }
            | Artifact::JsonObjectKey { file, .. } => file,
        }
    }

    /// True when the file stays on disk and only part of it is removed.
    pub fn is_shared(&self) -> bool {
        matches!(
            self,
            Artifact::DbRows { .. }
                | Artifact::DbPrefix { .. }
                | Artifact::JsonlLines { .. }
                | Artifact::JsonArrayEntry { .. }
                | Artifact::JsonObjectKey { .. }
        )
    }

    /// Bytes this artifact owns exclusively. Shared files count as zero.
    pub fn own_bytes(&self) -> u64 {
        match self {
            Artifact::File(p) | Artifact::Dir(p) => path_size(p),
            Artifact::DbRows { bytes, .. } | Artifact::DbPrefix { bytes, .. } => *bytes,
            Artifact::JsonlLines { .. }
            | Artifact::JsonArrayEntry { .. }
            | Artifact::JsonObjectKey { .. } => 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub agent: AgentKind,
    pub id: String,
    pub title: Option<String>,
    pub project: Option<PathBuf>,
    pub created: Option<DateTime<Utc>>,
    pub last_active: Option<DateTime<Utc>>,
    pub message_count: Option<u64>,
    pub bytes: u64,
    pub artifacts: Vec<Artifact>,
    /// Id of the session this one was spawned from, when the agent records that link. Subagent
    /// and forked sessions point at their parent through it.
    #[serde(default)]
    pub parent_id: Option<String>,
}

impl Session {
    /// A session with only its identity set. Every other field starts empty.
    pub fn new(agent: AgentKind, id: impl Into<String>) -> Self {
        Self {
            agent,
            id: id.into(),
            title: None,
            project: None,
            created: None,
            last_active: None,
            message_count: None,
            bytes: 0,
            artifacts: Vec::new(),
            parent_id: None,
        }
    }

    pub fn key(&self) -> String {
        format!("{}:{}", self.agent.label(), self.id)
    }

    /// Sets `bytes` to the sum of every artifact's `own_bytes()`.
    pub fn recompute_bytes(&mut self) {
        self.bytes = self.artifacts.iter().map(Artifact::own_bytes).sum();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewMsg {
    pub role: String,
    pub text: String,
    pub ts: Option<DateTime<Utc>>,
}
#[cfg(test)]
mod tests {
    use super::*;

    fn all_variants() -> Vec<Artifact> {
        vec![
            Artifact::File(PathBuf::from("/a/file.jsonl")),
            Artifact::Dir(PathBuf::from("/a/dir")),
            Artifact::DbRows {
                db: PathBuf::from("/a/state.db"),
                deletes: vec![DbDelete {
                    table: "t".into(),
                    where_col: "c".into(),
                    value: "v".into(),
                }],
                bytes: 10,
            },
            Artifact::DbPrefix {
                db: PathBuf::from("/a/state.db"),
                table: "t".into(),
                col: "c".into(),
                prefix: "p".into(),
                bytes: 20,
            },
            Artifact::JsonlLines {
                file: PathBuf::from("/a/history.jsonl"),
                field: "payload.id".into(),
                value: "abc".into(),
            },
            Artifact::JsonArrayEntry {
                file: PathBuf::from("/a/sessions.json"),
                array_path: "entries".into(),
                field: "id".into(),
                value: "abc".into(),
            },
            Artifact::JsonObjectKey {
                file: PathBuf::from("/a/map.json"),
                object_path: "".into(),
                key: "abc".into(),
            },
        ]
    }

    #[test]
    fn artifact_serde_round_trip_all_variants() {
        for art in all_variants() {
            let json = serde_json::to_string(&art).expect("serialize");
            let back: Artifact = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(
                serde_json::to_string(&back).expect("re-serialize"),
                json,
                "round trip changed {json}"
            );
        }
    }

    #[test]
    fn is_shared_matches_variant_kind() {
        let shared: Vec<bool> = all_variants().iter().map(Artifact::is_shared).collect();
        assert_eq!(shared, vec![false, false, true, true, true, true, true]);
    }

    #[test]
    fn primary_path_returns_variant_path() {
        let paths: Vec<PathBuf> = all_variants()
            .iter()
            .map(|a| a.primary_path().to_path_buf())
            .collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/a/file.jsonl"),
                PathBuf::from("/a/dir"),
                PathBuf::from("/a/state.db"),
                PathBuf::from("/a/state.db"),
                PathBuf::from("/a/history.jsonl"),
                PathBuf::from("/a/sessions.json"),
                PathBuf::from("/a/map.json"),
            ]
        );
    }

    #[test]
    fn own_bytes_for_file_dir_and_shared_variants() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("f.bin");
        std::fs::write(&file, vec![0u8; 1234]).expect("write");
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).expect("mkdir");
        std::fs::write(sub.join("a.bin"), vec![0u8; 100]).expect("write");

        assert_eq!(Artifact::File(file.clone()).own_bytes(), 1234);
        assert_eq!(Artifact::Dir(sub.clone()).own_bytes(), path_size(&sub));
        assert!(Artifact::Dir(sub).own_bytes() >= 100);

        let db = Artifact::DbRows {
            db: file.clone(),
            deletes: vec![],
            bytes: 55,
        };
        assert_eq!(db.own_bytes(), 55);
        let prefix = Artifact::DbPrefix {
            db: file.clone(),
            table: "t".into(),
            col: "c".into(),
            prefix: "p".into(),
            bytes: 66,
        };
        assert_eq!(prefix.own_bytes(), 66);
        for art in all_variants().iter().skip(4) {
            assert_eq!(art.own_bytes(), 0);
        }
    }

    #[test]
    fn recompute_bytes_sums_own_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("f.bin");
        std::fs::write(&file, vec![0u8; 500]).expect("write");
        let mut session = Session {
            parent_id: None,
            agent: AgentKind::Codex,
            id: "x".into(),
            title: None,
            project: None,
            created: None,
            last_active: None,
            message_count: None,
            bytes: 0,
            artifacts: vec![
                Artifact::File(file.clone()),
                Artifact::DbRows {
                    db: file.clone(),
                    deletes: vec![],
                    bytes: 30,
                },
                Artifact::JsonlLines {
                    file: file.clone(),
                    field: "id".into(),
                    value: "x".into(),
                },
            ],
        };
        session.recompute_bytes();
        assert_eq!(session.bytes, 530);
    }
}
