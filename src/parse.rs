use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Longest message text kept in the index, in characters.
const MAX_TEXT: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Pi,
}

impl Agent {
    pub const ALL: [Agent; 3] = [Agent::Claude, Agent::Codex, Agent::Pi];

    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Pi => "pi",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Agent::ALL.into_iter().find(|a| a.as_str() == s)
    }

    /// Where this agent keeps its session logs.
    pub fn root(self) -> PathBuf {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let dir = |var: &str, default: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(default))
        };
        match self {
            Agent::Claude => dir("CLAUDE_CONFIG_DIR", ".claude").join("projects"),
            Agent::Codex => dir("CODEX_HOME", ".codex").join("sessions"),
            Agent::Pi => dir("PI_CODING_AGENT_DIR", ".pi/agent").join("sessions"),
        }
    }

    /// Whether `path` under `root()` is one of this agent's session logs.
    /// Claude keeps subagent logs one level deeper; those are not sessions.
    pub fn is_log(self, path: &Path) -> bool {
        let jsonl = path.extension().is_some_and(|e| e == "jsonl");
        let depth = path
            .strip_prefix(self.root())
            .map(|p| p.components().count())
            .unwrap_or(0);
        match self {
            Agent::Claude => jsonl && depth == 2,
            Agent::Codex => {
                jsonl
                    && path
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("rollout-"))
            }
            Agent::Pi => jsonl && depth == 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub ts: i64,
    pub text: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub started: i64,
    pub updated: i64,
    pub messages: Vec<Message>,
}

fn ts(v: &Value) -> Option<i64> {
    let s = v.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp())
}

/// Text of a message's content: a string, or the text blocks of an array.
/// Tool calls, tool results, thinking and images are left out.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| matches!(b["type"].as_str(), Some("text" | "Text")))
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// Harness-injected text that only looks like a user message.
fn injected(text: &str) -> bool {
    let t = text.trim_start();
    [
        "<command-",
        "<local-command",
        "<system-reminder>",
        "<task-notification>",
        "<environment_context>",
        "<user_instructions>",
        "Caveat: The messages below",
    ]
    .iter()
    .any(|p| t.starts_with(p))
}

fn truncate(mut s: String) -> String {
    if let Some((i, _)) = s.char_indices().nth(MAX_TEXT) {
        s.truncate(i);
    }
    s
}

fn first_line(s: &str) -> String {
    let line = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    match line.char_indices().nth(100) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_owned(),
    }
}

struct Builder {
    session: Session,
    seen: HashSet<(String, String)>,
    title: Option<String>,
}

impl Builder {
    fn new(id: String) -> Self {
        Builder {
            session: Session {
                id,
                ..Session::default()
            },
            seen: HashSet::new(),
            title: None,
        }
    }

    fn time(&mut self, t: Option<i64>) {
        if let Some(t) = t {
            let s = &mut self.session;
            if s.started == 0 || t < s.started {
                s.started = t;
            }
            s.updated = s.updated.max(t);
        }
    }

    fn push(&mut self, role: &str, ts: Option<i64>, text: String) {
        let text = text.trim().to_owned();
        if text.is_empty() || (role == "user" && injected(&text)) {
            return;
        }
        // Codex logs the same message as an event and as an item.
        if !self.seen.insert((role.to_owned(), text.clone())) {
            return;
        }
        self.time(ts);
        self.session.messages.push(Message {
            role: role.into(),
            ts: ts.unwrap_or(self.session.updated),
            text: truncate(text),
        });
    }

    fn finish(mut self) -> Option<Session> {
        if self.session.messages.is_empty() || self.session.id.is_empty() {
            return None;
        }
        let first_user = self
            .session
            .messages
            .iter()
            .find(|m| m.role == "user")
            .map(|m| first_line(&m.text));
        self.session.title = self.title.or(first_user).unwrap_or_default();
        Some(self.session)
    }
}

