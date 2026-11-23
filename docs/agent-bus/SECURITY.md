# Agent bus security

For reviewers. The full inventory is rows 8o to 8r of
[`docs/security-boundaries.md`](../security-boundaries.md).

## Who runs what

- The server never runs a program. It stores sessions, mail, wake handles and
  the loop guard, and answers the host service.
- The host service (`ai-memory message supervise`, installed by
  `ai-memory agent-bus install`) is the only process that starts a harness. It
  runs a closed set of adapters (`codex_queue`, `codex_exec_resume`,
  `claude_resume`, `gemini_resume`) from the program's canonical path, without
  a shell, with an environment cleared to a short allowlist (no ai-memory token,
  no provider key), no stdin, a timeout that kills the process group, and 4 KiB
  of output read only to classify the result.
- The Claude Code wake hook runs `ai-memory message watch --rewake`. It only
  waits and exits; it starts nothing.

## What reaches an agent

Only the fixed wake text: a count, the newest message id, the agent and the
inbox names. Never a subject, body or sender. The agent reads the mail with
its own tools and is told it is untrusted data.

## What a resume may do

- `claude_resume`: `--tools ""` (no built-in tool), `--allowedTools
  mcp__ai-memory`, `--permission-mode dontAsk`, 12 turns, in the session's own
  directory. It runs only when `claude agents --json` answers and lists no
  process for the session, and the directory exists, belongs to you, is not
  writable by every user and is not under a temporary directory.
- `codex_exec_resume`: a read-only sandbox, in the conversation's directory
  under the same checks as a Claude Code resume.
- `gemini_resume`: `--approval-mode default`; not yet run against a real
  Gemini CLI.

## Trust in programs

The program of a known adapter is trusted the first time it is needed, after
file checks (a regular executable, owned by you or root, not group- or
world-writable, not in a world-writable directory, not under `/tmp`,
`/var/tmp` or `/dev/shm`). Its path and SHA-256 are recorded. A changed binary
at the same path is trusted again, logged, audited
(`agent_bus_program_trusted`) and shown on `/web/agent-bus`. This protects
against mistakes and records upgrades; it does not stop a process that already
runs as you, which could replace the binary anyway. Set
`trust_on_first_use = false` to require a terminal approval for every binary.

## Who may wake whom

- Inside a project: any agent, by mailing another harness. An agent's mail to
  itself never wakes it.
- Across projects: nobody, unless a link joins the two projects or an operator
  lists the project in a configured entry's `wake_from`. A link is requested,
  confirmed by the destination project and ended by its clock or by either
  side (see below).
- Any local process with the server token can send mail as a project and so
  wake its agents, within the fixed text and the loop guard. Keep the token
  readable by you alone.

## Links between projects

A link lets mail between exactly two projects wake like mail inside one
project. The rules run on the server in one transaction each, with no model and
no message text:

- A request never wakes anyone. Only the destination confirms: the root user on
  the destination project's page, or an agent of the destination.
- The requester never confirms its own request. The server refuses a caller
  that is not in the destination project (`not_destination`) and a session that
  filed the request (`own_request`).
- An agent confirms only with a hook-recorded session that was shown the
  request (`memory_link_list`) and then saw a user prompt that is not the bus's
  wake text, within 30 minutes. The server proves a user prompt followed; it
  does not read it, so it cannot tell yes from no. An agent that confirms
  without asking defeats this, which is why the tool text and the agent guide
  say never to.
- The CLI has no root confirmation. The web buttons need the root credential,
  the `x-ai-memory-ui` header and a same-origin request, and the server accepts
  Confirm only from the destination project's page.
- A link ends at its clock (at most 24 hours after the confirmation), when
  either side closes it, or when a project is purged. The wake gate compares the
  clock itself, so a link past its end wakes nobody even before the minute
  sweep records it. One live link per pair; at most 8 live links per project and
  6 agent requests per project per hour.
- A link's reason is untrusted text. It is scrubbed for secrets, bounded to 300
  characters, shown to the destination as a quotation and never obeyed.
- The Chat page shows another project's mail text only for mail sent while a
  link was active. Everything it shows from an agent is escaped (never markup),
  stripped of control and bidirectional characters, and cut.
- Every request, confirmation (with how: `web`, `web_both` or `agent`),
  refusal code, wake outcome and ending is in the link's event trail, and the
  request, confirmation and ending are in the audit log, never with message
  text.

A local process that holds the server token can still forge a hook session and
a user prompt (`POST /hook`) and so confirm as an agent. The token is the root
of trust for local processes; links add a human step against injected text and
runaway agents, not against a process that already owns the token.

## Loops

The server pauses a pair of agents on ping-pong (20 direction changes), on the
same body three times in 30 minutes, or on a chain of more than 40 woken
turns, with no human prompt in between. A prompt is human when it is not the
wake text. The mail is still delivered. Pauses and resumes are audited without
text.

## Known limits

- A token holder can forge a hook session with a directory it owns; a
  `claude -p` resume then runs with that directory's project settings.
- Prompts reach the server through the hook spool, so a resume by prompt can
  lag.
- `claude agents --json` may not list a session that is open only in another
  application; the bus also needs the server to have recorded the session as
  ended before it resumes it.
