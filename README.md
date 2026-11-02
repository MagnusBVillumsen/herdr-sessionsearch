# herdr-sessionsearch

A [Herdr](https://herdr.dev) popup that searches every Claude Code, Codex and
pi session on all your machines, and resumes the one you pick in a split
next to you, on the machine it belongs to.

```
 Search › forgejo actions                              412 sessions · heimdall ✓ · odin offline

 ▸ thor     claude    1d  ~/dev/Mono                    Forgejo Actions-fane på PR'er
            …the runner registers with Forgejo Actions and picks up…
   odin     codex     7d  ~/src/nixos                   CI for the nixos repo
            …PR points at `ts/ci/forgejo-actions`, where #164 landed…

 ↑↓ choose · enter resume in a split · m:NAME a:AGENT filter · esc close
```

- **Full-text search** of what you and the agents wrote (not tool output),
  with SQLite FTS5. Every word must match, as a prefix. `m:odin` and
  `a:codex` filter by machine and agent. An empty search lists the latest
  sessions.
- **Every machine**, clearly labelled. Each machine indexes its own logs;
  machines pull each other's index over SSH. A machine that is asleep or
  offline is skipped, and its sessions stay searchable from the last pull.
- **Resume where it lives.** Enter splits the pane you came from and runs
  `claude --resume`, `codex resume` or `pi --session` in the session's
  directory. A session on another machine runs there over `ssh -t`, or, with
  `remote = "machine"`, in a new tab in that machine's Herdr. A session that
  is already open in a pane is focused instead.
- **Wakes sleeping machines** before resuming on them, with
  [herdr-sleep](https://github.com/MagnusBVillumsen/herdr-sleepmanagement)
  or any command you set.

Logs are read from `~/.claude/projects`, `~/.codex/sessions` and
`~/.pi/agent/sessions` (`CLAUDE_CONFIG_DIR`, `CODEX_HOME` and
`PI_CODING_AGENT_DIR` are honoured). Claude subagent logs are left out.

## Install

Install it on every machine whose sessions you want to find, so the others
can pull from it.

As a Herdr plugin:

```sh
herdr plugin install MagnusBVillumsen/herdr-sessionsearch
```

and bind a key in `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "prefix+s"
type = "plugin_action"
action = "herdr-sessionsearch.open"
description = "search sessions"
```

Or install the binary (`cargo install --path .` or the Nix flake) and open it
as a popup:

```toml
[[keys.command]]
key = "prefix+s"
type = "popup"
command = "herdr-sessionsearch"
width = 130
height = 34
```

## Machines and sync

With no config, each machine pulls from every enabled saved Herdr machine
(`herdr machine list`) when the popup opens. That works when the machines
you search from can reach the others.

When some machines sleep, use a **hub**: an always-on machine that pulls from
all of them on a timer, so their sessions are there while they sleep. Point
the other machines at the hub:

```toml
# ~/.config/herdr-sessionsearch/config.toml on the hub
sources = ["thor", "odin", "loke"]
```

```toml
# on every other machine
sources = ["heimdall"]
```

and run `herdr-sessionsearch sync` on the hub regularly, for example with a
systemd user timer:

```ini
# ~/.config/systemd/user/herdr-sessionsearch.service
[Service]
Type=oneshot
ExecStart=%h/.cargo/bin/herdr-sessionsearch sync

# ~/.config/systemd/user/herdr-sessionsearch.timer
[Timer]
OnBootSec=2m
OnUnitActiveSec=10m

[Install]
WantedBy=timers.target
```

Sync is incremental: each pull asks only for what the source indexed since
the last pull. A machine never imports its own sessions back; its logs are
the truth about them. Deleted sessions are dropped locally but not on the
machines that already pulled them.

All settings:

```toml
machine = "thor"            # this machine's name (default: short hostname)
sources = ["heimdall"]      # where to pull from (default: saved Herdr machines)
wake = "herdr-sleep wake {machine}"  # default when herdr-sleep is installed
remote = "split"            # or "machine": open remote sessions in that machine's Herdr
remote_bin = "herdr-sessionsearch"   # the binary on other machines

[machines.loke]             # machines Herdr doesn't know, or overrides
target = "magnus@loke"      # SSH target
herdr = "loke"              # saved Herdr machine label, if it differs
wake = "wake-loke"          # wake command for this machine only
```

SSH runs with `BatchMode=yes`, so keys or an agent must be set up for it.

## Commands

| Command | |
|---|---|
| `herdr-sessionsearch` | the popup |
| `herdr-sessionsearch search WORDS` | print matches |
| `herdr-sessionsearch index` | index this machine's logs |
| `herdr-sessionsearch sync` | index, then pull from every source |
| `herdr-sessionsearch export [--since N]` | JSON lines of sessions indexed after `N` (what sync calls over SSH) |
| `herdr-sessionsearch import` | read those JSON lines from stdin, e.g. to push instead of pull |

The index lives in `$XDG_DATA_HOME/herdr-sessionsearch/index.db`.

## License

MIT or Apache-2.0, at your option.
