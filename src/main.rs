mod config;
mod index;
mod open;
mod parse;
mod shell;
mod sync;

use std::io::{Write, stdout};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{Local, TimeZone};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::{cursor, execute, queue, terminal};

use config::Config;
use index::{Hit, Index};

const USAGE: &str = "\
Search Claude Code, Codex and pi sessions on all your machines.

Usage:
  herdr-sessionsearch                 open the search popup
  herdr-sessionsearch search WORDS    print matching sessions
  herdr-sessionsearch index           index this machine's sessions
  herdr-sessionsearch sync            index, push to the hub, pull from the sources
  herdr-sessionsearch export [--since N]
                                      print sessions as JSON lines (for sync)
  herdr-sessionsearch import          read JSON lines from stdin

In the search, `m:NAME` and `a:AGENT` filter by machine and agent.";

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn ago(ts: i64) -> String {
    let d = (now() - ts).max(0);
    match d {
        d if d < 3600 => format!("{}m", d / 60),
        d if d < 86400 => format!("{}h", d / 3600),
        d if d < 30 * 86400 => format!("{}d", d / 86400),
        _ => Local
            .timestamp_opt(ts, 0)
            .single()
            .map(|t| t.format("%-d %b").to_string())
            .unwrap_or_default(),
    }
}

fn home_short(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && path.starts_with(&h) => format!("~{}", &path[h.len()..]),
        _ => path.to_owned(),
    }
}

fn clip(s: &str, width: usize) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_control() && c != '\u{1}' && c != '\u{2}' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let visible = |s: &str| s.chars().filter(|c| *c != '\u{1}' && *c != '\u{2}').count();
    if visible(&s) <= width {
        return s;
    }
    let mut out = String::new();
    let mut n = 0;
    for c in s.chars() {
        if c != '\u{1}' && c != '\u{2}' {
            if n + 1 >= width {
                out.push('…');
                break;
            }
            n += 1;
        }
        out.push(c);
    }
    out
}

/// Pads to `width` visible characters.
fn pad(s: &str, width: usize) -> String {
    let s = clip(s, width);
    let n = s.chars().count();
    format!("{s}{}", " ".repeat(width.saturating_sub(n)))
}

fn agent_color(a: parse::Agent) -> Color {
    match a {
        parse::Agent::Claude => Color::Rgb {
            r: 217,
            g: 119,
            b: 87,
        },
        parse::Agent::Codex => Color::Cyan,
        parse::Agent::Pi => Color::Magenta,
    }
}

fn print_hits(hits: &[Hit]) -> std::io::Result<()> {
    let mut out = stdout().lock();
    for h in hits {
        writeln!(
            out,
            "{:<10} {:<6} {:>5}  {}  {}  {}",
            h.machine,
            h.agent.as_str(),
            ago(h.updated),
            h.id,
            home_short(&h.cwd),
            h.title
        )?;
        if let Some(s) = &h.snippet {
            writeln!(out, "    {}", clip(&s.replace(['\u{1}', '\u{2}'], ""), 120))?;
        }
    }
    Ok(())
}

/// Indexes locally and pulls every source in parallel, reporting each
/// source's outcome as it lands.
fn refresh(
    cfg: &Config,
    me: &str,
    indexed: impl FnOnce(usize),
    report: impl Fn(&str, Result<usize>) + Sync,
) -> Result<usize> {
    let n = Index::open()?.scan(me)?;
    indexed(n);
    let sources = cfg.sources();
    let hub = cfg.push_to();
    thread::scope(|s| {
        if let Some(hub) = &hub {
            let report = &report;
            s.spawn(move || {
                let r = Index::open().and_then(|ix| sync::push(&ix, cfg, me, hub));
                report(&format!("→{}", hub.name), r);
            });
        }
        for src in &sources {
            let report = &report;
            s.spawn(move || {
                let r = Index::open().and_then(|mut ix| sync::pull(&mut ix, cfg, me, src));
                report(&src.name, r);
            });
        }
    });
    Ok(n)
}

#[derive(Default)]
struct State {
    sources: Vec<(String, Option<Result<usize, String>>)>,
    indexed: bool,
    dirty: bool,
}

struct Screen;

