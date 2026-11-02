use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::parse::{self, Agent, Session};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions(
  rowid INTEGER PRIMARY KEY,
  machine TEXT NOT NULL,
  agent TEXT NOT NULL,
  id TEXT NOT NULL,
  cwd TEXT NOT NULL,
  title TEXT NOT NULL,
  started INTEGER NOT NULL,
  updated INTEGER NOT NULL,
  turns INTEGER NOT NULL,
  indexed INTEGER NOT NULL,
  path TEXT,
  stamp TEXT,
  UNIQUE(machine, agent, id)
);
CREATE INDEX IF NOT EXISTS sessions_path ON sessions(path);
CREATE INDEX IF NOT EXISTS sessions_indexed ON sessions(indexed);
CREATE VIRTUAL TABLE IF NOT EXISTS messages USING fts5(
  text, role UNINDEXED, ts UNINDEXED, session UNINDEXED,
  tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TABLE IF NOT EXISTS sources(name TEXT PRIMARY KEY, synced INTEGER NOT NULL);
";

/// A session as it travels between machines.
#[derive(Serialize, Deserialize)]
pub struct Record {
    pub machine: String,
    pub agent: Agent,
    pub indexed: i64,
    pub session: Session,
}

#[derive(Clone)]
pub struct Hit {
    pub machine: String,
    pub agent: Agent,
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub updated: i64,
    pub messages: i64,
    /// The best matching text, with matches between `\x01` and `\x02`.
    pub snippet: Option<String>,
}

pub struct Index {
    db: Connection,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&path, out),
            Ok(t) if t.is_file() => out.push(path),
            _ => {}
        }
    }
}

/// Changes whenever the log is appended to or rewritten.
fn stamp(path: &Path) -> Option<String> {
    let m = std::fs::metadata(path).ok()?;
    let mtime = m
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    Some(format!("{mtime}:{}", m.len()))
}

