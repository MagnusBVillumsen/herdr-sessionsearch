use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::config::{self, Config, Remote};
use crate::index::Hit;
use crate::parse::Agent;
use crate::shell::quote;
use crate::sync;

pub fn resume(agent: Agent, id: &str) -> String {
    let id = quote(id);
    match agent {
        Agent::Claude => format!("claude --resume {id}"),
        Agent::Codex => format!("codex resume {id}"),
        Agent::Pi => format!("pi --session {id}"),
    }
}

fn call(machine: Option<&str>, args: &[&str]) -> Result<Value> {
    let mut cmd = config::herdr();
    if let Some(m) = machine {
        cmd.args(["--machine", m]);
    }
    let out = cmd.args(args).output().context("running herdr")?;
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    if let Some(e) = v.get("error") {
        bail!(
            "herdr {}: {}",
            args.join(" "),
            e["message"].as_str().unwrap_or("failed")
        );
    }
    if !out.status.success() {
        bail!(
            "herdr {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(v.get("result").cloned().unwrap_or(v))
}

fn id(v: &Value) -> Result<String> {
    v.as_str()
        .map(String::from)
        .with_context(|| format!("herdr returned no pane: {v}"))
}

/// The pane the popup was opened over.
fn focused_pane() -> Result<String> {
    if let Ok(ctx) = std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        && let Ok(v) = serde_json::from_str::<Value>(&ctx)
        && let Some(p) = v["focused_pane_id"].as_str()
    {
        return Ok(p.to_owned());
    }
    if let Ok(p) = std::env::var("HERDR_PANE_ID") {
        return Ok(p);
    }
    let ws = call(None, &["workspace", "list"])?;
    let ws = ws["workspaces"]
        .as_array()
        .and_then(|w| w.iter().find(|w| w["focused"] == true))
        .and_then(|w| w["workspace_id"].as_str())
        .context("no focused workspace")?
        .to_owned();
    let panes = call(None, &["pane", "list", "--workspace", &ws])?;
    panes["panes"]
        .as_array()
        .and_then(|p| p.iter().find(|p| p["focused"] == true))
        .map(|p| id(&p["pane_id"]))
        .context("no focused pane")?
}

/// The pane already running this session, if any.
fn running(machine: Option<&str>, session: &str) -> Option<String> {
    let panes = call(machine, &["pane", "list"]).ok()?;
    panes["panes"]
        .as_array()?
        .iter()
        .find(|p| p["agent_session"]["value"] == session)
        .and_then(|p| p["pane_id"].as_str().map(String::from))
}

fn split_and_run(cwd: Option<&str>, line: &str) -> Result<()> {
    let caller = focused_pane()?;
    let mut args = vec!["pane", "split", &caller, "--direction", "right", "--focus"];
    if let Some(cwd) = cwd {
        args.extend(["--cwd", cwd]);
    }
    let r = call(None, &args)?;
    let pane = id(&r["pane"]["pane_id"])?;
    call(None, &["pane", "run", &pane, line])?;
    Ok(())
}

pub enum Step {
    Waking(String),
}

/// Opens `hit` next to the focused pane, waking its machine first when it
/// sleeps. `progress` hears about slow steps.
pub fn open(cfg: &Config, me: &str, hit: &Hit, mut progress: impl FnMut(Step)) -> Result<String> {
    let cmd = resume(hit.agent, &hit.id);
    let cwd = (!hit.cwd.is_empty()).then_some(hit.cwd.as_str());
    if hit.machine == me {
        if let Some(pane) = running(None, &hit.id) {
            call(None, &["agent", "focus", &pane])?;
            return Ok("already open; focused it".into());
        }
        let dir = cwd.filter(|d| std::path::Path::new(d).is_dir());
        split_and_run(dir, &format!("exec {cmd}"))?;
        return Ok(format!("opened {}", hit.id));
    }

    let reach = cfg
        .reachable()
        .into_iter()
        .find(|r| r.name == hit.machine)
        .with_context(|| {
            format!(
                "don't know how to reach {}: add it with `herdr machine add` or [machines.{}]",
                hit.machine, hit.machine
            )
        })?;
    if !sync::reachable(&reach.target) {
        let wake = cfg
            .wake_command(&hit.machine)
            .with_context(|| format!("{} is offline and no wake command is set", hit.machine))?;
        progress(Step::Waking(hit.machine.clone()));
        let out = Command::new("sh")
            .args(["-c", &wake])
            .output()
            .context("running the wake command")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!(
                "waking {} failed: {}",
                hit.machine,
                err.trim().lines().last().unwrap_or("no output")
            );
        }
        if !sync::reachable(&reach.target) {
            bail!(
                "{} woke but does not answer on ssh yet; try again",
                hit.machine
            );
        }
    }

    let remote_line = match cwd {
        Some(d) => format!("cd {} && exec {cmd}", quote(d)),
        None => format!("exec {cmd}"),
    };
    match (cfg.remote, reach.herdr.as_deref()) {
        (Remote::Machine, Some(label)) => {
            if let Some(pane) = running(Some(label), &hit.id) {
                call(Some(label), &["agent", "focus", &pane])?;
                return Ok(format!("already open on {label}; switch to it"));
            }
            let ws = call(Some(label), &["workspace", "list"])?;
            let ws = ws["workspaces"]
                .as_array()
                .and_then(|w| w.iter().find(|w| w["focused"] == true).or(w.first()))
                .and_then(|w| w["workspace_id"].as_str())
                .with_context(|| format!("{label} has no workspace"))?
                .to_owned();
            let mut args = vec![
                "tab",
                "create",
                "--workspace",
                &ws,
                "--focus",
                "--label",
                &hit.title,
            ];
            if let Some(d) = cwd {
                args.extend(["--cwd", d]);
            }
            let r = call(Some(label), &args)?;
            let pane = id(&r["root_pane"]["pane_id"])?;
            call(Some(label), &["pane", "run", &pane, &format!("exec {cmd}")])?;
            Ok(format!("opened in a new tab on {label}; switch to it"))
        }
        _ => {
            let line = format!(
                "exec ssh -t {} {}",
                quote(&reach.target),
                quote(&remote_line)
            );
            split_and_run(None, &line)?;
            Ok(format!("opened {} on {}", hit.id, hit.machine))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_commands_quote_ids() {
        assert_eq!(resume(Agent::Codex, "a-1"), "codex resume 'a-1'");
        assert_eq!(resume(Agent::Pi, "p"), "pi --session 'p'");
    }
}
