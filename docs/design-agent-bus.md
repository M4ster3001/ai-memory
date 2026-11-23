# Agent bus: plug-and-play messaging between the agents of a project

Status: proposal, written 2026-11-15 after the first real run of the wake supervisor. Nothing in this note is built yet. It records what the owner asked for, what the supervisor of #28 gives today, where it falls short, and the design that closes the gap. It extends [design-agent-wakeup-and-secure-messaging.md](design-agent-wakeup-and-secure-messaging.md); where the two disagree, this note wins for what to build next.

## 1. What the owner asked for

1. Plug and play. A project turns messaging on without hand-written configuration.
2. Full duplex between the agents of one project. Any agent can reach any other and be answered the same way.
3. Fast, and no tokens spent while nothing happens. Waiting is free; an agent spends tokens only when it reads and answers a message that arrived.
4. ai-memory is the server and the orchestrator behind all of it. Projects do not run their own watchers.
5. A project talks to another project only when the human user asks for it. When they do, the system opens a connection between exactly the projects they named.
6. Any agent can adopt it by reading the documentation: Claude Code, Codex, Gemini and others. No external services.

Principle added 2026-11-16 (owner): rooms and every other chat feature are one more resource on top of the inbox, never a replacement. The inbox stays the base: one-to-one mail, delivery states and receipts, `memory_message_*`, the watcher and the wake supervisor keep working exactly as they do today, with or without rooms. A room post that mentions an agent becomes ordinary inbox mail for that agent, so anything that reads the inbox sees it. Removing the rooms must leave the inbox untouched, and no room feature may be required to use the inbox.

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
| F3 adapters and wakes, no hourly budget | Built (2026-11-21). `message watch --rewake` as Claude Code's `Stop` hook with `asyncRewake` (added by `install-hooks` on native installs and by `agent-bus install`); adapters `claude_resume`, `codex_exec_resume` (falls back to `codex_queue` on a busy chat or a refused login) and `gemini_resume` (documentation only, untested); V107 loop guard (ping-pong, repeated content, thread depth, per-pair breaker, human prompt resumes) and coalescing instead of budgets; programs trusted on first use; second-opinion fixes 1, 3, 4 and 5 (2 is the loop guard). |
| F4 human-opened links between projects | Built with F7 (2026-11-21), see §8. |
| F6 plug and play | Built (2026-11-21). `agent-bus install` writes the native hooks of every agent found, the Claude Code wake hook and the service; idempotent. Wake handles follow the directory the session reported last (V108); a stopped Codex conversation whose directory is gone is queued instead. |
| F7 chat page and links between projects | Built (2026-11-21). V109 `agent_links` and `agent_link_events`, `memory_link_*`, `ai-memory link`, `/admin/links*`, root-only `/web/chat` with a live timeline and a Cross-project tab, `/web/chat/watch`. §8 records what was built and where it differs from the first specification. |
| F6 mid-turn mail notice | Built (2026-11-21). Native `PreToolUse`/`PostToolUse` hooks of Codex and `PostToolUse` of Claude Code add a fixed line through `hookSpecificOutput.additionalContext` (documented by Codex for both events and present in the 0.169.0-alpha.11.1 hook schema), from `GET /handoff?inbox_turn=1`; check every 5 s, notice every 15 s, once per id; 300 ms budget. Gemini not covered. |
| F5 `agent-bus install`, status, doctor, dashboard, agent guide | Built (2026-11-21). `ai-memory agent-bus install|status|doctor`, `GET /inbox/bus/status`, `POST /inbox/bus/program`, root-only `/web/agent-bus` with a resume button, `docs/agent-bus/`. No `adopt` and no `memory_bus_whoami`: `doctor` covers the checks. |

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

Choices made while building F3 and F5, for review:

- An open Claude Code session is woken only by its `Stop` hook; the
  supervisor resumes a Claude Code session only when the server recorded it as
  ended and `claude agents --json` lists no process for it. A session that is
  open but whose process died without `SessionEnd` waits for the idle
  auto-end.
- The rewake hook waits 12 hours per turn (`timeout` 43200). Claude Code's
  upper bound for a hook timeout was not tested; a session idle for longer
  hears of its mail at its next prompt.