impl Screen {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        Ok(Screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(stdout(), cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

struct Ui {
    query: String,
    hits: Vec<Hit>,
    sel: usize,
    top: usize,
    message: Option<(Color, String)>,
}

fn segment(out: &mut impl Write, color: Option<Color>, text: &str) -> Result<()> {
    match color {
        Some(c) => queue!(out, SetForegroundColor(c), Print(text), ResetColor)?,
        None => queue!(out, Print(text))?,
    }
    Ok(())
}

/// Prints a snippet with matches highlighted.
fn snippet(out: &mut impl Write, s: &str) -> Result<()> {
    let mut on = false;
    for part in s.split_inclusive(['\u{1}', '\u{2}']) {
        let (text, mark) = match part.chars().last() {
            Some(c @ ('\u{1}' | '\u{2}')) => (&part[..part.len() - 1], Some(c)),
            _ => (part, None),
        };
        segment(
            out,
            Some(if on { Color::Yellow } else { Color::DarkGrey }),
            text,
        )?;
        if let Some(m) = mark {
            on = m == '\u{1}';
        }
    }
    Ok(())
}

fn draw(out: &mut impl Write, ui: &mut Ui, state: &State, total: i64) -> Result<()> {
    let (w, h) = terminal::size()?;
    let (w, h) = (w as usize, h as usize);
    queue!(
        out,
        cursor::MoveTo(0, 0),
        terminal::Clear(terminal::ClearType::All)
    )?;

    let mut status: Vec<(Color, String)> = vec![(Color::DarkGrey, format!("{total} sessions"))];
    if !state.indexed {
        status.push((Color::DarkGrey, " · indexing…".into()));
    }
    for (name, r) in &state.sources {
        if name.starts_with('→') && !matches!(r, Some(Err(e)) if e != "offline") {
            continue;
        }
        let (c, s) = match r {
            None => (Color::DarkGrey, format!(" · {name} …")),
            Some(Ok(_)) => (Color::Green, format!(" · {name} ✓")),
            Some(Err(e)) if e == "offline" => (Color::DarkGrey, format!(" · {name} offline")),
            Some(Err(_)) => (Color::Yellow, format!(" · {name} !")),
        };
        status.push((c, s));
    }
    let status_len: usize = status.iter().map(|(_, s)| s.chars().count()).sum();
    let prompt = format!(" Search › {}", ui.query);
    segment(out, None, &pad(&prompt, w.saturating_sub(status_len + 1)))?;
    queue!(
        out,
        SetAttribute(Attribute::Reverse),
        Print(" "),
        SetAttribute(Attribute::Reset)
    )?;
    for (c, s) in &status {
        segment(out, Some(*c), s)?;
    }
    queue!(out, Print("\r\n\r\n"))?;

    let rows = h.saturating_sub(4);
    let per = 2;
    let fit = (rows / per).max(1);
    if ui.sel < ui.top {
        ui.top = ui.sel;
    } else if ui.sel >= ui.top + fit {
        ui.top = ui.sel + 1 - fit;
    }
    if ui.hits.is_empty() {
        let msg = if ui.query.trim().is_empty() {
            "No sessions indexed yet."
        } else {
            "No matches."
        };
        segment(out, Some(Color::DarkGrey), &format!("   {msg}\r\n"))?;
    }
    for (i, hit) in ui.hits.iter().enumerate().skip(ui.top).take(fit) {
        let chosen = i == ui.sel;
        segment(out, Some(Color::Cyan), if chosen { " ▸ " } else { "   " })?;
        segment(out, Some(Color::Green), &pad(&hit.machine, 9))?;
        segment(
            out,
            Some(agent_color(hit.agent)),
            &pad(hit.agent.as_str(), 7),
        )?;
        segment(
            out,
            Some(Color::DarkGrey),
            &format!("{:>5}  ", ago(hit.updated)),
        )?;
        let place = pad(&home_short(&hit.cwd), 28);
        segment(out, Some(Color::Blue), &place)?;
        segment(out, Some(Color::DarkGrey), "  ")?;
        let title_w = w.saturating_sub(3 + 9 + 7 + 7 + 28 + 2 + 1);
        if chosen {
            queue!(out, SetAttribute(Attribute::Bold))?;
        }
        segment(out, None, &clip(&hit.title, title_w))?;
        queue!(out, SetAttribute(Attribute::Reset), Print("\r\n"))?;
        let detail = match &hit.snippet {
            Some(s) => s.clone(),
            None => format!("{} messages · {}", hit.messages, hit.id),
        };
        queue!(out, Print(" ".repeat(12)))?;
        snippet(out, &clip(&detail.replace('\n', " "), w.saturating_sub(13)))?;
        queue!(out, Print("\r\n"))?;
    }

    queue!(out, cursor::MoveTo(0, h.saturating_sub(1) as u16))?;
    let problem = state.sources.iter().find_map(|(name, r)| match r {
        Some(Err(e)) if e != "offline" => Some(format!("{name}: {e}")),
        _ => None,
    });
    match (&ui.message, problem) {
        (Some((c, m)), _) => segment(out, Some(*c), &clip(&format!(" {m}"), w))?,
        (None, Some(p)) => segment(out, Some(Color::Yellow), &clip(&format!(" {p}"), w))?,
        (None, None) => segment(
            out,
            Some(Color::DarkGrey),
            &clip(
                " ↑↓ choose · enter resume in a split · m:NAME a:AGENT filter · esc close",
                w,
            ),
        )?,
    }
    out.flush()?;
    Ok(())
}

fn tui(cfg: Config) -> Result<()> {
    let me = cfg.me();
    let ix = Index::open()?;
    let cfg = Arc::new(cfg);
    let state = Arc::new(Mutex::new(State {
        sources: cfg
            .sources()
            .into_iter()
            .map(|s| (s.name, None))
            .chain(cfg.push_to().map(|h| (format!("→{}", h.name), None)))
            .collect(),
        ..State::default()
    }));
    {
        let (cfg, state, me) = (cfg.clone(), state.clone(), me.clone());
        thread::spawn(move || {
            let mark = |name: &str, r: Result<usize>| {
                let mut st = state.lock().unwrap();
                if let Some(slot) = st.sources.iter_mut().find(|(n, _)| n == name) {
                    slot.1 = Some(r.map_err(|e| e.to_string()));
                }
                st.dirty = true;
            };
            let indexed = |_| {
                let mut st = state.lock().unwrap();
                st.indexed = true;
                st.dirty = true;
            };
            let r = refresh(&cfg, &me, indexed, mark);
            if let Err(e) = r {
                let mut st = state.lock().unwrap();
                st.indexed = true;
                st.sources.push(("index".into(), Some(Err(e.to_string()))));
            }
        });
    }

    let mut ui = Ui {
        query: String::new(),
        hits: ix.search("", 200)?,
        sel: 0,
        top: 0,
        message: None,
    };
    let _screen = Screen::enter()?;
    let mut out = stdout();
    let mut changed = false;
    loop {
        {
            let mut st = state.lock().unwrap();
            if st.dirty || changed {
                st.dirty = false;
                drop(st);
                match ix.search(&ui.query, 200) {
                    Ok(h) => ui.hits = h,
                    Err(e) => ui.message = Some((Color::Yellow, e.to_string())),
                }
                ui.sel = ui.sel.min(ui.hits.len().saturating_sub(1));
                changed = false;
            }
        }
        let total = ix.count().unwrap_or(0);
        draw(&mut out, &mut ui, &state.lock().unwrap(), total)?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(k) = event::read()? else {
            continue;
        };
        if k.kind == KeyEventKind::Release {
            continue;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let page = (terminal::size()?.1 as usize).saturating_sub(4) / 2;
        match k.code {
            KeyCode::Esc => break,
            KeyCode::Char('c' | 'q') if ctrl => break,
            KeyCode::Up => ui.sel = ui.sel.saturating_sub(1),
            KeyCode::Char('p' | 'k') if ctrl => ui.sel = ui.sel.saturating_sub(1),
            KeyCode::Down => ui.sel = (ui.sel + 1).min(ui.hits.len().saturating_sub(1)),
            KeyCode::Char('n' | 'j') if ctrl => {
                ui.sel = (ui.sel + 1).min(ui.hits.len().saturating_sub(1))
            }
            KeyCode::PageUp => ui.sel = ui.sel.saturating_sub(page),
            KeyCode::PageDown => ui.sel = (ui.sel + page).min(ui.hits.len().saturating_sub(1)),
            KeyCode::Char('u') if ctrl => {
                ui.query.clear();
                changed = true;
            }
            KeyCode::Char('w') if ctrl => {
                let t = ui.query.trim_end().len();
                ui.query
                    .truncate(ui.query[..t].rfind(' ').map_or(0, |i| i + 1));
                changed = true;
            }
            KeyCode::Backspace => {
                ui.query.pop();
                changed = true;
            }
            KeyCode::Char(c) if !ctrl => {
                ui.query.push(c);
                ui.sel = 0;
                changed = true;
            }
            KeyCode::Enter => {
                let Some(hit) = ui.hits.get(ui.sel).cloned() else {
                    continue;
                };
                ui.message = Some((
                    Color::DarkGrey,
                    format!("opening {} on {}…", hit.id, hit.machine),
                ));
                draw(&mut out, &mut ui, &state.lock().unwrap(), total)?;
                let result = open::open(&cfg, &me, &hit, |open::Step::Waking(m)| {
                    ui.message = Some((Color::Yellow, format!("{m} is asleep; waking it…")));
                    let _ = draw(&mut stdout(), &mut ui, &state.lock().unwrap(), total);
                });
                match result {
                    Ok(_) => break,
                    Err(e) => ui.message = Some((Color::Red, format!("{e:#}"))),
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Run from Herdr's `pane.agent_status_changed` hook: only sync once the
/// agent has stopped, when its session has something new to share.
fn agent_still_working() -> bool {
    if std::env::var("HERDR_PLUGIN_EVENT").as_deref() != Ok("pane.agent_status_changed") {
        return false;
    }
    fn status(v: &serde_json::Value) -> Option<&str> {
        match v {
            serde_json::Value::Object(m) => m
                .get("agent_status")
                .and_then(|s| s.as_str())
                .or_else(|| m.values().find_map(status)),
            _ => None,
        }
    }
    std::env::var("HERDR_PLUGIN_EVENT_JSON")
        .ok()
        .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
        .is_some_and(|v| status(&v) == Some("working"))
}

/// Held while syncing, so the hooks of several agents finishing together
/// do not all sync at once.
fn sync_lock() -> Result<Option<std::fs::File>> {
    let dir = config::dir("XDG_DATA_HOME", ".local/share").join("herdr-sessionsearch");
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::File::create(dir.join("sync.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = Config::load()?;
    let me = cfg.me();
    match args.first().map(String::as_str) {
        None => tui(cfg),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some("index") => {
            let n = Index::open()?.scan(&me)?;
            eprintln!("{n} sessions changed");
            Ok(())
        }
        Some("sync") => {
            if agent_still_working() {
                return Ok(());
            }
            let Some(_lock) = sync_lock()? else {
                eprintln!("another sync is running");
                return Ok(());
            };
            let n = refresh(
                &cfg,
                &me,
                |_| {},
                |name, r| match r {
                    Ok(n) => eprintln!("{name}: {n} sessions"),
                    Err(e) => eprintln!("{name}: {e:#}"),
                },
            )?;
            eprintln!("{me}: {n} sessions changed");
            Ok(())
        }
        Some("export") => {
            let since = match args.get(1).map(String::as_str) {
                Some("--since") => args
                    .get(2)
                    .context("--since needs a number")?
                    .parse()
                    .context("--since")?,
                Some(a) => bail!("unknown argument {a}"),
                None => 0,
            };
            let mut ix = Index::open()?;
            ix.scan(&me)?;
            let mut out = std::io::BufWriter::new(stdout().lock());
            ix.export(since, |r| {
                serde_json::to_writer(&mut out, &r)?;
                out.write_all(b"\n")?;
                Ok(())
            })?;
            out.flush()?;
            Ok(())
        }
        Some("import") => {
            let (n, _) = Index::open()?.import(&me, sync::records(std::io::stdin().lock()))?;
            eprintln!("{n} sessions imported");
            Ok(())
        }
        Some("search") => {
            let mut ix = Index::open()?;
            ix.scan(&me)?;
            match print_hits(&ix.search(&args[1..].join(" "), 50)?) {
                Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(e.into()),
                _ => Ok(()),
            }
        }
        Some(a) => bail!("unknown command {a}\n\n{USAGE}"),
    }
}
