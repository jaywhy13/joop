# joop

joop watches a GitHub repository for issues labelled `loop`, and hands each one to a
[Pi](https://github.com/badlogic/pi-mono) agent running in its own [Zellij](https://zellij.dev)
tab. It then supervises those agents: it expects them to check in, replaces the ones that go
quiet, and stands down when your Claude usage limits run out.

```
┌─ joop ───────────────────────────────────────────────────────────────────────┐
│ jaywhy13/joop  label:loop  watching · Claude session 15%  last poll: 20s ago │
└──────────────────────────────────────────────────────────────────────────────┘
┌─ sessions (2) ───────────────────────────────────────────────────────────────┐
│ issue   title                     status    tab          last update   due   │
│ #42     Add a --dry-run flag      working   issue-42 (3) 4m ago        in 16m│
│ #7      Fix the poll interval     stale     issue-7 (4)  22m ago       in 0s │
└──────────────────────────────────────────────────────────────────────────────┘
```

## How it works

1. **Poll.** Every 10 minutes (or when you press `p`), joop runs `gh issue list` for open
   issues carrying the label.
2. **Isolate.** Each issue gets its own `git worktree` on a `joop/issue-<n>` branch, so two
   agents never share a working tree.
3. **Dispatch.** joop opens a Zellij tab named `issue-<n>` in the `joop` session and starts
   `pi` there with a prompt that invokes `/implement` on the issue.
4. **Supervise.** The prompt tells the agent to report back. joop tracks those reports and
   acts when they stop coming.
5. **Stand down.** When your Claude usage nears its limit, joop stops dispatching. When the
   limit resets, it resumes *and types a message into every running tab* telling the agent to
   carry on — a paused agent will not restart on its own.

## Getting started

Requirements: Rust, [`gh`](https://cli.github.com) (authenticated), `git`, `zellij`, and `pi`.

```sh
cargo install --path .

# joop asks which repository to watch, or takes it from the current checkout
joop

# or say so up front
joop --repo jaywhy13/joop --repo-path ~/code/joop
```

`joop` draws a dashboard. `joop run --headless` logs to stderr instead, for running under a
process supervisor.

### Keys

| Key | Does |
| --- | --- |
| `p` / `r` | Poll GitHub now, without waiting for the timer |
| `l` | Re-read Claude usage limits now |
| `↑` `↓` / `j` `k` | Move between sessions |
| `o` / `Enter` | Jump to the selected agent's Zellij tab |
| `q` / `Esc` | Quit (the agents keep running) |

## The reporting contract

Each agent is told, in its opening prompt, that reporting is mandatory:

| Deadline | Default | What happens if it is missed |
| --- | --- | --- |
| First report after startup | 2 minutes | Marked `stale` on the dashboard |
| Between later reports | 20 minutes | Marked `stale` on the dashboard |
| Total silence | 30 minutes | Tab is closed and the issue goes to a fresh agent |

Agents report with the CLI:

```sh
joop report --issue 42 --status working --detail "writing the parser tests"
```

Statuses an agent may send: `started`, `working`, `blocked`, `review`, `done`, `failed`.
joop sets `dispatched`, `stale` and `dead` itself, and rejects an agent that tries to claim
one of those.

A replacement agent is told it is attempt *n* and that the worktree may already hold partial
work, so it reads `git status` and builds on what the dead agent left rather than starting over.

### HTTP API

The CLI is a thin wrapper over a localhost-only HTTP API, which agents can also call directly:

| Route | Does |
| --- | --- |
| `POST /report` | `{"issue": 42, "status": "working", "detail": "..."}` |
| `GET /sessions` | Every session joop knows about |
| `GET /health` | Liveness |

## Claude usage limits

joop reads the same usage figures Claude Code's `/usage` shows, using the OAuth credentials in
`~/.claude/.credentials.json`. When the tightest limit passes `--limit-threshold` (90% by
default), joop stops starting new work and shows when the limit resets.

> That usage endpoint is internal to Claude Code and undocumented. joop treats a reading it
> cannot take as *unknown* rather than *exhausted*, so a change there degrades to "keep
> working", never to a silent stall. Pass `--no-limit-checks` to skip it entirely.

## State

joop keeps everything in `state.json` (`$XDG_STATE_HOME/joop/state.json` by default), so
restarting it does not re-dispatch work already underway. `joop status` prints the same table
without starting the dashboard. In dashboard mode, logs go to `joop.log` next to the state file.

## Configuration

| Flag | Default | Meaning |
| --- | --- | --- |
| `--repo` | asked for | Repository to watch, `owner/name` |
| `--repo-path` | `.` | Checkout the worktrees are cut from |
| `--label` | `loop` | Label that marks an issue as joop's |
| `--session` | `joop` | Zellij session the tabs open in |
| `--worktree-root` | `<repo>-joop-worktrees` | Where worktrees are created |
| `--poll-interval` | `10` | Minutes between automatic polls |
| `--first-update` | `2` | Minutes allowed before the first report |
| `--update-interval` | `20` | Minutes allowed between later reports |
| `--dead-after` | `30` | Minutes of silence before an agent is replaced |
| `--limit-threshold` | `0.9` | Claude usage fraction at which joop stands down |
| `--api-port` | `7777` | Port the reporting API listens on |
| `--skill` | `/implement` | Skill the agent drives the work with |
| `--agent` | `pi` | Agent binary run in each tab |

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

Every external tool sits behind a trait over a `CommandRunner`, so the tests assert on the
exact `gh`, `git` and `zellij` command lines joop would run without running them. Time is
injected through a `Clock`, so the heartbeat and reassignment rules are tested without sleeping.
