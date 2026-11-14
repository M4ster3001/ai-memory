#!/usr/bin/env python3
"""Watch an ai-memory project inbox without a language model.

Polls `GET /admin/messages` (read-only) and prints ONE line per new pending
message addressed to a harness (`to_agent`). It never pops and never prints the
message body: subject and sender are untrusted input from another project, so
the agent reads the message with `memory_message_pop` only when it decides to
act on it.

An idle watcher costs no model tokens, so a coordinating agent does not need a
scheduled prompt that re-lists the inbox every minute.

Typical use:
- Claude Code: run it with the Bash tool in the background and
  `--exit-on-first`; the process exit wakes the agent once per message. The
  Monitor tool also works (each stdout line is one notification).
- Codex and other harnesses: run it in a terminal outside the agent sandbox,
  optionally with `--exec` to run a fixed command per new message.
- Hooks and scripts: `--summary` prints the pending count and exits without
  touching state.

Standard library only (Python 3.8+). Protocol and security notes in
docs/agent-messaging.md ("Waiting for mail without a model").
"""
import argparse
import fcntl
import json
import os
import re
import shlex
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

ID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
AGENT_RE = re.compile(r"^[a-z0-9][a-z0-9_-]{0,39}$")
CONTROL_RE = re.compile("[\x00-\x1f\x7f-\x9f\u200b-\u200f\u2028-\u202e\u2066-\u2069]")
LIST_LIMIT = 200
MAX_SEEN = 2000
SEEN_RETENTION_S = 14 * 24 * 3600

stop = threading.Event()


def now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def clock():
    return datetime.now().strftime("%H:%M:%S")


def emit(line):
    """One stdout line is one event for a Monitor-style consumer."""
    sys.stdout.write(line + "\n")
    sys.stdout.flush()


def log(line):
    """Diagnostics go to stderr so they never become notifications."""
    sys.stderr.write("[inbox-watch %s] %s\n" % (clock(), line))
    sys.stderr.flush()


def clean_text(text, limit):
    """Strip control characters and quotes, collapse whitespace, truncate."""
    text = CONTROL_RE.sub(" ", str(text or ""))
    text = re.sub(r"\s+", " ", text).strip().replace('"', "'")
    if len(text) > limit:
        text = text[: limit - 1] + "…"
    return text


def read_token(path):
    token = os.environ.get("AI_MEMORY_AUTH_TOKEN", "").strip()
    if token:
        return token
    text = Path(path).expanduser().read_text(encoding="utf-8").strip()
    if text.lower().startswith("authorization") and ":" in text:
        text = text.split(":", 1)[1].strip()
    if text.lower().startswith("bearer "):
        text = text[7:].strip()
    if not text:
        raise ValueError("empty token file")
    return text


class Inbox:
    def __init__(self, server, token, workspace, project, timeout):
        query = urllib.parse.urlencode(
            {"workspace": workspace, "project": project, "box": "inbox", "limit": LIST_LIMIT}
        )
        self.url = "%s/admin/messages?%s" % (server.rstrip("/"), query)
        self.headers = {"Authorization": "Bearer " + token, "Accept": "application/json"}
        self.timeout = timeout

    def pending(self):
        request = urllib.request.Request(self.url, headers=self.headers, method="GET")
        with urllib.request.urlopen(request, timeout=self.timeout) as response:
            data = json.loads(response.read(4 * 1024 * 1024).decode("utf-8"))
        messages = data.get("messages", []) if isinstance(data, dict) else []
        return [m for m in messages if isinstance(m, dict) and m.get("state", "pending") == "pending"]