- The loop guard reads at most the last two hours of a pair when no human
  prompt was recorded, so a project without prompt capture is not paused by
  old traffic. Its thresholds (20 turns, 3 copies in 30 minutes, depth 40) are
  constants, not settings.
- A paused pair stays paused until a human prompt or the dashboard; there is
  no timed half-open state.
- Trust on first use is the default; `trust_on_first_use = false` keeps the
  terminal approval. Configured entries keep their terminal approval either
  way.
- The Claude Code hook that `agent-bus install` writes carries no token: it
  passes `--data-dir` and `--config`, and the binary reads the server and token
  from that configuration. `install-hooks` puts the URL and token in the
  command's environment, as its other hooks do.
- Bite checks run while building F3 (each restored): dropping the `paused`
  label from `wake-candidates` fails `candidates_label_mail_of_a_paused_pair`;
  dropping the world-writable check from `check_session_dir` fails
  `a_resumed_session_runs_only_in_a_checked_directory_of_the_operator`; letting
  the rewake hook wake on `paused` mail fails
  `only_new_mail_from_another_agent_or_a_receipt_wakes`.

## 8. F7 specification: the chat page and links between projects

Written 2026-11-21 from the owner's request and built the same day. It extends
§3.6 (links) and replaces §3.7 (rooms as a link with a name) for the web view.
§8.4 lists what was built and where it differs from the text above it; the
text is kept as the first specification.

### 8.1 `/web/chat`

- Root only, like `/web/rooms`, reached from the sidebar "Agents" group
  (today "Agent chat" points at `/web/rooms`; it will point here).
- A project selector, then one timeline per project, newest at the bottom:
  addressed mail between agents of the project (`agent_messages`,
  `kind = 'message'`) and room posts, merged by time. One bubble per item,
  with an icon and colour per sender: `claude-code`, `codex`, `gemini-cli`,
  and "human" for posts from the dashboard.
- Each mail bubble shows its delivery state (`queued`, `wake_attempted`,
  `delivered`, `wake_failed/<reason>`), receipts folded under the message
  they report on, and a banner with the Resume button when the pair is
  paused (the existing `POST /web/agent-bus/resume`).
- Bodies are shown only for mail inside the selected project, or between
  two projects with an active link. Mail from any other project shows its
  sender project and state, never its body.
- Updates without reload through a new root-only long-poll,
  `GET /web/chat/watch?project=<id>&after=<cursor>`, woken by the writer's
  inbox signals (the same mechanism as `GET /inbox/watch`); no model call.
- A composer posts to a room as `human` (`from_agent = other`, a new
  `human` label in the view). An `@codex` or `@claude-code` mention becomes
  mail as today. A human post also counts as a human anchor for the loop
  guard, so it resumes paused pairs of the project.

### 8.2 Links between projects (the §3.6 design, refined)

States: `requested` → `confirmed` (by the destination) → `active` →
`closed` or `expired`. A request never wakes anyone.

- V108 is taken by the wake handle directory; links need V109:
  `agent_links(id, a_workspace_id, a_project_id, b_workspace_id,
  b_project_id, state, requested_at, requested_by, reason, confirmed_at,
  confirmed_via, expires_at, closed_at, closed_by)`, pair normalized, one
  live link per pair, and `agent_link_events` for the audit trail.
- Opening: a human asks in project A (prompt captured by A's hooks, checked
  as in the authorization research), or uses "Connect projects" on
  `/web/chat` (two projects, reason, validity default 2 h, at most 24 h).
- Confirming: the destination's human, either with the "Confirm" button on
  the destination project's chat page, or in the destination agent's chat:
  the agent shows the request, asks, and calls `memory_link_confirm` with
  the reference of the human's answer, which the server checks was captured
  after the request was shown.
- While active, mail between the two projects passes the wake gate as
  same-project mail (`wake-candidates` labels it `linked`), and only between
  those two. The loop guard applies unchanged. Closing: either human, any
  time; expiry closes it by itself.
- Surfaces: MCP `memory_link_request`, `memory_link_confirm`,
  `memory_link_close`; CLI `ai-memory link request|confirm|close|list`;
  root-only routes `/admin/links*`; the "Cross-project" tab on `/web/chat`
  lists links with state, expiry and who asked, with Confirm and Close.

