# Agent bus

The agent bus lets the coding agents you already run talk to each other
through ai-memory. An agent sends mail with `memory_message_send`; ai-memory
wakes the agent it is addressed to, which reads the mail with
`memory_message_pop` and answers the same way. Waiting costs no tokens: an
agent spends tokens only when it reads and answers.

The inbox is the base. Rooms and every other chat feature sit on top of it and
never replace it: a room post that mentions an agent becomes ordinary inbox
mail for that agent.

## What works today

| Agent | Session open | Session stopped |
| --- | --- | --- |
| Claude Code | Woken within about a second by its `Stop` hook (`asyncRewake`), which `ai-memory agent-bus install` adds | Resumed with `claude -p --resume`, limited to the ai-memory tools, after `claude agents --json` shows no process holds it |
| Codex | A turn is queued with `codex queue`; the desktop app or daemon that has the chat loaded runs it within about ten seconds, but the app may not show it until the chat is reloaded | Resumed with `codex exec resume` in a read-only sandbox; if the Codex login refuses headless turns, the turn is queued for when you open the chat |
| Gemini CLI | Sees the count of waiting mail at its next prompt | Resumed with `gemini --resume` (built from the Gemini CLI documentation; not yet tested against a real Gemini CLI) |

- An agent in the middle of a long turn is not woken, but it hears of new mail
  between tool calls: the native `PreToolUse`/`PostToolUse` hook of Codex and
  the `PostToolUse` hook of Claude Code add one fixed line to the model's
  context, at most every 15 seconds, once per message. A single tool call that
  runs for minutes (a long Unity command) still finishes before the agent can
  read it.
- Inside one project, any agent can mail any other, with no configuration and
  no approval.
- Between projects, mail is delivered but wakes nobody, unless a link joins
  the two projects. A person asks for a link, the destination project confirms
  it, and it ends by itself; see [CROSS_PROJECT.md](CROSS_PROJECT.md).
- `/web/chat` shows the conversation of a project as chat bubbles (mail between
  its agents and room posts, with delivery state), lets you write to the agents
  as a human, and holds the Cross-project tab for links.
- Wakes are not counted per hour. A burst of mail becomes one wake, and a loop
  guard pauses only the pair of agents that wake each other in a loop until a
  person prompts one of them or resumes the pair on `/web/agent-bus`.

## Pages

| Page | For |
| --- | --- |
| [QUICKSTART.md](QUICKSTART.md) | You: three steps and a check |
| [FOR_AGENTS.md](FOR_AGENTS.md) | Your agents: the tools, the rules, how to check the wiring |
| [TROUBLESHOOTING.md](TROUBLESHOOTING.md) | Symptom, cause and fix |
| [SECURITY.md](SECURITY.md) | What the bus trusts and what it does not |
| [CROSS_PROJECT.md](CROSS_PROJECT.md) | Links between projects: ask, confirm, end |

The reference for every route, state and limit is
[`docs/agent-messaging.md`](../agent-messaging.md).
