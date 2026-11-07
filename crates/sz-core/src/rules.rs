//! Cleanup rules (age, size, agent, project filters) used by `clean`.

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Duration, Utc};
use regex::RegexBuilder;

use crate::model::{AgentKind, Session};

/// Every set field must match (AND). An empty rule matches every session.
#[derive(Default, Clone, Debug)]
pub struct Rule {
    pub older_than: Option<Duration>,
    pub larger_than: Option<u64>,
    pub agents: Vec<AgentKind>,
    /// Case-insensitive regex matched against the session's project path.
    pub project: Option<String>,
    pub no_project: bool,
}

/// Parses `30d`, `12h`, `2w`, `6m` (months, 30 days each) or `1y` (365 days).
pub fn parse_duration(s: &str) -> Result<Duration> {
    let text = s.trim().to_ascii_lowercase();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (num, unit) = text.split_at(split);
    let n: i64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid duration '{s}': expected a number then h, d, w, m or y"))?;
    let dur = match unit.trim() {
        "h" => Duration::try_hours(n),
        "d" => Duration::try_days(n),
        "w" => Duration::try_weeks(n),
        "m" => n.checked_mul(30).and_then(Duration::try_days),
        "y" => n.checked_mul(365).and_then(Duration::try_days),
        other => bail!("unknown duration unit '{other}' in '{s}'"),
    };
    dur.ok_or_else(|| anyhow!("duration '{s}' is out of range"))
}

/// Parses a size such as `500KB`, `100MB`, `1.5GB` or `2G`, or plain bytes. 1 KB = 1024 bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let text = s.trim().to_ascii_lowercase();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (num, unit) = text.split_at(split);
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid size '{s}': expected a number then a unit such as MB"))?;
    if !n.is_finite() || n < 0.0 {
        bail!("invalid size '{s}': must be a non-negative number");
    }
    const K: f64 = 1024.0;
    let mult = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => K,
        "m" | "mb" | "mib" => K * K,
        "g" | "gb" | "gib" => K * K * K,
        "t" | "tb" | "tib" => K * K * K * K,
        other => bail!("unknown size unit '{other}' in '{s}'"),
    };
    Ok((n * mult).round() as u64)
}

/// Maps a label or alias (`kiro-cli`, `kirocrew`, `antigravity`, ...) to an agent.
pub fn parse_agent(s: &str) -> Option<AgentKind> {
    let key = s.trim().to_ascii_lowercase();
    if let Some(kind) = AgentKind::all().iter().find(|k| k.label() == key) {
        return Some(*kind);
    }
    Some(match key.as_str() {
        "claude-code" | "claude_code" => AgentKind::Claude,
        "kiro_cli" | "kirocli" | "kiro-cli" => AgentKind::KiroCli,
        "kiro_crew" | "kirocrew" | "crew" => AgentKind::KiroCrew,
        "open-code" | "open_code" => AgentKind::OpenCode,
        "antigravity" => AgentKind::Gemini,
        "code" | "vs-code" | "vscode-insiders" => AgentKind::Vscode,
        "gh-copilot" | "github-copilot" => AgentKind::Copilot,
        "pi-agent" | "pi-coding-agent" => AgentKind::Pi,
        _ => return None,
    })
}

impl Rule {
    pub fn matches(&self, s: &Session, now: DateTime<Utc>) -> bool {
        if !self.agents.is_empty() && !self.agents.contains(&s.agent) {
            return false;
        }
        if let Some(age) = self.older_than {
            match s.last_active.or(s.created) {
                Some(t) if now - t > age => {}
                _ => return false,
            }
        }
        if let Some(min) = self.larger_than {
            if s.bytes <= min {
                return false;
            }
        }
        if self.no_project && s.project.is_some() {
            return false;
        }
        if let Some(pattern) = &self.project {
            let Ok(re) = RegexBuilder::new(pattern).case_insensitive(true).build() else {
                return false;
            };
            let Some(project) = &s.project else {
                return false;
            };
            if !re.is_match(&project.to_string_lossy()) {
                return false;
            }
        }
        true
    }

    pub fn is_empty(&self) -> bool {
        self.older_than.is_none()
            && self.larger_than.is_none()
            && self.agents.is_empty()
            && self.project.is_none()
            && !self.no_project
    }
}

