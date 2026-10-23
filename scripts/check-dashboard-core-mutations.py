#!/usr/bin/env python3
"""Prove focused Rust tests reject semantic faults; restore sources in finally.

Run alone, with no concurrent builds. This touches only clean tracked source
files, backs up their exact bytes, and never writes production data. A compiler
error, a failed baseline or a timeout does not count as a killed mutation.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
TARGET = ROOT / "target"
TARGET.mkdir(exist_ok=True)
OUT = Path(tempfile.mkdtemp(prefix="dashboard-core-mutations-", dir=TARGET))
LOCK = TARGET / "dashboard-core-mutations.lock"
REPORT = {"mutants": [], "complete": False}

CASES = [
    {
        "name": "expose-restricted-mailbox-projects", "file": "crates/ai-memory-web/src/routes/inbox.rs",
        "start": "pub(crate) async fn handler(", "end": "#[derive(Deserialize)]",
        "before": "list_projects_with_stats(viewer)", "after": "list_projects_with_stats(None)",
        "command": ["cargo", "test", "-p", "ai-memory-web", "--lib", "operations_inbox_queue_context_hides_foreign_projects_and_preserves_mail"],
    },
    {
        "name": "capture-only-at-session-end", "file": "crates/ai-memory-cli/src/commands/hook.rs",
        "start": 'if matches!(\n        args.event.as_str(),',
        "end": '    // Token-usage visibility',
        "before": '"post-tool-use" | "stop" | "pre-compact" | "session-end"', "after": '"session-end"',
        "command": ["cargo", "test", "-p", "ai-memory-cli", "--test", "suite", "native_hook_reports_live_usage_for_every_supported_event_and_harness"],
    },
    {
        "name": "count-codex-cache-as-fresh-input", "file": "crates/ai-memory-cli/src/commands/session_usage.rs",
        "start": "pub fn extract_live_calls(", "end": "/// Cumulative token usage",
        "before": "input.saturating_sub(cached)", "after": "input",
        "command": ["cargo", "test", "-p", "ai-memory-cli", "--lib", "live_token_codex_reads_modern_usage_without_counting_cached_input_twice"],
    },
    {
        "name": "let-replayed-counters-decrease", "file": "crates/ai-memory-store/src/ops.rs",
        "start": "pub fn upsert_token_calls(", "end": "\npub fn ",
        "before": "output_tokens = MAX(output_tokens, excluded.output_tokens)", "after": "output_tokens = excluded.output_tokens",
        "command": ["cargo", "test", "-p", "ai-memory-store", "--lib", "token_calls_keep_partial_reports_monotonic_in_rows_and_aggregates"],
    },
    {
        "name": "allow-the-wrong-harness-to-claim", "file": "crates/ai-memory-store/src/ops.rs",
        "start": "pub(crate) fn pop_message_in_transaction(", "end": "/// Cancel",
        "replacements": [("(to_agent IS NULL OR to_agent = ?3)", "(?3 IS NOT NULL)", 2), ("(to_agent IS NULL OR to_agent = ?4)", "(?4 IS NOT NULL)", 1)],
        "command": ["cargo", "test", "-p", "ai-memory-store", "--lib", "addressed_messages_are_claimed_once_only_by_the_selected_harness"],
    },
]

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--case", choices=[case["name"] for case in CASES])
selected = parser.parse_args().case
if selected:
    CASES = [case for case in CASES if case["name"] == selected]


def run(case, label):
    log = OUT / (case["name"] + "-" + label + ".log")
    with log.open("w") as output:
        result = subprocess.run(case["command"], cwd=ROOT, stdout=output, stderr=subprocess.STDOUT, timeout=300)
    return result.returncode, log.read_text(errors="replace")


def replace_bytes(path, contents):
    staged = path.with_name(path.name + ".mutation-tmp")
    try:
        staged.write_bytes(contents)
        os.chmod(staged, path.stat().st_mode & 0o777)
        os.replace(staged, path)
    finally:
        staged.unlink(missing_ok=True)


with LOCK.open("x"):
    try:
        for case in CASES:
            path = ROOT / case["file"]
            subprocess.run(["git", "diff", "--exit-code", "HEAD", "--", case["file"]], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
            original = path.read_bytes()
            (OUT / (case["name"] + ".original")).write_bytes(original)
            text = original.decode()
            start = text.index(case["start"])
            end = text.index(case["end"], start)
            section = text[start:end]
            for before, after, count in case.get("replacements", [(case.get("before"), case.get("after"), 1)]):
                assert section.count(before) == count, case["name"] + ": source anchor drifted"
                section = section.replace(before, after)
            changed = (text[:start] + section + text[end:]).encode()
            code, _ = run(case, "baseline")
            assert code == 0, case["name"] + ": baseline failed"
            try:
                replace_bytes(path, changed)
                code, output = run(case, "mutant")
                assert code != 0 and "test result: FAILED." in output, case["name"] + ": mutation survived or failed outside a test"
                assert case["command"][-1] + " ... FAILED" in output, case["name"] + ": unrelated test failed"
                REPORT["mutants"].append({"name": case["name"], "killed": True, "test": case["command"][-1]})
                print("KILLED " + case["name"], flush=True)
            finally:
                assert path.read_bytes() == changed, "Source changed concurrently; original backup: " + str(OUT)
                replace_bytes(path, original)
        # Every changed file must be back to its exact committed content.
        subprocess.run(["git", "diff", "--exit-code", "HEAD", "--", *sorted({c["file"] for c in CASES})], cwd=ROOT, check=True)
    except Exception as error:
        REPORT["error"] = str(error)
        raise
    finally:
        try:
            # Source restoration alone leaves the last deliberately broken
            # executable in target/. Rebuild before another test consumes it.
            subprocess.run(["git", "diff", "--exit-code", "HEAD", "--", *sorted({c["file"] for c in CASES})], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
            with (OUT / "restore-build.log").open("w") as log:
                subprocess.run(["cargo", "build", "--locked", "--bin", "ai-memory"], cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, timeout=300, check=True)
            REPORT["executable_restored"] = True
            REPORT["complete"] = len(REPORT["mutants"]) == len(CASES) and "error" not in REPORT
        except Exception as error:
            REPORT["restore_error"] = str(error)
            raise
        finally:
            (OUT / "report.json").write_text(json.dumps(REPORT, indent=2) + "\n")
            LOCK.unlink(missing_ok=True)
            print("Core mutation report: " + str(OUT / "report.json"), flush=True)
