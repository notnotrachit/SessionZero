//! Grouping and sorting of sessions.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};
use regex::RegexBuilder;

use crate::model::Session;
use crate::util::is_placeholder_project;

const NO_PROJECT: &str = "(no project)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupBy {
    /// Project, then agent.
    Project,
    /// Agent, then project.
    Agent,
    /// Parent folder of the project root, then project.
    Folder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortBy {
    Size,
    Recent,
    Oldest,
    Messages,
    Name,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub key: String,
    pub label: String,
    pub bytes: u64,
    pub count: usize,
    pub last_active: Option<DateTime<Utc>>,
    pub children: Vec<Group>,
    /// Indices into the input slice, in `sort` order. Only set on leaf groups.
    pub sessions: Vec<usize>,
}

/// Aggregate for one group while the tree is being built.
struct Node {
    key: String,
    label: String,
    bytes: u64,
    count: usize,
    last_active: Option<DateTime<Utc>>,
    /// `last_active`, falling back to `created`. Used only for sorting.
    sort_time: Option<DateTime<Utc>>,
    messages: u64,
    children: BTreeMap<String, Node>,
    sessions: Vec<usize>,
}

impl Node {
    fn new(key: String, label: String) -> Self {
        Node {
            key,
            label,
            bytes: 0,
            count: 0,
            last_active: None,
            sort_time: None,
            messages: 0,
            children: BTreeMap::new(),
            sessions: Vec::new(),
        }
    }

    fn add(&mut self, s: &Session, index: Option<usize>) {
        self.bytes += s.bytes;
        self.count += 1;
        self.messages += s.message_count.unwrap_or(0);
        self.last_active = self.last_active.max(s.last_active);
        self.sort_time = self.sort_time.max(s.last_active.or(s.created));
        if let Some(i) = index {
            self.sessions.push(i);
        }
    }

    fn into_group(mut self, sessions: &[Session], sort: SortBy) -> Group {
        let mut kids: Vec<Node> = std::mem::take(&mut self.children).into_values().collect();
        kids.sort_by(|a, b| node_cmp(a, b, sort));
        let children = kids
            .into_iter()
            .map(|k| k.into_group(sessions, sort))
            .collect();
        let mut idx = std::mem::take(&mut self.sessions);
        sort_indices(sessions, &mut idx, sort);
        Group {
            key: self.key,
            label: self.label,
            bytes: self.bytes,
            count: self.count,
            last_active: self.last_active,
            children,
            sessions: idx,
        }
    }
}