/// Indices of the sessions that match `rule`.
pub fn select(sessions: &[Session], rule: &Rule, now: DateTime<Utc>) -> Vec<usize> {
    sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| rule.matches(s, now))
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Artifact;
    use chrono::TimeZone;
    use std::path::PathBuf;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 7, 12, 0, 0).unwrap()
    }

    fn session(agent: AgentKind, bytes: u64, last: Option<DateTime<Utc>>) -> Session {
        Session {
            parent_id: None,
            agent,
            id: format!("{}-{bytes}", agent.label()),
            title: None,
            project: None,
            created: None,
            last_active: last,
            message_count: None,
            bytes,
            artifacts: vec![Artifact::File(PathBuf::from("/x/f"))],
        }
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("30d").unwrap(), Duration::days(30));
        assert_eq!(parse_duration("12h").unwrap(), Duration::hours(12));
        assert_eq!(parse_duration("2w").unwrap(), Duration::weeks(2));
        assert_eq!(parse_duration("6m").unwrap(), Duration::days(180));
        assert_eq!(parse_duration("1y").unwrap(), Duration::days(365));
        assert_eq!(parse_duration(" 3 d ").unwrap(), Duration::days(3));
        assert!(parse_duration("30").is_err());
        assert!(parse_duration("3x").is_err());
        assert!(parse_duration("d").is_err());
        assert!(parse_duration("99999999999999999999y").is_err());
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("500").unwrap(), 500);
        assert_eq!(parse_size("500B").unwrap(), 500);
        assert_eq!(parse_size("500KB").unwrap(), 500 * 1024);
        assert_eq!(parse_size("100MB").unwrap(), 100 * 1024 * 1024);
        assert_eq!(parse_size("1.5GB").unwrap(), 1_610_612_736);
        assert_eq!(parse_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert!(parse_size("MB").is_err());
        assert!(parse_size("5 furlongs").is_err());
        assert!(parse_size("-1MB").is_err());
    }

    #[test]
    fn parses_agents_and_aliases() {
        assert_eq!(parse_agent("codex"), Some(AgentKind::Codex));
        assert_eq!(parse_agent("kiro"), Some(AgentKind::KiroCli));
        assert_eq!(parse_agent("kiro-cli"), Some(AgentKind::KiroCli));
        assert_eq!(parse_agent("kiro-crew"), Some(AgentKind::KiroCrew));
        assert_eq!(parse_agent("kirocrew"), Some(AgentKind::KiroCrew));
        assert_eq!(parse_agent("antigravity"), Some(AgentKind::Gemini));
        assert_eq!(parse_agent("Claude"), Some(AgentKind::Claude));
        assert_eq!(parse_agent("nope"), None);
    }

    #[test]
    fn empty_rule_matches_everything() {
        let rule = Rule::default();
        assert!(rule.is_empty());
        assert!(rule.matches(&session(AgentKind::Codex, 1, None), now()));
    }

    #[test]
    fn older_than_uses_last_active_then_created_and_skips_unknown() {
        let rule = Rule {
            older_than: Some(Duration::days(30)),
            ..Rule::default()
        };
        let old = now() - Duration::days(40);
        let recent = now() - Duration::days(2);
        assert!(rule.matches(&session(AgentKind::Codex, 1, Some(old)), now()));
        assert!(!rule.matches(&session(AgentKind::Codex, 1, Some(recent)), now()));
        assert!(!rule.matches(&session(AgentKind::Codex, 1, None), now()));

        let mut created_only = session(AgentKind::Codex, 1, None);
        created_only.created = Some(old);
        assert!(rule.matches(&created_only, now()));
    }

    #[test]
    fn larger_than_is_strict() {
        let rule = Rule {
            larger_than: Some(100),
            ..Rule::default()
        };
        assert!(!rule.matches(&session(AgentKind::Codex, 100, None), now()));
        assert!(rule.matches(&session(AgentKind::Codex, 101, None), now()));
    }

    #[test]
    fn fields_combine_with_and() {
        let rule = Rule {
            agents: vec![AgentKind::Claude],
            larger_than: Some(10),
            ..Rule::default()
        };
        assert!(rule.matches(&session(AgentKind::Claude, 50, None), now()));
        assert!(!rule.matches(&session(AgentKind::Codex, 50, None), now()));
        assert!(!rule.matches(&session(AgentKind::Claude, 5, None), now()));
    }

    #[test]
    fn project_regex_and_no_project() {
        let mut in_repo = session(AgentKind::Codex, 1, None);
        in_repo.project = Some(PathBuf::from("/work/Alpha-App"));
        let no_project = session(AgentKind::Codex, 1, None);

        let by_regex = Rule {
            project: Some("^/work/alpha".into()),
            ..Rule::default()
        };
        assert!(by_regex.matches(&in_repo, now()));
        assert!(!by_regex.matches(&no_project, now()));

        let orphan = Rule {
            no_project: true,
            ..Rule::default()
        };
        assert!(!orphan.matches(&in_repo, now()));
        assert!(orphan.matches(&no_project, now()));
    }

    #[test]
    fn select_returns_matching_indices() {
        let sessions = vec![
            session(AgentKind::Codex, 1, None),
            session(AgentKind::Claude, 1, None),
            session(AgentKind::Codex, 1, None),
        ];
        let rule = Rule {
            agents: vec![AgentKind::Codex],
            ..Rule::default()
        };
        assert_eq!(select(&sessions, &rule, now()), vec![0, 2]);
    }
}
