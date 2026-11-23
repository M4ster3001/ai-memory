# Agent bus quickstart

On the machine where your agents run, with the server already up and the
native `ai-memory` binary on your `PATH` (steps 1 to 3 below explain each):

```bash
ai-memory agent-bus install --yes
ai-memory agent-bus doctor
```

That is the whole setup: two commands and no file to edit. Then restart the
agents you had open. A new project needs nothing: open the agent in it and
its hooks report the session.

To let an agent wire itself instead, paste this into it:

```text
Run `ai-memory agent-bus doctor`; if any line says FAIL, run the fix it prints (usually `ai-memory agent-bus install --yes`), run doctor again, and tell me what is still failing.
```

The steps, in detail:

## 1. Run a server with the agent bus

Run or upgrade the ai-memory server to a release with the agent bus, keeping
your data volume. With Docker:

```bash
docker pull docker.io/akitaonrails/ai-memory:latest
docker stop ai-memory && docker rm ai-memory
docker run -d --name ai-memory --restart unless-stopped \
    -p 127.0.0.1:49374:49374 -v ai-memory-data:/data \
    docker.io/akitaonrails/ai-memory:latest
```

Add the `-e` options you used before. The server migrates the database on
start; the agent bus needs nothing else from it.

## 2. Wire your agents

Each agent needs the ai-memory MCP entry and hooks. The hooks are how
ai-memory learns which sessions exist, their thread or session id and their
directory, and how a busy Codex or Claude Code hears of mail between tool
calls. `ai-memory agent-bus install` (step 3) writes the native hooks for
every agent it finds (`~/.claude`, `~/.codex`, `~/.gemini`), so this step is
only the MCP entry, if the agents do not have it yet:

```bash
ai-memory install-mcp
```

## 3. Install the agent bus on this machine

The server never runs programs. One per-user service on your machine does: it
waits on the server and wakes the agents. It needs the native `ai-memory`
binary and the server's root token in its configuration (the same one
`ai-memory status` uses).

On a Docker install, copy the binary out of the image first:

```bash
docker create --name aim-copy docker.io/akitaonrails/ai-memory:latest
docker cp aim-copy:/usr/local/bin/ai-memory ~/.local/bin/ai-memory
mkdir -p ~/.local/share/ai-memory
docker cp aim-copy:/usr/local/share/ai-memory/hooks ~/.local/share/ai-memory/hooks
docker rm aim-copy
```

The hook bundle beside it (`~/.local/share/ai-memory/hooks`) is where
`install` finds the hook files; pass `--hooks-dir` if you keep it elsewhere.

Then:

```bash
ai-memory agent-bus install
```

It prints what it will do and asks once:

1. write the user service (systemd on Linux, a LaunchAgent on macOS) that runs
   `ai-memory message supervise`;
2. write the native hooks of each agent it finds, as
   `ai-memory install-hooks --agent <agent> --apply` would (Claude Code with
   its wake hook, Codex, Gemini CLI); each file gets a backup;
3. start the service.

Run it again at any time: it rewrites the same hooks and unit and changes
nothing else.

No configuration file is written. Agents are discovered from their hook
sessions, and the `claude`, `codex` and `gemini` programs are trusted the
first time they are needed: their path and SHA-256 are recorded, and a changed
binary is logged and shown on `/web/agent-bus`. Pass `--dry-run` to see the
plan without changing anything, or `--yes` to skip the question.

Restart open Claude Code sessions so they load the new hook.

## Check

```bash
ai-memory agent-bus doctor
```

Every line should read `ok`. A `FAIL` line says how to fix it. Then ask one
agent, in a prompt, to message another in the same project, for example in
Claude Code: "send codex a message asking which files it changed today and
wait for the answer". The Codex chat gets a turn, answers, and Claude Code is
woken by the answer.

`ai-memory agent-bus status` and the `/web/agent-bus` page show the agents
the server knows, how their last wake went and any paused pair.