### 8.3 Where this differs from §3.6

- Two-step opening (request, then confirmation in the destination) instead
  of one human act on the dashboard.
- Default validity stays 2 h (owner choice in this request), maximum 24 h.
- Links carry their own audit table besides `audit_log`.

### 8.4 As built

Where the build differs from §8.1 to §8.3, and from §3.6:

- Links are stored in the direction they were asked (`from_*` asked, `to_*` is
  the destination that confirms), not as a normalized pair. A partial unique
  index over the ordered pair of project ids (`MIN`, `MAX`) keeps one live
  link per pair in either direction. The destination has to be known, so the
  direction is needed.
- The table has no `confirmed_via = 'dashboard'`; it has `web`, `web_both`
  (the root user ticked "also confirm for the destination" in the form) and
  `agent`. A request from the web form starts `requested` unless that box is
  ticked, so the destination's Confirm button is the default path.
- Validity is counted from the confirmation. A request that nobody confirms
  expires 24 hours after it was filed. The expiry check is written once
  (`ai_memory_core::agent_links::is_past`) and the wake gate runs it itself, in
  the same read as `wake-candidates`; the 60-second message sweep only records
  `expired` and closes the row's trail.
- The agent's confirmation evidence is simpler than the research draft's
  `prompt_ref`: the server records which agent session was shown the request
  (`memory_link_list` is the presentation), and requires a user prompt in that
  same session after it, within 30 minutes, that is not the bus's wake text. It
  does not read the prompt. The prompt of the requester's user is not checked at
  request time: a request is inert and the confirmation is the gate; in its
  place there are caps (8 live links per project, 6 agent requests per project
  per hour).
- `wake-candidates` answers the label `linked`. The supervisor, the rewake hook
  and the mid-turn notice treat it like `same_project`. A supervisor binary
  older than this release does not know the label and never wakes on it
  (it is read as unknown), which is the safe failure.
- "Archived" has no meaning in ai-memory today: a project can only be purged.
  Purging a project (or its workspace) ends its live links in the same
  transaction and writes `agent_link_closed` with `project_purged` to the audit
  log; the link rows then go with the project (the audit rows stay).
- The chat page shows another project's mail text only when the mail was sent
  while a link between the two was active (a window from the confirmation to the
  close or clock end), so the conversation of an ended link stays readable and
  mail around it does not. Receipts are folded under their mail, never bubbles of
  their own. The page updates through a fingerprint long-poll (a handful of
  indexed counts), not through the inbox signals, because room posts and
  delivery reports do not raise those.
- A human post from the dashboard is a room post with `from_agent = other`,
  shown as "human"; besides the mention mail it records a human anchor for every
  harness of the project that has a wake handle and resumes the project's paused
  pairs.
- The CLI cannot confirm as the root user. `link confirm` and
  `/admin/links/confirm` apply the agent rules (recorded session, shown, user
  prompt after). The root user's confirmation is the web button, accepted only
  for the destination project's page.
- §3.6 says "only the authenticated human can open it" and "one human act on the
  dashboard". Now: a human request needs the destination's confirmation too
  (one extra click, or the "both" box), and an agent may file a request, never
  confirm its own.

### 8.5 Bite checks

Each mutation was applied, the named test turned red, and the code was
restored (the five ran in one scripted pass, store tests only):

| Mutation | Test that failed |
| --- | --- |
| The wake gate ignores the clock (`l.expires_at > now` replaced by a constant) | `an_expired_link_wakes_nobody_even_before_the_sweep_ends_it` |
| An agent's confirmation drops the destination check | `an_agent_confirms_only_after_its_user_answered_the_request_it_was_shown` (the requester then meets `OwnRequest`, so the refusal is backed by a second rule, and the test pins the exact code) |
| The confirming prompt need not follow the presentation | `an_agent_confirms_only_after_its_user_answered_the_request_it_was_shown` |
| The chat shows another project's text whatever the link | `the_chat_shows_another_projects_text_only_for_mail_sent_during_a_link` |
| The web Confirm drops the destination check | `a_request_wakes_nobody_and_the_destination_page_opens_exactly_the_pair` |

