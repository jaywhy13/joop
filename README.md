# joop

joop watches a GitHub repository for issues labelled `loop`, and hands each one to a
`claude` agent running in its own [herdr](https://github.com/badlogic/herdr) tab. It then
supervises those agents: it expects them to check in, shows the pull request each one opens,
replaces the ones that go quiet, closes a tab once its work has landed, and stands down when
your Claude usage limits run out.

```
┌─ joop ──────────────────────────────────────────────────────────────────────────┐
│ jaywhy13/joop  label:loop  watching · Claude session 15%  last poll: 20s ago    │
└─────────────────────────────────────────────────────────────────────────────────┘
┌─ sessions (3) ──────────────────────────────────────────────────────────────────┐
│ issue  title                  status   pull request  tab     last update   due  │
│ #42    Add a --dry-run flag   review   #51           w5:t2   2m ago        in 3m│
│ #7     Fix the poll interval  working  —             w5:t3   4m ago        in 16m│
│ #9     Tidy the event log     stale    #48           w5:t4   22m ago       in 0s│
└─────────────────────────────────────────────────────────────────────────────────┘
```

## How it works

1. **Poll.** Every 10 minutes (or when you press `p`), joop runs `gh issue list` for open
   issues carrying the label.
2. **Isolate.** Each issue gets its own `git worktree` on a `joop/issue-<n>` branch, so two
   agents never share a working tree.
3. **Dispatch.** joop opens a herdr tab labelled `issue-<n>` in its `joop` workspace, starts
   `claude --dangerously-skip-permissions` in it, and hands it a prompt that invokes
   `/implement` on the issue.
4. **Supervise.** The prompt tells the agent to report back. joop tracks those reports and
   acts when they stop coming.
5. **Follow the pull request.** When an agent reports the pull request it opened, joop records
   it, shows it on the dashboard, and pushes it onto the agent's herdr pane — so the issue and
   its pull request read straight off herdr's own UI. The agent then watches that pull request
   for reviews, and joop expects a check-in on that faster rhythm.
6. **Close the tab.** When the agent reports the pull request merged, the work has landed, so
   joop closes the whole herdr tab.
7. **Stand down.** When your Claude usage nears its limit, joop stops dispatching. When the
   limit resets, it resumes *and tells every running agent to carry on* — a paused agent will
   not restart on its own.

## Getting started

Requirements: Rust, [`gh`](https://cli.github.com) (authenticated), `git`, `herdr`, and
`claude`.

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
| `o` / `Enter` | Bring the selected agent's herdr tab to the front |
| `f` | Forget the selected issue, so a later poll hands it out again |
| `q` / `Esc` | Quit (the agents keep running) |

## The reporting contract

Each agent is told, in its opening prompt, that reporting is mandatory:

| Deadline | Default | What happens if it is missed |
| --- | --- | --- |
| First report after startup | 2 minutes | Marked `stale` on the dashboard |
| Between later reports, while working | 20 minutes | Marked `stale` on the dashboard |
| Between later reports, once a pull request is open | 5 minutes | Marked `stale` on the dashboard |
| Total silence | 30 minutes | Tab is closed and the issue goes to a fresh agent |

The 5-minute window is not joop being impatient: the agent is told to check its pull request
for reviews and failing checks every 5 minutes, so each check doubles as its check-in.

An agent whose herdr tab has disappeared is replaced at once rather than waiting out its
heartbeat, since a tab that is gone is an agent that is not coming back. After
`--max-attempts` agents (3 by default) have gone quiet on one issue, joop stops reassigning
it and parks it as `failed` rather than cycling agents through it forever.

Agents report with the CLI:

```sh
joop report --issue 42 --status working --detail "writing the parser tests"
```

Statuses an agent may send: `started`, `working`, `blocked`, `waiting`, `review`, `done`,
`merged`, `failed`. `blocked` means a human is needed; `waiting` means other issues must land
first (see [Blocked issues](#blocked-issues)). joop sets `dispatched`, `stale` and `dead` itself,
and rejects an agent that tries to claim one of those.

### Pull requests

Reporting `review` requires the pull request, as a number or a URL — a review joop was not told
about is nothing it can show:

```sh
joop report --issue 42 --status review \
  --pull-request https://github.com/jaywhy13/joop/pull/51 \
  --detail "opened for review"
```

joop then puts it on the agent's herdr pane, so herdr's own UI shows the issue and its pull
request against that agent:

```text
title:  #42 · PR #51
tokens: issue=42  status=review  pull_request=51  pr_url=https://github.com/.../pull/51
```

An agent keeps listening for feedback on its pull request until it is merged or closed, so a
session does not end while the pull request is still open. Reporting `merged` closes the tab, so
an agent should make that its last action:

```sh
joop report --issue 42 --status merged
```

If the pull request is closed without merging, the agent reports `failed` with the reason.

## Blocked issues

Before an agent starts building, it checks the issue for a "Blocked by" list, and for work it
cannot build without, and looks up each issue it names. If any of them is still open, the agent
reports `waiting`, naming them, and stops:

```sh
joop report --issue 92 --status waiting --blocked-by 87,89 --detail "waiting on #87, #89"
```

joop then closes the tab and holds the issue. It is not handed to a new agent on each poll, so a
held issue costs nothing while it waits. Releasing it is one of two things:

- **A merge that unblocks it.** When an agent's merged work unblocks other issues, it names them,
  and joop starts those straight away:

  ```sh
  joop report --issue 90 --status merged --unblocks 92,93
  ```

- **The hourly check.** Every hour joop asks a Claude model (`claude-haiku-5-5`, run through
  `claude -p`) which held issues are no longer blocked. It gives the model each held issue's
  description and the open or closed state of each blocker. Those the model releases are started.
  The check is skipped while joop is paused on Claude limits, and a failed check leaves every issue
  held until the next hour.

Run `joop forget --issue 92` to hand a held issue out again without waiting for its blockers.

A replacement agent is told it is attempt *n* and that the worktree may already hold partial
work, so it reads `git status` and builds on what the dead agent left rather than starting over.

### HTTP API

The CLI is a thin wrapper over a localhost-only HTTP API, which agents can also call directly:

| Route | Does |
| --- | --- |
| `POST /report` | `{"issue": 42, "status": "review", "detail": "...", "pull_request": "https://.../pull/51"}` |
| `GET /sessions` | Every session joop knows about |
| `GET /health` | Liveness |

## Claude usage limits

joop reads the same usage figures Claude Code's `/usage` shows, using the OAuth credentials in
`~/.claude/.credentials.json`. When the tightest limit passes `--limit-threshold` (90% by
default), joop stops starting new work and shows when the limit resets.

**While paused, the heartbeat clock stops.** The agents are out of capacity too, so holding
them to a 20-minute reporting deadline would declare every one of them dead and close every
tab — leaving nothing to resume. When capacity returns, each running agent is told to carry on
and gets a fresh first-report window before it counts as late.

> That usage endpoint is internal to Claude Code and undocumented. joop treats a reading it
> cannot take as *unknown* rather than *exhausted*, so a change there degrades to "keep
> working", never to a silent stall. Pass `--no-limit-checks` to skip it entirely.

## Each issue is handled once

joop dispatches an issue once. An open issue keeps its `loop` label after the work is done, so
re-dispatching on status alone would open a new tab on every poll, forever. To hand the same
issue out again, press `f` on the dashboard or run:

```sh
joop forget --issue 42
```

## State

joop keeps everything in `state.json` (`$XDG_STATE_HOME/joop/state.json` by default), so
restarting it does not re-dispatch work already underway. A state file written by the Zellij
version of joop is not readable by this one: its sessions point at Zellij tabs that no longer
mean anything, so delete it rather than migrating it. `joop status` prints the same table
without starting the dashboard. In dashboard mode, logs go to `joop.log` next to the state file.

## Configuration

| Flag | Default | Meaning |
| --- | --- | --- |
| `--repo` | asked for | Repository to watch, `owner/name` |
| `--repo-path` | `.` | Checkout the worktrees are cut from |
| `--label` | `loop` | Label that marks an issue as joop's |
| `--workspace` | `joop` | herdr workspace the tabs open in |
| `--worktree-root` | `<repo>-joop-worktrees` | Where worktrees are created |
| `--poll-interval` | `10` | Minutes between automatic polls |
| `--first-update` | `2` | Minutes allowed before the first report |
| `--update-interval` | `20` | Minutes allowed between later reports while working |
| `--review-interval` | `5` | Minutes allowed between reports once a pull request is open |
| `--dead-after` | `30` | Minutes of silence before an agent is replaced |
| `--limit-threshold` | `0.9` | Claude usage fraction at which joop stands down |
| `--max-attempts` | `3` | Agents allowed on one issue before joop parks it |
| `--api-port` | `7777` | Port the reporting API listens on |
| `--skill` | `/implement` | Skill the agent drives the work with |
| `--agent` | `claude` | Agent kind herdr starts in each tab |
| `--agent-arg` | `--dangerously-skip-permissions` | Argument passed to the agent itself; repeatable |

## Which herdr session joop drives

joop shells out to `herdr`, which talks to whichever session `HERDR_SOCKET_PATH` points at,
defaulting to `~/.config/herdr/herdr.sock`. So running joop inside a herdr pane drives that
same session, and running it from an ordinary terminal drives the default one. Point
`HERDR_SOCKET_PATH` at another session's socket (`herdr session list` prints them) to put
joop's tabs somewhere else.

## A word on trust

The label is the only gate. A `loop`-labelled issue's title and body go straight into the
opening prompt of an agent started as `claude --dangerously-skip-permissions`, in a worktree of
your repository, so **anyone who can label an issue in the watched repository can direct an
agent that will not stop to ask**. joop passes the text as a single argument rather than
through a shell, so there is no command injection, but prompt injection is inherent to the
design. Watch repositories whose labels you control, and change `--agent-arg` if you want the
permission prompts back.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

Every external tool sits behind a trait over a `CommandRunner`, so the tests assert on the
exact `gh`, `git` and `herdr` command lines joop would run without running them. Time is
injected through a `Clock`, so the heartbeat and reassignment rules are tested without sleeping.