pub fn parse(agent: Agent, path: &Path, log: &str) -> Option<Session> {
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let lines = log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok());
    match agent {
        Agent::Claude => {
            let mut b = Builder::new(stem);
            let mut custom = None;
            for v in lines {
                match v["type"].as_str() {
                    Some("custom-title") => custom = v["customTitle"].as_str().map(String::from),
                    Some("ai-title") => b.title = v["aiTitle"].as_str().map(String::from),
                    Some(role @ ("user" | "assistant")) => {
                        if v["isSidechain"] == true || v["isMeta"] == true {
                            continue;
                        }
                        if b.session.cwd.is_empty() {
                            b.session.cwd = v["cwd"].as_str().unwrap_or_default().into();
                        }
                        b.push(
                            role,
                            ts(&v["timestamp"]),
                            content_text(&v["message"]["content"]),
                        );
                    }
                    _ => {}
                }
            }
            if custom.is_some() {
                b.title = custom;
            }
            b.finish()
        }
        Agent::Codex => {
            let mut b = Builder::new(String::new());
            for v in lines {
                let p = &v["payload"];
                let t = ts(&v["timestamp"]);
                match (v["type"].as_str(), p["type"].as_str()) {
                    (Some("session_meta"), _) => {
                        b.session.id = p["id"].as_str().unwrap_or_default().into();
                        b.session.cwd = p["cwd"].as_str().unwrap_or_default().into();
                        b.time(ts(&p["timestamp"]).or(t));
                    }
                    (Some("event_msg"), Some("user_message")) => {
                        b.push("user", t, p["message"].as_str().unwrap_or_default().into())
                    }
                    (Some("event_msg"), Some("agent_message")) => b.push(
                        "assistant",
                        t,
                        p["message"].as_str().unwrap_or_default().into(),
                    ),
                    (Some("event_msg"), Some("item_completed")) => {
                        let item = &p["item"];
                        let role = match item["type"].as_str() {
                            Some("UserMessage") => "user",
                            Some("AgentMessage") => "assistant",
                            _ => continue,
                        };
                        b.push(role, t, content_text(&item["content"]));
                    }
                    _ => {}
                }
            }
            b.finish()
        }
        Agent::Pi => {
            let mut b = Builder::new(String::new());
            for v in lines {
                match v["type"].as_str() {
                    Some("session") => {
                        b.session.id = v["id"].as_str().unwrap_or_default().into();
                        b.session.cwd = v["cwd"].as_str().unwrap_or_default().into();
                        b.time(ts(&v["timestamp"]));
                    }
                    Some("session_info") => {
                        b.title = v["name"].as_str().map(String::from).or(b.title.take())
                    }
                    Some("message") => {
                        let role = match v["message"]["role"].as_str() {
                            Some(r @ ("user" | "assistant")) => r,
                            _ => continue,
                        };
                        b.push(
                            role,
                            ts(&v["timestamp"]),
                            content_text(&v["message"]["content"]),
                        );
                    }
                    _ => {}
                }
            }
            b.finish()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_skips_meta_sidechains_and_tools() {
        let log = r#"{"type":"user","cwd":"/r","timestamp":"2026-01-01T10:00:00Z","message":{"content":"fix the build"}}
{"type":"user","isMeta":true,"message":{"content":"reminder"}}
{"type":"assistant","timestamp":"2026-01-01T10:01:00Z","message":{"content":[{"type":"text","text":"Done"},{"type":"tool_use"}]}}
{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}
{"type":"user","isSidechain":true,"message":{"content":"sub"}}
{"type":"ai-title","aiTitle":"Build fix"}"#;
        let s = parse(Agent::Claude, Path::new("/x/abc.jsonl"), log).unwrap();
        assert_eq!(
            (s.id.as_str(), s.cwd.as_str(), s.title.as_str()),
            ("abc", "/r", "Build fix")
        );
        let texts: Vec<_> = s.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["fix the build", "Done"]);
        assert_eq!(s.updated - s.started, 60);
    }

    #[test]
    fn codex_dedupes_events_and_items() {
        let log = r#"{"type":"session_meta","timestamp":"2026-01-01T10:00:00Z","payload":{"id":"t1","cwd":"/r"}}
{"type":"event_msg","timestamp":"2026-01-01T10:00:01Z","payload":{"type":"user_message","message":"hello"}}
{"type":"event_msg","timestamp":"2026-01-01T10:00:01Z","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"hello"}]}}}
{"type":"event_msg","timestamp":"2026-01-01T10:00:05Z","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"Text","text":"hi"}]}}}"#;
        let s = parse(Agent::Codex, Path::new("/x/rollout-1-t1.jsonl"), log).unwrap();
        assert_eq!(s.id, "t1");
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.title, "hello");
    }

    #[test]
    fn pi_reads_text_blocks() {
        let log = r#"{"type":"session","id":"p1","cwd":"/r","timestamp":"2026-01-01T10:00:00Z"}
{"type":"message","timestamp":"2026-01-01T10:00:01Z","message":{"role":"user","content":[{"type":"text","text":"q"}]}}
{"type":"message","message":{"role":"toolResult","content":[{"type":"text","text":"out"}]}}
{"type":"message","message":{"role":"assistant","content":[{"type":"thinking"},{"type":"text","text":"a"}]}}"#;
        let s = parse(Agent::Pi, Path::new("/x/1_p1.jsonl"), log).unwrap();
        let texts: Vec<_> = s.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!((s.id.as_str(), texts), ("p1", vec!["q", "a"]));
    }
}
