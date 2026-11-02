use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::config::{Config, Reach};
use crate::index::{Index, Record};
use crate::shell::quote;

pub fn ssh(target: &str) -> Command {
    let mut c = Command::new("ssh");
    c.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=4", target]);
    c
}

pub fn reachable(target: &str) -> bool {
    ssh(target)
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Reads JSON lines of records.
pub fn records(input: impl Read) -> impl Iterator<Item = Result<Record>> {
    BufReader::new(input)
        .lines()
        .filter(|l| l.as_ref().map_or(true, |l| !l.trim().is_empty()))
        .map(|l| Ok(serde_json::from_str(&l?)?))
}

/// Pulls the sessions `source` indexed since the last pull. Returns how
/// many were new or changed.
pub fn pull(ix: &mut Index, cfg: &Config, me: &str, source: &Reach) -> Result<usize> {
    let since = ix.synced(&source.name)?;
    let line = format!("{} export --since {since}", quote(cfg.remote_bin()));
    let mut child = ssh(&source.target)
        .arg(line)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running ssh")?;
    let out = child.stdout.take().context("ssh stdout")?;
    let result = ix.import(me, records(out));
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let status = child.wait()?;
    if !status.success() {
        let err = err.trim();
        match status.code() {
            Some(255) => bail!("offline"),
            Some(127) => bail!("{} is not installed there", cfg.remote_bin()),
            _ if err.is_empty() => bail!("export failed ({status})"),
            _ => bail!("{}", err.lines().last().unwrap_or(err)),
        }
    }
    let (n, last) = result?;
    if last > 0 {
        ix.set_synced(&source.name, last)?;
    }
    Ok(n)
}

/// Sends this machine's sessions indexed since the last push to `hub`.
/// Returns how many were sent.
pub fn push(ix: &Index, cfg: &Config, me: &str, hub: &Reach) -> Result<usize> {
    let key = format!("push:{}", hub.name);
    let since = ix.synced(&key)?;
    let line = format!("{} import", quote(cfg.remote_bin()));
    let mut child = ssh(&hub.target)
        .arg(line)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running ssh")?;
    let (mut n, mut last) = (0, 0);
    let sent = {
        let mut input = BufWriter::new(child.stdin.take().context("ssh stdin")?);
        let r = ix.export(since, |r| {
            if r.machine == me {
                last = last.max(r.indexed);
                n += 1;
                serde_json::to_writer(&mut input, &r)?;
                input.write_all(b"\n")?;
            }
            Ok(())
        });
        r.and_then(|_| Ok(input.flush()?))
    };
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let status = child.wait()?;
    if !status.success() {
        match status.code() {
            Some(255) => bail!("offline"),
            Some(127) => bail!("{} is not installed there", cfg.remote_bin()),
            _ => bail!(
                "import failed: {}",
                err.trim().lines().last().unwrap_or("no output")
            ),
        }
    }
    sent?;
    if last > 0 {
        ix.set_synced(&key, last)?;
    }
    Ok(n)
}