/// Turns what the user typed into an FTS5 query: every word must appear,
/// as a prefix. Returns `None` when there is nothing to match.
pub fn fts_query(words: &[&str]) -> Option<String> {
    let terms: Vec<String> = words
        .iter()
        .map(|w| w.replace('"', ""))
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .map(|w| format!("\"{w}\"*"))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

impl Index {
    pub fn open() -> Result<Self> {
        let dir = config::dir("XDG_DATA_HOME", ".local/share").join("herdr-sessionsearch");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Self::at(&dir.join("index.db"))
    }

    pub fn at(path: &Path) -> Result<Self> {
        let db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        db.busy_timeout(std::time::Duration::from_secs(10))?;
        db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
        db.execute_batch(SCHEMA)?;
        Ok(Index { db })
    }

    /// Indexes new and changed session logs on this machine and forgets
    /// deleted ones. Returns how many sessions changed.
    pub fn scan(&mut self, me: &str) -> Result<usize> {
        let known: Vec<(String, String)> = {
            let mut q = self.db.prepare(
                "SELECT path, stamp FROM sessions WHERE machine = ?1 AND path IS NOT NULL",
            )?;
            q.query_map([me], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let known: std::collections::HashMap<String, String> = known.into_iter().collect();
        let mut seen = HashSet::new();
        let mut changed = 0;
        let tx = self.db.transaction()?;
        for agent in Agent::ALL {
            let mut files = Vec::new();
            walk(&agent.root(), &mut files);
            for path in files.into_iter().filter(|p| agent.is_log(p)) {
                let key = path.to_string_lossy().into_owned();
                let Some(st) = stamp(&path) else { continue };
                seen.insert(key.clone());
                if known.get(&key) == Some(&st) {
                    continue;
                }
                let Ok(log) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Some(s) = parse::parse(agent, &path, &log) else {
                    continue;
                };
                store_in(&tx, me, agent, &s, Some(&key), Some(&st))?;
                changed += 1;
            }
        }
        for path in known.keys().filter(|p| !seen.contains(*p)) {
            tx.execute(
                "DELETE FROM messages WHERE session IN (SELECT rowid FROM sessions WHERE machine = ?1 AND path = ?2)",
                params![me, path],
            )?;
            tx.execute(
                "DELETE FROM sessions WHERE machine = ?1 AND path = ?2",
                params![me, path],
            )?;
            changed += 1;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Every session indexed at or after `since`, with its messages.
    pub fn export(&self, since: i64, mut emit: impl FnMut(Record) -> Result<()>) -> Result<()> {
        let mut q = self.db.prepare(
            "SELECT rowid, machine, agent, id, cwd, title, started, updated, indexed
             FROM sessions WHERE indexed >= ?1 ORDER BY indexed",
        )?;
        let mut msgs = self.db.prepare(
            "SELECT role, ts, text FROM messages WHERE session = ?1 AND role != 'title' ORDER BY rowid",
        )?;
        let mut rows = q.query([since])?;
        while let Some(r) = rows.next()? {
            let rowid: i64 = r.get(0)?;
            let agent: String = r.get(2)?;
            let Some(agent) = Agent::from_str(&agent) else {
                continue;
            };
            let messages = msgs
                .query_map([rowid], |m| {
                    Ok(parse::Message {
                        role: m.get(0)?,
                        ts: m.get(1)?,
                        text: m.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            emit(Record {
                machine: r.get(1)?,
                agent,
                indexed: r.get(8)?,
                session: Session {
                    id: r.get(3)?,
                    cwd: r.get(4)?,
                    title: r.get(5)?,
                    started: r.get(6)?,
                    updated: r.get(7)?,
                    messages,
                },
            })?;
        }
        Ok(())
    }

    /// Stores sessions from another machine. Sessions of `me` are skipped:
    /// this machine's own logs are the truth about them.
    pub fn import(
        &mut self,
        me: &str,
        records: impl Iterator<Item = Result<Record>>,
    ) -> Result<(usize, i64)> {
        let tx = self.db.transaction()?;
        let (mut n, mut last) = (0, 0);
        for r in records {
            let r = r?;
            last = last.max(r.indexed);
            if r.machine != me && store_in(&tx, &r.machine, r.agent, &r.session, None, None)? {
                n += 1;
            }
        }
        tx.commit()?;
        Ok((n, last))
    }

    pub fn synced(&self, source: &str) -> Result<i64> {
        Ok(self
            .db
            .query_row(
                "SELECT synced FROM sources WHERE name = ?1",
                [source],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    pub fn set_synced(&self, source: &str, at: i64) -> Result<()> {
        self.db.execute(
            "INSERT INTO sources(name, synced) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET synced = max(synced, excluded.synced)",
            params![source, at],
        )?;
        Ok(())
    }

    /// Sessions matching `words`, best first, or the latest ones when
    /// nothing is typed. `machine:x` and `agent:x` words filter.
    pub fn search(&self, input: &str, limit: usize) -> Result<Vec<Hit>> {
        let mut machine = None;
        let mut agent = None;
        let mut words = Vec::new();
        for w in input.split_whitespace() {
            match w.split_once(':') {
                Some(("m" | "machine", v)) if !v.is_empty() => machine = Some(v),
                Some(("a" | "agent", v)) if !v.is_empty() => agent = Some(v),
                _ => words.push(w),
            }
        }
        let filter =
            "(?2 IS NULL OR s.machine LIKE ?2 || '%') AND (?3 IS NULL OR s.agent LIKE ?3 || '%')";
        let row = |r: &rusqlite::Row| -> rusqlite::Result<(Hit, String)> {
            let agent: String = r.get(1)?;
            Ok((
                Hit {
                    machine: r.get(0)?,
                    agent: Agent::from_str(&agent).unwrap_or(Agent::Claude),
                    id: r.get(2)?,
                    cwd: r.get(3)?,
                    title: r.get(4)?,
                    updated: r.get(5)?,
                    messages: r.get(6)?,
                    snippet: r.get(7)?,
                },
                agent,
            ))
        };
        let Some(q) = fts_query(&words) else {
            let sql = format!(
                "SELECT s.machine, s.agent, s.id, s.cwd, s.title, s.updated, s.turns, NULL
                 FROM sessions s WHERE ?1 IS NULL AND {filter} ORDER BY s.updated DESC LIMIT ?4"
            );
            let mut st = self.db.prepare(&sql)?;
            return Ok(st
                .query_map(params![None::<String>, machine, agent, limit as i64], row)?
                .filter_map(|r| r.ok().map(|(h, _)| h))
                .collect());
        };
        // FTS5 auxiliary functions do not work with GROUP BY, so take the
        // best messages and keep the first one per session.
        let sql = format!(
            "SELECT s.machine, s.agent, s.id, s.cwd, s.title, s.updated, s.turns,
                    snippet(messages, 0, char(1), char(2), '…', 16)
             FROM messages JOIN sessions s ON s.rowid = messages.session
             WHERE messages MATCH ?1 AND {filter}
             ORDER BY bm25(messages) LIMIT ?4"
        );
        let mut st = self.db.prepare(&sql)?;
        let mut seen = HashSet::new();
        let mut hits = Vec::new();
        for r in st.query_map(params![q, machine, agent, (limit * 20) as i64], row)? {
            let (h, _) = r?;
            if seen.insert((h.machine.clone(), h.agent, h.id.clone())) {
                hits.push(h);
                if hits.len() == limit {
                    break;
                }
            }
        }
        Ok(hits)
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self
            .db
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))?)
    }
}

/// Stores a session, replacing an older copy. Returns false when the
/// stored copy is newer.
fn store_in(
    db: &Connection,
    machine: &str,
    agent: Agent,
    s: &Session,
    path: Option<&str>,
    stamp: Option<&str>,
) -> Result<bool> {
    let old: Option<(i64, i64)> = db
        .query_row(
            "SELECT rowid, updated FROM sessions WHERE machine = ?1 AND agent = ?2 AND id = ?3",
            params![machine, agent.as_str(), s.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((rowid, updated)) = old {
        if path.is_none() && updated >= s.updated {
            return Ok(false);
        }
        db.execute("DELETE FROM messages WHERE session = ?1", [rowid])?;
        db.execute("DELETE FROM sessions WHERE rowid = ?1", [rowid])?;
    }
    db.execute(
        "INSERT INTO sessions(machine, agent, id, cwd, title, started, updated, turns, indexed, path, stamp)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            machine,
            agent.as_str(),
            s.id,
            s.cwd,
            s.title,
            s.started,
            s.updated,
            s.messages.len() as i64,
            now(),
            path,
            stamp
        ],
    )?;
    let rowid = db.last_insert_rowid();
    let mut insert =
        db.prepare_cached("INSERT INTO messages(text, role, ts, session) VALUES (?1, ?2, ?3, ?4)")?;
    insert.execute(params![s.title, "title", s.started, rowid])?;
    for m in &s.messages {
        insert.execute(params![m.text, m.role, m.ts, rowid])?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::Message;

    fn session(id: &str, text: &str, updated: i64) -> Session {
        Session {
            id: id.into(),
            cwd: "/r".into(),
            title: format!("about {id}"),
            started: 1,
            updated,
            messages: vec![Message {
                role: "user".into(),
                ts: 1,
                text: text.into(),
            }],
        }
    }

    #[test]
    fn imports_searches_and_keeps_newer_copies() {
        let dir = std::env::temp_dir().join(format!("hss-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut ix = Index::at(&dir.join("i.db")).unwrap();
        let rec = |m: &str, s: Session| {
            Ok(Record {
                machine: m.into(),
                agent: Agent::Codex,
                indexed: 5,
                session: s,
            })
        };
        let (n, last) = ix
            .import(
                "thor",
                vec![
                    rec("odin", session("a", "the Ødegaard migration", 10)),
                    rec("thor", session("b", "skipped: mine", 10)),
                ]
                .into_iter(),
            )
            .unwrap();
        assert_eq!((n, last), (1, 5));
        let (n, _) = ix
            .import(
                "thor",
                vec![rec("odin", session("a", "stale", 9))].into_iter(),
            )
            .unwrap();
        assert_eq!(n, 0);

        let hits = ix.search("ØDEG migr", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.as_deref().unwrap().contains('\u{1}'));
        assert_eq!(ix.search("m:thor migration", 10).unwrap().len(), 0);
        assert_eq!(ix.search("", 10).unwrap().len(), 1);

        let mut out = Vec::new();
        ix.export(0, |r| {
            out.push(r);
            Ok(())
        })
        .unwrap();
        assert_eq!(out[0].session.messages.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fts_query_quotes_words() {
        assert_eq!(
            fts_query(&["foo", "a\"b", "--"]).as_deref(),
            Some("\"foo\"* \"ab\"*")
        );
        assert_eq!(fts_query(&[]), None);
    }
}
