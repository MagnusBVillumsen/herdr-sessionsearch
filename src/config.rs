use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// This machine's name in the index; defaults to the short hostname.
    pub machine: Option<String>,
    /// Machines to pull sessions from. Defaults to every enabled saved Herdr
    /// machine; on a peer of a hub, set it to just the hub.
    pub sources: Option<Vec<String>>,
    /// Command that wakes a sleeping machine; `{machine}` is replaced by its
    /// name. Defaults to `herdr-sleep wake {machine}` when that is installed.
    pub wake: Option<String>,
    /// How sessions on other machines open: `split` runs them over SSH in a
    /// split next to you, `machine` opens a tab in that machine's Herdr.
    pub remote: Remote,
    /// The binary on other machines.
    pub remote_bin: Option<String>,
    pub machines: HashMap<String, MachineConfig>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Remote {
    #[default]
    Split,
    Machine,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MachineConfig {
    /// SSH target; defaults to the saved Herdr machine's target.
    pub target: Option<String>,
    /// Saved Herdr machine label, if it differs from the name.
    pub herdr: Option<String>,
    /// Wake command for this machine, overriding the global one.
    pub wake: Option<String>,
}

/// How to reach a machine.
#[derive(Clone, Debug)]
pub struct Reach {
    pub name: String,
    pub target: String,
    pub herdr: Option<String>,
}

pub fn dir(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(default)
    })
}

pub fn herdr() -> Command {
    Command::new(std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into()))
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = dir("XDG_CONFIG_HOME", ".config").join("herdr-sessionsearch/config.toml");
        match std::fs::read_to_string(&path) {
            Ok(raw) => toml::from_str(&raw).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn me(&self) -> String {
        if let Some(m) = &self.machine {
            return m.clone();
        }
        let host = Command::new("uname")
            .arg("-n")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "local".into());
        host.split('.').next().unwrap_or(&host).to_owned()
    }

    pub fn remote_bin(&self) -> &str {
        self.remote_bin.as_deref().unwrap_or("herdr-sessionsearch")
    }

    /// Every machine we know how to reach: saved Herdr machines plus the
    /// ones in the config.
    pub fn reachable(&self) -> Vec<Reach> {
        let saved = herdr()
            .args(["machine", "list", "--json"])
            .output()
            .ok()
            .and_then(|o| serde_json::from_slice::<Vec<Value>>(&o.stdout).ok())
            .unwrap_or_default();
        let mut out: Vec<Reach> = saved
            .iter()
            .filter(|m| m["enabled"] != false)
            .filter_map(|m| {
                let label = m["label"].as_str()?;
                let name = self
                    .machines
                    .iter()
                    .find(|(_, c)| c.herdr.as_deref() == Some(label))
                    .map(|(n, _)| n.clone())
                    .unwrap_or_else(|| label.to_owned());
                Some(Reach {
                    target: self
                        .machines
                        .get(&name)
                        .and_then(|c| c.target.clone())
                        .unwrap_or_else(|| m["target"].as_str().unwrap_or(label).to_owned()),
                    herdr: Some(label.to_owned()),
                    name,
                })
            })
            .collect();
        for (name, c) in &self.machines {
            if out.iter().any(|r| &r.name == name) {
                continue;
            }
            if let Some(target) = &c.target {
                out.push(Reach {
                    name: name.clone(),
                    target: target.clone(),
                    herdr: c.herdr.clone(),
                });
            }
        }
        out
    }

    pub fn sources(&self) -> Vec<Reach> {
        let me = self.me();
        let all = self.reachable();
        match &self.sources {
            Some(names) => names
                .iter()
                .filter(|n| **n != me)
                .map(|n| {
                    all.iter().find(|r| &r.name == n).cloned().unwrap_or(Reach {
                        name: n.clone(),
                        target: n.clone(),
                        herdr: None,
                    })
                })
                .collect(),
            None => all.into_iter().filter(|r| r.name != me).collect(),
        }
    }

    pub fn wake_command(&self, machine: &str) -> Option<String> {
        let template = self
            .machines
            .get(machine)
            .and_then(|c| c.wake.clone())
            .or_else(|| self.wake.clone())
            .or_else(|| on_path("herdr-sleep").then(|| "herdr-sleep wake {machine}".into()))?;
        Some(template.replace("{machine}", &crate::shell::quote(machine)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_peer_config() {
        let c: Config = toml::from_str(
            r#"
sources = ["heimdall"]
wake = "wake {machine}"
remote = "machine"
[machines.thor]
target = "me@thor.lan"
"#,
        )
        .unwrap();
        assert_eq!(c.remote, Remote::Machine);
        assert_eq!(c.wake_command("thor").unwrap(), "wake 'thor'");
        assert_eq!(c.machines["thor"].target.as_deref(), Some("me@thor.lan"));
    }
}
