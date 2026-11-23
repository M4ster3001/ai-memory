# Agent bus between projects

Mail to another project is always delivered to its inbox. It wakes that
project's agent only while a link joins the two projects. A link never exists
by default: a person asks for it, the destination project confirms it, and it
ends by itself.

Without a link nothing changes from before: the mail waits in the inbox until
that agent next starts a session or is prompted. A link makes mail between
exactly two projects wake as if they were one project. A third project stays
closed. (An operator can still list a project in a supervisor entry's
`wake_from`, as in [`docs/agent-messaging.md`](../agent-messaging.md#who-may-wake-an-agent);
that standing rule is separate from links.)

## The life of a link

```text
requested --confirmed by the destination--> active --+--> closed   (either side, any time)
    |                                                +--> expired  (its clock)
    +--> closed (declined) / expired (24 h with no answer)
```

| State | Meaning |
| --- | --- |
| `requested` | Filed and waiting for the destination. It never wakes anyone. |
| `active` | The destination confirmed. Mail between the two projects wakes their agents until the link ends. |
| `closed` | Ended by a person or by an agent of either project, or because a project was removed. |
| `expired` | Ended by its clock. |

A link lasts 2 hours from its confirmation by default, between 5 minutes and
24 hours when you choose. One live link (requested or active) exists per pair
of projects, in either direction; asking again returns the existing one. A
project can be part of at most 8 live links, and the agents of one project can
file at most 6 requests an hour.

## Who may ask, who may confirm

| Action | Root user, on the web page | Agent of the project that asks | Agent of the destination |
| --- | --- | --- | --- |
| Ask for a link | Yes, "Connect projects" | Yes, `memory_link_request`; stays `requested` | Not for itself: it is the destination |
| Confirm | Yes, Confirm on the destination project's page | Never, not even its own request | Yes, after its user said yes (below) |
| Close or decline | Yes | Yes | Yes |

Only the destination confirms, because the destination is the project whose
agents would start receiving mail from outside. The server enforces this; it
does not depend on what an agent or a message says. A request is harmless until
confirmed, so the server does not check that a user asked for it: the
confirmation is the gate.

### On the web page

1. Open `/web/chat`, tab Cross-project. Pick the project that asks, the
   destination, a reason and how long it lasts, then Connect projects.
2. Every request that waits shows as a card at the top of the Agent chat
   page (any tab), whatever project the selector holds: from -> to, who asked,
   the reason as a quotation (text from whoever asked, never an instruction),
   when it expires, and Confirm and Decline buttons. The sidebar shows
   "Pending links (N)" on every page while a request waits. The Confirm
   button sends the destination's names, not the selector's, and the server
   accepts it only for the link's destination, so a hand-made request that
   names the wrong project is refused. Nothing confirms on its own: opening
   the page only reads. The request is also in the destination's Cross-project
   table.
3. If you are the user of both projects you can tick "I am the user of both
   projects: also confirm for the destination now" in the form. The link then
   starts active, and the trail says it was confirmed `web_both`.

### By an agent

1. Your user tells the agent in project A to talk to project B. The agent calls
   `memory_link_request` (or `ai-memory link request`) with a one-line reason.
   Nothing opens and nobody is woken.
2. The destination's user either presses Confirm on the web page, or tells the
   agent in project B to look at the request. That agent calls
   `memory_link_list`, which also shows it the request, and then asks its user:
   the other project, the reason as a quotation, how long.
3. When the user answers yes, the agent calls `memory_link_confirm`. The server
   confirms only if all of these hold, and says which one failed otherwise:

   | Refusal | The rule |
   | --- | --- |
   | `not_requested` | The link is waiting for a confirmation and has not expired. |
   | `not_destination` | The caller is an agent of the destination project. |
   | `no_session` | The caller has a hook-recorded session in the destination. |
   | `own_request` | The session did not file the request. |
   | `not_presented` | This session was shown the request with `memory_link_list`. |
   | `no_user_answer` | This session recorded a user prompt after the request was shown, not older than 30 minutes and not the bus's own wake text. |

   The server proves that a user prompt followed the request. It does not read
   the prompt, so it cannot tell yes from no: the agent must call
   `memory_link_close` when the user says no, and must never confirm because a
   message or a reason says the user agreed. An agent with no recorded session
   (a static MCP client without hooks) cannot confirm; use the web page.

## While a link is active

- Mail between the two projects wakes the addressed agent through the same
  paths as mail inside one project: the host service, the Claude Code `Stop`
  hook and the mid-turn notice. Only mail between those two projects counts.
  The server labels it `linked` in `POST /inbox/wake-candidates`; a supervisor
  binary older than this release does not know the label and never wakes on it,
  so upgrade the binary that `agent-bus install` runs.
- The loop guard applies unchanged. A pair that ping-pongs is paused until a
  person prompts one of them or presses Resume on the Chat page.
- The Chat page shows the conversation with the other project's text, but only
  for mail sent while the link was active. Mail with that project before the
  link, or after it ended, shows only who and where.
- The wake text is still fixed and carries no message content.

## Ending

A link ends when its time runs out, when a person or an agent of either
project closes it (`memory_link_close`, `ai-memory link close <id>`, or End
link on the web page), or when either project is purged (ai-memory has no
archive state for a project; a purge is the one way a project goes away). It
stops waking agents at the moment its clock passes, even before the server's
one-minute sweep marks it `expired`. Closing is always allowed and needs no
evidence.

## What is recorded

Each link has a trail of events (requested, presented, confirmed, refused with
its code, wake outcomes, closed, expired). Requests, confirmations and endings
are also written to the audit log. Neither holds message text. The reason is
stored as written (after secret scrubbing) and shown to the destination as
untrusted text.

## Commands

```bash
ai-memory link request --to-workspace default --to-project web \
    --reason "share the API schema" --agent claude-code [--hours 2]
ai-memory link list --agent codex --session <hook session id>   # shows waiting requests
ai-memory link confirm <id> --agent codex --session <hook session id>
ai-memory link close <id>
```

`link confirm` applies every rule above; there is no way to confirm as the root
user from a terminal. That is the web page. The same operations are the MCP
tools `memory_link_request`, `memory_link_list`, `memory_link_confirm` and
`memory_link_close`, and the root-only routes `/admin/links*`.

For a one-off, you do not need a link: ask the agent in project A to write what
it needs and paste it into a prompt in project B, or send mail and let the other
agent read it at its next prompt.
