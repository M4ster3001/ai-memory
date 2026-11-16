# Agent bus: plug-and-play messaging between the agents of a project

Status: proposal, written 2026-11-15 after the first real run of the wake supervisor. Nothing in this note is built yet. It records what the owner asked for, what the supervisor of #28 gives today, where it falls short, and the design that closes the gap. It extends [design-agent-wakeup-and-secure-messaging.md](design-agent-wakeup-and-secure-messaging.md); where the two disagree, this note wins for what to build next.

## 1. What the owner asked for

1. Plug and play. A project turns messaging on without hand-written configuration.
2. Full duplex between the agents of one project. Any agent can reach any other and be answered the same way.
3. Fast, and no tokens spent while nothing happens. Waiting is free; an agent spends tokens only when it reads and answers a message that arrived.
4. ai-memory is the server and the orchestrator behind all of it. Projects do not run their own watchers.
5. A project talks to another project only when the human user asks for it. When they do, the system opens a connection between exactly the projects they named.
6. Any agent can adopt it by reading the documentation: Claude Code, Codex, Gemini and others. No external services.

## 2. What exists today (slices 1 to 3a, PRs #25 to #28)

| Piece | What it does | Limit found in use |
| --- | --- | --- |
| Long-poll `GET /inbox/watch` and `message watch` | Wake a waiter in milliseconds, no model | Only a waiter that someone started |
| Delivery states and receipts | The sender learns `delayed`, `wake_failed`, `budget_exhausted` | Works well |
| Wake supervisor (`message supervise`) | Runs `codex queue` when mail arrives for a listed agent | Needs a hand-written entry, a copied binary, a thread id and a terminal prompt to approve |
| Same-project rule | Mail inside one project wakes without authorization | Works |

Findings from the first real run on 2026-11-15:

- Setup is not plug and play. A person had to write `config.toml`, copy the binary out of the container, find the Codex thread id, approve in a terminal and keep a terminal tab open. The approval prompt did not receive typed input in the desktop app twice; the person had to run it in another terminal.
- Only Codex can be woken. A stopped Claude Code session is never woken, so replies to it waited for hours.
- The budget of 4 wakes an hour per agent is tight for a working session. Six test messages spent it, and the next real messages were refused until the window rolled. A test needs a throwaway agent, not the one doing the work.
- The Codex desktop app ran the turn opened by `codex queue` but did not show it on screen.
- A sender that is not a hook session (the Claude Code MCP connection sends only the bearer token) is classified `unverified`. The fix in #28 is the same-project rule.
- The server restart during an image swap left the supervisor retrying, which is the right behavior.

## 3. Design

### 3.1 Agents are discovered, not configured

Hooks already report a session when it starts. The server keeps, per session, the harness, the project and a wake handle: the Codex thread id, the Claude Code session id, or nothing when the harness cannot be woken. A new table `agent_wake_handles` stores them and the server refuses to guess: no handle, no wake. The supervisor entry in `config.toml` disappears for the common case.

### 3.2 One daemon per host, installed by one command

`ai-memory agent-bus install` prints what it will do and, with the human's confirmation, installs one per-user service (systemd on Linux, a LaunchAgent on macOS) that runs the supervisor. It reads the registry and the adapter set from the server over the long-poll it already has. It holds the bearer token the human already gave the CLI. No configuration file is required; one file may override budgets.

### 3.3 Approval is a click in the dashboard

The daemon executes programs, so a human must approve each adapter binary once. The approval moves from a terminal `[y/N]` prompt to a button on the dashboard page for the host. The page shows the same facts the prompt shows today: canonical path, SHA-256, argv shape, working directory, environment variable names. The approval is still bound to the binary hash, so an upgraded program asks again.

### 3.4 Delivery paths, by cost

| Situation | Path | Tokens while idle | Latency |
| --- | --- | --- | --- |
| Agent is mid-session | Hook adds a one-line notice at the next prompt, with the count only | 0 | Next prompt |
| Codex session open | `codex queue` into its thread | 0 | About 1 s |
| Claude Code session stopped | Headless resume (`claude -p --resume <session>`) with a fixed prompt and a sandbox | 0 until it runs | Seconds |
| Claude Code session open and idle | Claude channels through `mcp-bridge --channel inbox` | 0 | Immediate; research preview behind a development flag |

Every wake text is fixed and carries no message content. The agent reads messages itself with the existing tools.

### 3.5 Full duplex

A reply is an ordinary `message_send`, which wakes the other side through the same registry. Both ends use one path, so neither is special. The budgets are per agent pair, not per agent, so a long back-and-forth between two agents that the human started does not starve a third.

