# Agent bus quickstart

Three steps on the machine where your agents run, then one check.

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
ai-memory learns which sessions exist and how to wake them:

```bash
ai-memory setup
```

or, per agent, `ai-memory install-mcp` and `ai-memory install-hooks`. Restart
the agents afterwards.

## 3. Install the agent bus on this machine

The server never runs programs. One per-user service on your machine does: it
waits on the server and wakes the agents. It needs the native `ai-memory`
binary and the server's root token in its configuration (the same one
`ai-memory status` uses).

On a Docker install, copy the binary out of the image first:

```bash
docker create --name aim-copy docker.io/akitaonrails/ai-memory:latest
docker cp aim-copy:/usr/local/bin/ai-memory ~/.local/bin/ai-memory
docker rm aim-copy
```

Then:

```bash
ai-memory agent-bus install
```

It prints what it will do and asks once:

1. write the user service (systemd on Linux, a LaunchAgent on macOS) that runs
   `ai-memory message supervise`;
2. add the Claude Code wake hook to `~/.claude/settings.json` (a backup is
   kept beside it);
3. start the service.

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