fn cmp_time_desc(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn cmp_time_asc(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn node_cmp(a: &Node, b: &Node, sort: SortBy) -> Ordering {
    let primary = match sort {
        SortBy::Size => b.bytes.cmp(&a.bytes),
        SortBy::Recent => cmp_time_desc(a.sort_time, b.sort_time),
        SortBy::Oldest => cmp_time_asc(a.sort_time, b.sort_time),
        SortBy::Messages => b.messages.cmp(&a.messages),
        SortBy::Name => a.label.to_lowercase().cmp(&b.label.to_lowercase()),
    };
    primary.then_with(|| a.key.cmp(&b.key))
}

/// Sorts session indices in place. Sessions without a date sort last for `Recent` and `Oldest`.
// `&mut Vec` is the public signature in docs/core-api.md, so the lint is allowed here.
#[allow(clippy::ptr_arg)]
pub fn sort_indices(sessions: &[Session], idx: &mut Vec<usize>, sort: SortBy) {
    let time = |s: &Session| s.last_active.or(s.created);
    idx.sort_by(|&a, &b| {
        let (sa, sb) = (&sessions[a], &sessions[b]);
        let primary = match sort {
            SortBy::Size => sb.bytes.cmp(&sa.bytes),
            SortBy::Recent => cmp_time_desc(time(sa), time(sb)),
            SortBy::Oldest => cmp_time_asc(time(sa), time(sb)),
            SortBy::Messages => sb
                .message_count
                .unwrap_or(0)
                .cmp(&sa.message_count.unwrap_or(0)),
            SortBy::Name => sa
                .title
                .as_deref()
                .unwrap_or(&sa.id)
                .to_lowercase()
                .cmp(&sb.title.as_deref().unwrap_or(&sb.id).to_lowercase()),
        };
        primary.then_with(|| sa.key().cmp(&sb.key()))
    });
}

/// Builds the group tree. Both levels aggregate bytes, count and last activity from their sessions.
pub fn group(sessions: &[Session], by: GroupBy, sort: SortBy) -> Vec<Group> {
    let home = dirs::home_dir();
    let mut tops: BTreeMap<String, Node> = BTreeMap::new();
    for (i, s) in sessions.iter().enumerate() {
        let agent = (s.agent.label().to_string(), s.agent.label().to_string());
        // Placeholder paths (`/`, the home directory) name no project, even when a cache written
        // by an older build still carries them. A repository root is never a placeholder.
        let root = s
            .project
            .as_deref()
            .filter(|p| !is_placeholder_project(p, home.as_deref()))
            .map(project_root)
            .filter(|r| !is_placeholder_project(r, home.as_deref()));
        let project = match &root {
            Some(r) => (r.to_string_lossy().into_owned(), display_path(r)),
            None => (NO_PROJECT.to_string(), NO_PROJECT.to_string()),
        };
        let folder = match &root {
            Some(r) => {
                let parent = r.parent().unwrap_or(r.as_path());
                (parent.to_string_lossy().into_owned(), display_path(parent))
            }
            None => (NO_PROJECT.to_string(), NO_PROJECT.to_string()),
        };
        let (outer, inner) = match by {
            GroupBy::Project => (project, agent),
            GroupBy::Agent => (agent, project),
            GroupBy::Folder => (folder, project),
        };
        let top = tops
            .entry(outer.0.clone())
            .or_insert_with(|| Node::new(outer.0, outer.1));
        top.add(s, None);
        let child = top
            .children
            .entry(inner.0.clone())
            .or_insert_with(|| Node::new(inner.0, inner.1));
        child.add(s, Some(i));
    }

    let mut out: Vec<Node> = tops.into_values().collect();
    out.sort_by(|a, b| node_cmp(a, b, sort));
    out.into_iter()
        .map(|n| n.into_group(sessions, sort))
        .collect()
}

/// Case-insensitive regex over title, project, agent label and id. An invalid regex is
/// matched as a literal substring.
pub fn filter(sessions: &[Session], pattern: &str) -> Vec<usize> {
    let re = RegexBuilder::new(pattern).case_insensitive(true).build();
    let needle = pattern.to_lowercase();
    sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            let fields = [
                s.title.clone().unwrap_or_default(),
                s.project
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                s.agent.label().to_string(),
                s.id.clone(),
            ];
            match &re {
                Ok(re) => fields.iter().any(|f| re.is_match(f)),
                Err(_) => fields.iter().any(|f| f.to_lowercase().contains(&needle)),
            }
        })
        .map(|(i, _)| i)
        .collect()
}

/// Nearest ancestor of `p` (inclusive) that contains `.git`, or `p` itself when none does.
/// Results are memoized for the life of the process.
pub fn project_root(p: &Path) -> PathBuf {
    static MEMO: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();
    let memo = MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = memo.lock().ok().and_then(|m| m.get(p).cloned()) {
        return hit;
    }
    let root = p
        .ancestors()
        .find(|a| a.join(".git").exists())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| p.to_path_buf());
    if let Ok(mut m) = memo.lock() {
        m.insert(p.to_path_buf(), root.clone());
    }
    root
}