### 3.6 Cross-project only when the human asks

The human tells any agent or the dashboard "connect project A and B". The server creates a link: two project ids, a creation time, an expiry (default 2 hours, maximum 24), the human who asked, and a reason. While the link is open, mail between those two projects wakes like mail inside one project. Any other project stays closed. The link ends at expiry, when the human closes it, or when either project is archived. Every link, wake and refusal is written to the audit trail and shown on the dashboard. An agent can propose a link; only the authenticated human can open it.

This replaces `wake_from` for the common case. `wake_from` stays as a standing rule for operators who want one.

### 3.7 Rooms

A room is a link with more than two projects and a name, opened the same way, for agents to trade ideas. Multicast goes through a room, never through a standing broadcast. Out of scope for v1.

### 3.8 Dead-letter view

Messages that expired, could not wake or exceeded the budget go to a dead-letter list on the dashboard with retry, inspect and archive. Deletion happens only after a retention period the user sets, as in a queue service. Specified in the main design note; built after the bus.

## 4. Phases

1. `agent_wake_handles` and discovery through hooks, with the daemon reading them instead of a config entry.
2. Dashboard approval and `agent-bus install`.
3. Claude Code adapter: headless resume, then the channel bridge when the owner accepts the research preview.
4. Links and the audit view.
5. Pair budgets and tuned defaults.
6. Rooms and the dead-letter view.

Each phase ships behind its own tests and the project's existing release gates, and keeps the supervisor of #28 working.

## 5. Security notes

- The wake text is fixed. Mail content never enters argv.
- The daemon runs a closed set of adapters, without a shell, with a cleared environment, a timeout and a killed process group.
- Approval is bound to the binary hash and is a human act on an authenticated dashboard page.
- A link is created only by an authenticated human, expires, and is audited.
- The bearer token is the root of trust for local processes. A local process with the token can send mail as its own project; budgets and the fixed text bound the damage.
- Claude channels need a development flag. Do not make them the default path.

## 6. Open questions for the owner

1. Is the default link lifetime of 2 hours right?
2. Should the dashboard also allow a standing cross-project rule, or only expiring links?
3. Accept the Claude channels research preview as an optional adapter?
4. Should the daemon be installed on first `ai-memory serve` after asking, or only by an explicit command?

## 7. Build status

Slices as the owner ordered them on 2026-11-15, on branch `feat/agent-bus`:

| Slice | State |
| --- | --- |
| F1 wake handles from hooks, discovery in the supervisor | Built. V105 `agent_wake_handles`, `GET /inbox/bus`, `discover = true`, `message supervise approve --adapter codex_queue`. A Claude Code handle is recorded but has no adapter yet. |
| F2 rooms inside a project | Built. V106, `memory_room_*`, `ai-memory room`, `/admin/rooms*`, root-only `/web/rooms`. A mention is addressed mail with a fixed body, so the F1 bus wakes the mentioned agent. |
| F3 Claude headless adapter, stopped Codex, pair budgets | Not built. |
| F4 human-opened links between projects | Not built. |
| F5 `agent-bus install`, dashboard approval, agent guide, `adopt` | Not built. |

Bite checks run while building them, each restored afterwards: the store
accepting a wake handle kind of another harness fails
`a_handle_of_another_harness_rolls_the_whole_event_back`; `GET /inbox/bus`
without its root check fails
`the_bus_needs_the_marker_the_root_credential_and_a_known_query`; a mention
body that carries the post text fails
`a_post_stays_in_its_room_and_only_a_mention_reaches_an_inbox`; and mention
parsing that lets a poster mention itself fails
`mentions_are_canonical_harness_names_once_each_without_the_poster`.

Choices made while building F1 and F2, for review:

- The wake handle is the session id itself, recorded only when the native id
  is a canonical UUID, so the server never stores a handle it derived.
  Whether a session is open is read from `sessions.ended_at`.
- `GET /inbox/bus` answers the root credential only, because it lists every
  project. A database user's supervisor keeps its configured entries.
- A program approval covers one adapter kind for every discovered agent. It is
  pinned to the program's path and SHA-256 and stored beside the entry
  approvals in `approvals.json`.
- Discovered agents are woken only by mail from their own project. Links (F4)
  are the planned way across projects.
- Room members are harnesses, the identity addressed mail already uses. Two
  sessions of one harness in a project share a membership and a read
  position.
- A plain post wakes nobody; only an `@harness` mention does. This keeps a busy
  room from spending every member's tokens.