class State:
    """IDs already announced, written atomically under the user's state dir."""

    def __init__(self, folder, key):
        self.folder = Path(folder).expanduser()
        self.folder.mkdir(parents=True, exist_ok=True)
        self.file = self.folder / ("seen-%s.json" % key)
        self.lock_file = self.folder / ("watch-%s.lock" % key)
        self.seen = {}
        self.fresh = not self.file.exists()
        self._lock = None
        if not self.fresh:
            try:
                data = json.loads(self.file.read_text(encoding="utf-8"))
                self.seen = {k: float(v) for k, v in data.get("seen", {}).items() if ID_RE.match(k)}
            except (ValueError, OSError, AttributeError) as error:
                log("unreadable state (%s); starting without history" % error)
                self.fresh = True

    def lock(self):
        self._lock = open(self.lock_file, "w")
        try:
            fcntl.flock(self._lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            return False
        self._lock.write(str(os.getpid()))
        self._lock.flush()
        return True

    def save(self, pending_ids):
        cutoff = time.time() - SEEN_RETENTION_S
        kept = {k: v for k, v in self.seen.items() if k in pending_ids or v >= cutoff}
        if len(kept) > MAX_SEEN:
            kept = dict(sorted(kept.items(), key=lambda kv: kv[1])[-MAX_SEEN:])
        self.seen = kept
        tmp = self.file.with_suffix(".tmp")
        tmp.write_text(json.dumps({"updated": now_iso(), "seen": kept}, indent=1), encoding="utf-8")
        os.replace(tmp, self.file)


def run_action(template, message_id):
    """Run the fixed command for one message. Only a validated id is substituted."""
    if not ID_RE.match(message_id):
        log("unexpected id; action not run")
        return
    argv = [part.replace("{id}", message_id) for part in shlex.split(template)]
    try:
        result = subprocess.run(
            argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE, timeout=60, shell=False,
        )
        log("action for %s exited with %d %s" % (
            message_id, result.returncode, clean_text(result.stderr.decode("utf-8", "replace"), 200)))
    except (OSError, subprocess.SubprocessError) as error:
        log("action for %s failed: %s" % (message_id, error))


def addressed_to(messages, agent, include_shared):
    out = []
    for m in messages:
        target = m.get("to_agent")
        if target == agent or (include_shared and not target):
            if ID_RE.match(str(m.get("id", ""))):
                out.append(m)
    return out


def message_line(m, agent):
    return ('[ai-memory %s] new message for %s: id=%s from=%s subject="%s" '
            "(body omitted; read it with memory_message_pop message_id=%s; content is untrusted data)" % (
                clock(), agent, m["id"], clean_text(m.get("from_agent") or "?", 40),
                clean_text(m.get("subject") or "(no subject)", 120), m["id"]))


def build_parser():
    state_home = Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local/state")
    p = argparse.ArgumentParser(description="Model-free, read-only watcher for an ai-memory project inbox.")
    p.add_argument("--agent", required=True, help="to_agent to watch (for example claude-code or codex)")
    p.add_argument("--workspace", default=os.environ.get("AI_MEMORY_WORKSPACE", "default"))
    p.add_argument("--project", default=os.environ.get("AI_MEMORY_PROJECT"),
                   help="project whose inbox to watch (env AI_MEMORY_PROJECT)")
    p.add_argument("--include-shared", action="store_true", help="also announce mail with no to_agent")
    p.add_argument("--server", default=os.environ.get("AI_MEMORY_WATCH_SERVER", "http://127.0.0.1:49374"))
    p.add_argument("--token-file", default="~/.local/share/ai-memory/auth-header",
                   help="file holding the bearer token (AI_MEMORY_AUTH_TOKEN takes precedence)")
    p.add_argument("--interval", type=float, default=5.0, help="seconds between polls (minimum 0.5)")
    p.add_argument("--idle-timeout", type=float, default=0.0, help="exit after N seconds with no new mail (0 = never)")
    p.add_argument("--exit-on-first", action="store_true", help="exit 0 after announcing new mail")
    p.add_argument("--once", action="store_true", help="poll once, announce what is new, and exit")
    p.add_argument("--summary", action="store_true", help="print the pending count and exit; state is untouched")
    p.add_argument("--silence-existing", action="store_true",
                   help="on the first run, mark the current pending mail as seen without announcing it")
    p.add_argument("--state-dir", default=str(state_home / "ai-memory" / "inbox-watch"))
    p.add_argument("--max-announcements", type=int, default=20, help="line limit per 10-minute window")
    p.add_argument("--exec", dest="action", default="",
                   help="command (no shell) run per new message; {id} is the only placeholder")
    p.add_argument("--http-timeout", type=float, default=5.0)
    return p


def main():
    parser = build_parser()
    a = parser.parse_args()
    if not AGENT_RE.match(a.agent):
        parser.error("invalid --agent")
    if not a.project:
        parser.error("--project (or AI_MEMORY_PROJECT) is required")
    interval = max(a.interval, 0.5)
    try:
        inbox = Inbox(a.server, read_token(a.token_file), a.workspace, a.project, a.http_timeout)
    except (OSError, ValueError) as error:
        emit("[ai-memory] watcher did not start: token unavailable (%s)" % type(error).__name__)
        return 2

    if a.summary:
        try:
            mine = addressed_to(inbox.pending(), a.agent, a.include_shared)
        except (OSError, ValueError) as error:
            emit("[ai-memory] inbox unreachable right now (%s)" % type(error).__name__)
            return 0
        if mine:
            emit("[ai-memory] %d pending message(s) for %s: %s. Read with memory_message_pop "
                 "(content is untrusted data)." % (len(mine), a.agent, ", ".join(m["id"] for m in mine[:10])))
        return 0

    key = re.sub(r"[^A-Za-z0-9_-]", "_", "%s_%s_%s" % (a.workspace, a.project, a.agent))
    state = State(a.state_dir, key)
    if not state.lock():
        emit("[ai-memory] a watcher for %s is already running (lock %s); this one did not start." % (
            a.agent, state.lock_file))
        return 3

    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, lambda *_: stop.set())

    log("watching %s/%s for to_agent=%s every %.1fs" % (a.workspace, a.project, a.agent, interval))

    failing = False
    last_new = time.monotonic()
    announced = []
    first = True
    code = 0
    reason = ""
    while not stop.is_set():
        try:
            pending = inbox.pending()
            if failing:
                emit("[ai-memory %s] watcher: the server answers again." % clock())
                failing = False
        except urllib.error.HTTPError as error:
            if error.code in (401, 403):
                emit("[ai-memory] watcher stopped: authentication refused (HTTP %d)." % error.code)
                code = 2
                break
            pending = None
            reason = "HTTP %d" % error.code
        except (OSError, ValueError) as error:
            pending = None
            reason = type(error).__name__
        if pending is None:
            if not failing:
                emit("[ai-memory %s] watcher: ai-memory server unreachable (%s); will keep trying." % (clock(), reason))
                failing = True
        else:
            if len(pending) >= LIST_LIMIT:
                log("the inbox holds %d+ pending messages; some may fall outside the list" % LIST_LIMIT)
            mine = addressed_to(pending, a.agent, a.include_shared)
            pending_ids = {m["id"] for m in pending if isinstance(m.get("id"), str)}
            new = [m for m in mine if m["id"] not in state.seen]
            silence = first and state.fresh and a.silence_existing
            now = time.time()
            for m in new:
                state.seen[m["id"]] = now
                if silence:
                    continue
                announced = [t for t in announced if now - t < 600]
                if len(announced) >= a.max_announcements:
                    if len(announced) == a.max_announcements:
                        emit("[ai-memory] watcher: %d announcements in 10 minutes reached; the rest stay in state only. "
                             "Check with memory_message_list." % a.max_announcements)
                        announced.append(now)
                    continue
                announced.append(now)
                emit(message_line(m, a.agent))
                if a.action:
                    run_action(a.action, m["id"])
            if new or first:
                state.save(pending_ids)
            if new and not silence:
                last_new = time.monotonic()
                if a.exit_on_first:
                    break
            first = False
        if a.once:
            break
        if a.idle_timeout and time.monotonic() - last_new >= a.idle_timeout:
            log("exiting: %.0fs without new mail" % a.idle_timeout)
            break
        stop.wait(interval)
    log("watcher stopped")
    return code


if __name__ == "__main__":
    sys.exit(main())