/// Shows a path under the home directory as `~/...`.
pub fn display_path(p: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = p.strip_prefix(&home) {
            let rest = rest.to_string_lossy();
            return if rest.is_empty() {
                "~".to_string()
            } else {
                format!("~/{rest}")
            };
        }
    }
    p.display().to_string()
}

/// Formats a byte count with 1024-based units, e.g. `1.2 GB`.
pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if b < 1024 {
        return format!("{b} B");
    }
    let mut value = b as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AgentKind;
    use chrono::TimeZone;

    fn session(
        agent: AgentKind,
        id: &str,
        project: Option<PathBuf>,
        bytes: u64,
        last: Option<DateTime<Utc>>,
    ) -> Session {
        Session {
            parent_id: None,
            agent,
            id: id.into(),
            title: Some(format!("title {id}")),
            project,
            created: None,
            last_active: last,
            message_count: Some(bytes / 10),
            bytes,
            artifacts: vec![],
        }
    }

    fn t(day: u32) -> Option<DateTime<Utc>> {
        Some(Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap())
    }

    /// `<tmp>/parent/repo/.git` with two sub-directories, plus a plain folder without git.
    fn fixture() -> (tempfile::TempDir, Vec<Session>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("parent").join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        let plain = tmp.path().join("parent").join("plain");
        std::fs::create_dir_all(&plain).unwrap();

        let sessions = vec![
            session(AgentKind::Codex, "a", Some(repo.join("sub")), 100, t(1)),
            session(AgentKind::Claude, "b", Some(repo.clone()), 300, t(5)),
            session(AgentKind::Codex, "c", Some(plain.clone()), 50, t(3)),
            session(AgentKind::Claude, "d", None, 10, None),
        ];
        (tmp, sessions)
    }

    #[test]
    fn project_grouping_aggregates_and_nests_agents() {
        let (tmp, sessions) = fixture();
        let groups = group(&sessions, GroupBy::Project, SortBy::Size);
        let repo_key = tmp.path().join("parent").join("repo").to_string_lossy().into_owned();
        let repo = groups.iter().find(|g| g.key == repo_key).expect("repo group");
        assert_eq!(repo.count, 2);
        assert_eq!(repo.bytes, 400);
        assert_eq!(repo.last_active, t(5));
        assert!(repo.sessions.is_empty(), "parents hold no sessions");
        assert_eq!(repo.children.len(), 2);
        let codex = repo.children.iter().find(|g| g.key == "codex").unwrap();
        assert_eq!(codex.sessions, vec![0]);
        assert!(codex.children.is_empty());

        assert_eq!(groups.last().unwrap().key, NO_PROJECT);
        assert_eq!(groups.last().unwrap().children[0].sessions, vec![3]);
    }

    #[test]
    fn agent_grouping_puts_agent_on_top() {
        let (_tmp, sessions) = fixture();
        let groups = group(&sessions, GroupBy::Agent, SortBy::Name);
        let keys: Vec<&str> = groups.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(keys, vec!["claude", "codex"]);
        let claude = &groups[0];
        assert_eq!(claude.count, 2);
        assert_eq!(claude.bytes, 310);
        assert_eq!(claude.children.len(), 2);
        assert_eq!(claude.children[0].key, NO_PROJECT, "'(' sorts before '/' by name");
    }

    #[test]
    fn root_and_home_projects_are_treated_as_no_project() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("parent").join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let mut sessions = vec![
            session(AgentKind::KiroCli, "root", Some(PathBuf::from("/")), 10, t(1)),
            session(AgentKind::KiroCli, "repo", Some(repo.clone()), 20, t(2)),
        ];
        // A cache from an older build may still carry the home directory as a project.
        if let Some(home) = dirs::home_dir() {
            sessions.push(session(AgentKind::Codex, "home", Some(home), 30, t(3)));
        }

        for by in [GroupBy::Project, GroupBy::Agent, GroupBy::Folder] {
            let groups = group(&sessions, by, SortBy::Name);
            let keys: Vec<String> = all_keys(&groups);
            assert!(!keys.iter().any(|k| k == "/"), "{by:?} produced '/': {keys:?}");
        }

        let groups = group(&sessions, GroupBy::Folder, SortBy::Name);
        let no_project = groups.iter().find(|g| g.key == NO_PROJECT).expect("no-project group");
        let placed: Vec<usize> = no_project.children.iter().flat_map(|c| c.sessions.clone()).collect();
        assert!(placed.contains(&0), "root session is unplaced");
        let folder = groups
            .iter()
            .find(|g| g.key == tmp.path().join("parent").to_string_lossy().into_owned())
            .expect("repo folder");
        assert_eq!(folder.count, 1);
    }

    /// Every group key at every level, depth first.
    fn all_keys(groups: &[Group]) -> Vec<String> {
        groups
            .iter()
            .flat_map(|g| std::iter::once(g.key.clone()).chain(all_keys(&g.children)))
            .collect()
    }

    #[test]
    fn folder_grouping_uses_parent_of_project_root() {
        let (tmp, sessions) = fixture();
        let groups = group(&sessions, GroupBy::Folder, SortBy::Size);
        let parent_key = tmp.path().join("parent").to_string_lossy().into_owned();
        let parent = groups.iter().find(|g| g.key == parent_key).expect("folder group");
        assert_eq!(parent.count, 3);
        assert_eq!(parent.children.len(), 2);
        let no_project = groups.iter().find(|g| g.key == NO_PROJECT).unwrap();
        assert_eq!(no_project.children[0].sessions, vec![3]);
    }

    #[test]
    fn sort_orders_by_each_key() {
        let (_tmp, sessions) = fixture();
        let mut idx: Vec<usize> = (0..sessions.len()).collect();

        sort_indices(&sessions, &mut idx, SortBy::Size);
        assert_eq!(idx, vec![1, 0, 2, 3]);

        sort_indices(&sessions, &mut idx, SortBy::Recent);
        assert_eq!(idx, vec![1, 2, 0, 3], "undated sessions sort last");

        sort_indices(&sessions, &mut idx, SortBy::Oldest);
        assert_eq!(idx, vec![0, 2, 1, 3]);

        sort_indices(&sessions, &mut idx, SortBy::Messages);
        assert_eq!(idx, vec![1, 0, 2, 3]);

        sort_indices(&sessions, &mut idx, SortBy::Name);
        assert_eq!(idx, vec![0, 1, 2, 3]);
    }

    #[test]
    fn filter_matches_fields_case_insensitively() {
        let (_tmp, sessions) = fixture();
        assert_eq!(filter(&sessions, "CLAUDE"), vec![1, 3]);
        assert_eq!(filter(&sessions, "title c"), vec![2]);
        assert_eq!(filter(&sessions, "^d$"), vec![3]);
        assert!(filter(&sessions, "nomatch").is_empty());
    }

    #[test]
    fn invalid_regex_falls_back_to_literal_substring() {
        let (_tmp, sessions) = fixture();
        assert_eq!(filter(&sessions, "title (b"), Vec::<usize>::new());
        let mut odd = sessions.clone();
        odd[0].title = Some("fix (wip".into());
        assert_eq!(filter(&odd, "(wip"), vec![0]);
    }

    #[test]
    fn project_root_finds_git_ancestor_and_falls_back_to_self() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("r");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let deep = repo.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(project_root(&deep), repo);
        assert_eq!(project_root(&repo), repo);
    }

    #[test]
    fn display_path_and_human_bytes() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(display_path(&home), "~");
            assert_eq!(display_path(&home.join("code")), "~/code");
        }
        assert_eq!(display_path(Path::new("/opt/x")), "/opt/x");
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1_288_490_189), "1.2 GB");
        assert_eq!(human_bytes(3 * 1024u64.pow(4)), "3.0 TB");
    }
}
