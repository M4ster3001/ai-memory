# Agent bus troubleshooting

Run `ai-memory agent-bus doctor` first: it checks each piece and prints a fix
for what fails. The service logs one line per wake to the user journal:

```bash
journalctl --user -u ai-memory-supervisor -f
```

| Symptom | Cause | Fix |
| --- | --- | --- |
| `doctor`: the server refused, the agent bus needs the root credential | The CLI uses a database user's token | Give the CLI the root token in `config.toml` or `AI_MEMORY_AUTH_TOKEN`, then reinstall |
| `doctor`: no ai-memory server answers | The server is down or at another URL | Start it; set `server_url` in `config.toml` |
| `doctor`: service not installed or not running | `agent-bus install` was not run, or the service failed | `ai-memory agent-bus install`; read the journal |
| An open Claude Code session is never woken | The wake hook is missing, or the session started before it was installed | `ai-memory agent-bus install`, then restart the session |
| An idle Claude Code session stopped being woken after 12 hours | The hook waits 12 hours per turn | The mail is announced at the next prompt; a new turn starts a new wait |
| A busy Codex or Claude Code never hears of mail during a turn | Its hooks are the Docker shell scripts, not the native binary, or a single tool call is still running | `ai-memory agent-bus install --yes` rewrites the native hooks; `agent-bus doctor` checks them. A notice arrives only between tool calls |
| `wake_failed/session_not_found` for Codex after its project folder moved | The session's last reported directory is gone | Nothing to do: the turn is queued with `codex queue` instead, and the next prompt in the conversation records the new directory |
| A Codex turn ran but the desktop app does not show it | The app does not draw turns it did not start | Reload the conversation in the app |
| `agent_unavailable/auth_failed` for Codex | The Codex CLI is signed in with an API key whose organization is in the API credits promotion (`codex_byok_restricted_by_api_credit_promo`) | `codex login` with your ChatGPT account. Until then the turn is queued and runs when you open the chat |
| `agent_unavailable/binary_missing` | The service did not find `claude`, `codex` or `gemini` | Install it in `~/.local/bin`, `/usr/local/bin` or another directory on the service's `PATH` |
| `wake_failed/not_approved` | The program failed the file checks (owned by another user, writable by others, under `/tmp`), or `trust_on_first_use = false` | Fix the file, or run `ai-memory message supervise approve --adapter <id>` |
| `wake_failed/session_not_found` | The stopped session's directory is gone, is world-writable or is under `/tmp`, or the harness has no such session | Open the agent in its project directory again |
| `wake_failed/loop_suspected` | The loop guard paused two agents | Prompt either session yourself, or press Resume on `/web/agent-bus` |
| Mail to another project never wakes its agent | No active link joins the two projects, or it expired | Open Agent chat, Cross-project: connect the projects and Confirm on the destination's page. See [CROSS_PROJECT.md](CROSS_PROJECT.md) |
| The link is `active` but the other project's agent is still not woken | The supervisor binary is older than the link label (`linked`) | Upgrade the binary and run `ai-memory agent-bus install` again |
| A request stays `requested` | The destination has not confirmed. The Confirm button shows only on the destination project's page | Open the destination project in Agent chat, Cross-project, and press Confirm; or ask its agent to run `memory_link_list` and ask its user |
| `memory_link_confirm` is refused with `not_presented` or `no_user_answer` | The agent did not show the request in this session, or its user has not answered since | Call `memory_link_list`, ask the user, wait for the answer, then confirm |
| `memory_link_confirm` is refused with `no_session` | The agent has no hook-recorded session (a static MCP client) | Confirm on the web page |
| The chat page says "reconnecting" | The long-poll cannot reach the server, or 16 pages are already watching | Reload the page; close other chat tabs |
| The chat page shows a mail without its text | The other project had no active link when the mail was sent | Expected: the page shows only who and where for such mail |
| A Claude Code session is not resumed though it is closed | `claude agents --json` still lists it, or the server has not seen its end | Close it fully; the server marks a session ended on `SessionEnd` or after it goes idle |
