"""Tests for scripts/agent-inbox-watch.py against a stub /admin/messages server."""
import importlib.util
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "agent-inbox-watch.py"
SPEC = importlib.util.spec_from_file_location("agent_inbox_watch", SCRIPT)
watch = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(watch)

ID_A = "01a1dafe-097e-7981-b430-7caa76d394bd"
ID_B = "01a1dafe-097e-7981-b430-7caa76d394be"
ID_C = "01a1dafe-097e-7981-b430-7caa76d394bf"
TOKEN = "test-token"


def message(message_id, to_agent, subject="hello", state="pending", body="SECRET BODY"):
    return {"id": message_id, "to_agent": to_agent, "from_agent": "codex",
            "subject": subject, "body": body, "state": state}


class Stub:
    """Serves one mutable inbox; records the query and the bearer it saw."""

    def __init__(self):
        self.messages = []
        self.status = 200
        self.seen_queries = []
        stub = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                stub.seen_queries.append(self.path)
                if self.headers.get("Authorization") != "Bearer " + TOKEN:
                    self.send_response(401)
                    self.end_headers()
                    return
                if stub.status != 200:
                    self.send_response(stub.status)
                    self.end_headers()
                    return
                payload = json.dumps({"messages": stub.messages}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, *args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = "http://127.0.0.1:%d" % self.server.server_address[1]

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class WatcherTest(unittest.TestCase):
    def setUp(self):
        self.stub = Stub()
        self.addCleanup(self.stub.close)
        self.state = tempfile.TemporaryDirectory()
        self.addCleanup(self.state.cleanup)

    def run_watch(self, *extra, agent="claude-code", token=TOKEN, timeout=20):
        env = dict(os.environ, AI_MEMORY_AUTH_TOKEN=token)
        command = [sys.executable, "-B", str(SCRIPT), "--agent", agent, "--project", "demo",
                   "--server", self.stub.url, "--state-dir", self.state.name, "--interval", "0.5", *extra]
        return subprocess.run(command, capture_output=True, text=True, env=env, timeout=timeout)

    def test_announces_only_messages_for_the_agent_and_never_the_body(self):
        self.stub.messages = [message(ID_A, "claude-code", "from codex"), message(ID_B, "codex"),
                              message(ID_C, "claude-code", state="claimed")]
        result = self.run_watch("--once")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(ID_A, result.stdout)
        self.assertNotIn(ID_B, result.stdout)
        self.assertNotIn(ID_C, result.stdout)
        self.assertNotIn("SECRET BODY", result.stdout)
        self.assertEqual(len(result.stdout.strip().splitlines()), 1)

    def test_queries_the_requested_inbox_with_the_bearer(self):
        self.run_watch("--once")
        query = self.stub.seen_queries[0]
        self.assertIn("project=demo", query)
        self.assertIn("box=inbox", query)

    def test_second_run_does_not_announce_the_same_message_again(self):
        self.stub.messages = [message(ID_A, "claude-code")]
        self.assertIn(ID_A, self.run_watch("--once").stdout)
        self.assertEqual(self.run_watch("--once").stdout, "")

    def test_silence_existing_marks_current_mail_seen_on_the_first_run_only(self):
        self.stub.messages = [message(ID_A, "claude-code")]
        self.assertEqual(self.run_watch("--once", "--silence-existing").stdout, "")
        self.stub.messages.append(message(ID_B, "claude-code"))
        out = self.run_watch("--once", "--silence-existing").stdout
        self.assertIn(ID_B, out)
        self.assertNotIn(ID_A, out)

    def test_exit_on_first_returns_after_a_message_arrives(self):
        self.stub.messages = []
        timer = threading.Timer(1.0, lambda: self.stub.messages.append(message(ID_A, "claude-code")))
        timer.start()
        self.addCleanup(timer.cancel)
        result = self.run_watch("--exit-on-first")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(ID_A, result.stdout)

    def test_summary_counts_without_touching_state(self):
        self.stub.messages = [message(ID_A, "claude-code")]
        out = self.run_watch("--summary").stdout
        self.assertIn("1 pending message(s)", out)
        self.assertEqual(list(Path(self.state.name).iterdir()), [])

    def test_shared_mail_needs_the_flag(self):
        self.stub.messages = [message(ID_A, None)]
        self.assertEqual(self.run_watch("--once").stdout, "")
        self.assertIn(ID_A, self.run_watch("--once", "--include-shared").stdout)

    def test_refused_token_exits_with_code_2(self):
        result = self.run_watch("--once", token="wrong")
        self.assertEqual(result.returncode, 2)
        self.assertIn("authentication refused", result.stdout)

    def test_second_watcher_for_the_same_agent_is_refused(self):
        lock_holder = watch.State(self.state.name, "default_demo_claude-code")
        self.assertTrue(lock_holder.lock())
        self.addCleanup(lock_holder._lock.close)
        result = self.run_watch("--once")
        self.assertEqual(result.returncode, 3)
        self.assertIn("already running", result.stdout)

    def test_exec_runs_without_a_shell_and_substitutes_only_a_valid_id(self):
        marker = Path(self.state.name) / "ran.txt"
        self.stub.messages = [message(ID_A, "claude-code")]
        template = "%s -c 'import sys,pathlib; pathlib.Path(sys.argv[1]).write_text(sys.argv[2])' %s {id}" % (
            sys.executable, marker)
        self.run_watch("--once", "--exec", template)
        self.assertEqual(marker.read_text(), ID_A)

    def free_port(self):
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            return probe.getsockname()[1]

    def start_with_webhook(self, port, secret):
        env = dict(os.environ, AI_MEMORY_AUTH_TOKEN=TOKEN)
        command = [sys.executable, "-B", str(SCRIPT), "--agent", "claude-code", "--project", "demo",
                   "--server", self.stub.url, "--state-dir", self.state.name, "--interval", "60",
                   "--exit-on-first", "--webhook-listen", "127.0.0.1:%d" % port, "--webhook-secret", secret]
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
        self.addCleanup(self.reap, process)
        deadline = time.time() + 10
        while time.time() < deadline:
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                    return process
            except OSError:
                time.sleep(0.1)
        self.fail("webhook receiver did not start")

    @staticmethod
    def reap(process):
        if process.poll() is None:
            process.kill()
        process.communicate()

    def post(self, port, path):
        request = urllib.request.Request("http://127.0.0.1:%d%s" % (port, path), data=b"{}", method="POST")
        try:
            with urllib.request.urlopen(request, timeout=5) as response:
                return response.status
        except urllib.error.HTTPError as error:
            error.close()
            return error.code

    def test_webhook_post_triggers_an_immediate_poll_despite_a_long_interval(self):
        port = self.free_port()
        process = self.start_with_webhook(port, "s3cret")
        time.sleep(0.5)
        self.stub.messages = [message(ID_A, "claude-code")]
        self.assertEqual(self.post(port, "/s3cret"), 204)
        out, _ = process.communicate(timeout=10)
        self.assertEqual(process.returncode, 0)
        self.assertIn(ID_A, out)

    def test_webhook_with_the_wrong_path_does_not_wake_the_loop(self):
        port = self.free_port()
        process = self.start_with_webhook(port, "s3cret")
        time.sleep(0.5)
        self.stub.messages = [message(ID_A, "claude-code")]
        self.assertEqual(self.post(port, "/wrong"), 404)
        time.sleep(1.5)
        self.assertIsNone(process.poll())

    def test_webhook_listen_needs_host_and_port(self):
        result = self.run_watch("--once", "--webhook-listen", "nonsense")
        self.assertEqual(result.returncode, 2)
        self.assertIn("HOST:PORT", result.stderr)

    def test_clean_text_removes_controls_quotes_and_truncates(self):
        dirty = 'a\x00b\n"c"‮' + "x" * 300
        cleaned = watch.clean_text(dirty, 20)
        self.assertEqual(len(cleaned), 20)
        self.assertNotIn("\n", cleaned)
        self.assertNotIn('"', cleaned)
        self.assertNotIn("‮", cleaned)


if __name__ == "__main__":
    unittest.main()
