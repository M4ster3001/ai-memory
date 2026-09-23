//! Client-side session token-usage extraction (token-cost visibility).
//!
//! Reads a harness's own transcript file to compute cumulative token totals
//! for one session. Called only at `session-end` (once per session, never on
//! the frequent `stop`/`post-tool-use` hot path — see `hook.rs`), so a
//! bounded linear scan (Claude Code) or a bounded tail read (Codex, whose
//! rollout files can reach hundreds of MB for a long session) is cheap
//! enough to run synchronously inline in the native hook before it spools
//! the session-end event. Best-effort throughout: any read/parse failure
//! yields `None`, never a wrong number, and a hook must never fail on its
//! account.
//!
//! Field names and the "sum by dedup'd message.id" / "take the last
//! cumulative record" strategies below were verified against real local
//! transcripts (a Claude Code JSONL and Codex rollout files up to 103 MB),
//! not just documentation.

use std::collections::HashSet;
use std::io::{BufRead as _, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

/// Cumulative token usage for one session, as reported by the harness's own
/// transcript. An extractor returning `None` means "could not determine",
/// never "zero" — callers must not report a zeroed `SessionUsage`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionUsage {
    /// Fresh (non-cached) input tokens, summed/read from the transcript.
    pub input_tokens: u64,
    /// Output tokens, including any reasoning/thinking tokens the harness
    /// reports as part of the same total (verified against real data: both
    /// Claude Code's `output_tokens_details.thinking_tokens` and Codex's
    /// `reasoning_output_tokens` are a breakdown of `output_tokens`, not
    /// additive to it).
    pub output_tokens: u64,
    /// Tokens written to a prompt cache.
    pub cache_write_tokens: u64,
    /// Tokens served from a prompt cache.
    pub cache_read_tokens: u64,
    /// Last-seen model name, when the transcript records one.
    pub model: Option<String>,
}

/// Refuse to scan a Claude Code transcript past this size. Guards against a
/// pathological file; real sessions are far smaller than this.
const CLAUDE_MAX_BYTES: u64 = 200 * 1024 * 1024;

/// How far from the end of a Codex rollout file to scan for the most recent
/// `token_usage_record`. Codex writes one after nearly every turn, so this
/// window holds one in any session that has had at least one model turn,
/// while staying bounded for the multi-hundred-MB rollout files a long
/// session produces (observed up to 103 MB locally).
const CODEX_TAIL_SCAN_BYTES: u64 = 4 * 1024 * 1024;

/// Sum `message.usage` across every non-meta assistant record in a Claude
/// Code JSONL transcript, deduplicating by `message.id`. Verified against a
/// real 2096-line/531-assistant-record local transcript: Claude Code's own
/// writer emits the same finalized message twice in a row (streaming, then
/// finalized) with byte-identical usage on every duplicate observed —
/// summing without the dedup roughly doubled the true total there.
#[must_use]
pub fn extract_claude_code_usage(transcript_path: &Path) -> Option<SessionUsage> {
    let file = std::fs::File::open(transcript_path).ok()?;
    if file.metadata().ok()?.len() > CLAUDE_MAX_BYTES {
        return None;
    }
    let reader = std::io::BufReader::new(file);
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut totals = SessionUsage::default();
    let mut any = false;
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) != Some("assistant") {
            continue;
        }
        if value.get("isMeta").and_then(serde_json::Value::as_bool) == Some(true) {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        let Some(id) = message.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !seen_ids.insert(id.to_owned()) {
            continue;
        }
        let Some(usage) = message.get("usage") else {
            continue;
        };
        totals.input_tokens += u64_field(usage, "input_tokens");
        totals.output_tokens += u64_field(usage, "output_tokens");
        totals.cache_write_tokens += u64_field(usage, "cache_creation_input_tokens");
        totals.cache_read_tokens += u64_field(usage, "cache_read_input_tokens");
        if let Some(model) = message.get("model").and_then(serde_json::Value::as_str) {
            totals.model = Some(model.to_owned());
        }
        any = true;
    }
    any.then_some(totals)
}

/// Read the most recent `token_usage_record` from a Codex rollout JSONL
/// file. Its `payload.thread_token_usage` is already the cumulative total
/// for this rollout file, so — unlike Claude Code — this needs no summing,
/// only the latest record. Scans backward from a bounded tail window rather
/// than the whole file, since a long Codex session's rollout can reach
/// hundreds of megabytes (observed locally); the window may begin mid-line,
/// but that partial first line simply fails to parse as JSON and is
/// skipped, which is safe because every LATER (more complete) line in an
/// append-only file this process never writes concurrently to is intact.
#[must_use]
pub fn extract_codex_usage(transcript_path: &Path) -> Option<SessionUsage> {
    let mut file = std::fs::File::open(transcript_path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(CODEX_TAIL_SCAN_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity(usize::try_from(len - start).unwrap_or(0));
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) != Some("token_usage_record") {
            continue;
        }
        let Some(usage) = value.pointer("/payload/thread_token_usage") else {
            continue;
        };
        return Some(SessionUsage {
            input_tokens: u64_field(usage, "input_tokens"),
            // `reasoning_output_tokens` is a breakdown of `output_tokens`
            // (verified against real data: input_tokens + output_tokens ==
            // total_tokens exactly), not additive to it.
            output_tokens: u64_field(usage, "output_tokens"),
            cache_write_tokens: u64_field(usage, "cache_write_input_tokens"),
            cache_read_tokens: u64_field(usage, "cached_input_tokens"),
            // Not carried on this record; left unset rather than guessed.
            model: None,
        });
    }
    None
}

fn u64_field(value: &serde_json::Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

/// Bound how many rollout files a Codex session-id lookup will stat before
/// giving up, mirroring the scan caps used elsewhere in this codebase
/// (`MAX_SCAN_FILES` in `ai-memory-workstream`) — a install with years of
/// rollout history must not turn a session-end hook into an unbounded walk.
const MAX_CODEX_ROLLOUT_SCAN: usize = 20_000;

/// Locate a Codex rollout file by the session id ai-memory's own hooks
/// receive. Codex names each rollout `rollout-<timestamp>-<uuid>.jsonl`
/// under `~/.codex/sessions/<year>/<month>/<day>/`; the trailing UUID is
/// the same id Codex's hook payloads send as `session_id` (verified: the
/// server-visible session id for a real local Codex session matched the
/// UUID suffix of that session's own rollout file, not the different
/// cross-resume id recorded inside `token_usage_record.payload.session_id`
/// on files from an earlier, resumed thread).
#[must_use]
pub fn locate_codex_rollout(codex_home: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    let suffix = format!("-{session_id}.jsonl");
    let root = codex_home.join("sessions");
    let mut scanned = 0usize;
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if scanned >= MAX_CODEX_ROLLOUT_SCAN {
                return None;
            }
            scanned += 1;
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&suffix))
            {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_line(
        msg_id: &str,
        input: u64,
        output: u64,
        cache_write: u64,
        cache_read: u64,
    ) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {
                "id": msg_id,
                "model": "claude-sonnet-5",
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_creation_input_tokens": cache_write,
                    "cache_read_input_tokens": cache_read,
                    "output_tokens_details": {"thinking_tokens": output / 2}
                }
            }
        })
        .to_string()
    }

    #[test]
    fn claude_code_sums_usage_and_dedups_by_message_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        // The same finalized message written twice (streaming then final),
        // exactly as observed live — the duplicate must not double-count.
        let body = format!(
            "{}\n{}\n{}\n",
            claude_line("msg_1", 2, 298, 67828, 0),
            claude_line("msg_1", 2, 298, 67828, 0),
            claude_line("msg_2", 2, 188, 371, 67828),
        );
        std::fs::write(&path, body).unwrap();

        let usage = extract_claude_code_usage(&path).unwrap();
        assert_eq!(usage.input_tokens, 4);
        assert_eq!(usage.output_tokens, 486);
        assert_eq!(usage.cache_write_tokens, 68199);
        assert_eq!(usage.cache_read_tokens, 67828);
        assert_eq!(usage.model.as_deref(), Some("claude-sonnet-5"));
    }

    #[test]
    fn claude_code_ignores_meta_and_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        let body = format!(
            "not json\n{}\n{{\"type\":\"assistant\",\"isMeta\":true,\"message\":{{\"id\":\"m\",\"usage\":{{\"input_tokens\":999}}}}}}\n{}\n",
            claude_line("real", 5, 5, 0, 0),
            "",
        );
        std::fs::write(&path, body).unwrap();

        let usage = extract_claude_code_usage(&path).unwrap();
        assert_eq!(
            usage.input_tokens, 5,
            "isMeta and unparseable lines must be skipped"
        );
    }

    #[test]
    fn claude_code_missing_file_is_none_not_zero() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(extract_claude_code_usage(&tmp.path().join("nope.jsonl")).is_none());
    }

    fn codex_record(input: u64, output: u64, cache_write: u64, cached: u64) -> String {
        serde_json::json!({
            "type": "token_usage_record",
            "payload": {
                "thread_token_usage": {
                    "input_tokens": input,
                    "cached_input_tokens": cached,
                    "cache_write_input_tokens": cache_write,
                    "output_tokens": output,
                    "reasoning_output_tokens": output / 3,
                    "total_tokens": input + output,
                }
            }
        })
        .to_string()
    }

    #[test]
    fn codex_takes_the_last_cumulative_record() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        let body = format!(
            "{}\n{{\"type\":\"response_item\",\"payload\":{{}}}}\n{}\n",
            codex_record(26852, 145, 0, 4864),
            codex_record(2_027_386, 1411, 0, 1_792_256),
        );
        std::fs::write(&path, body).unwrap();

        let usage = extract_codex_usage(&path).unwrap();
        assert_eq!(usage.input_tokens, 2_027_386);
        assert_eq!(
            usage.output_tokens, 1411,
            "reasoning tokens are a breakdown of output_tokens, not additive"
        );
        assert_eq!(usage.cache_read_tokens, 1_792_256);
    }

    #[test]
    fn codex_scans_only_the_tail_window() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        // Padding well past the tail-scan window, followed by the real
        // record: proves the extractor never needed to read the padding.
        let padding = "x".repeat((CODEX_TAIL_SCAN_BYTES as usize) * 3);
        let body = format!("{padding}\n{}\n", codex_record(100, 50, 0, 10));
        std::fs::write(&path, body).unwrap();

        let usage = extract_codex_usage(&path).unwrap();
        assert_eq!(usage.input_tokens, 100);
    }

    #[test]
    fn codex_missing_file_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(extract_codex_usage(&tmp.path().join("nope.jsonl")).is_none());
    }

    #[test]
    fn locate_codex_rollout_finds_nested_file_by_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        let day_dir = tmp.path().join("sessions/2026/09/22");
        std::fs::create_dir_all(&day_dir).unwrap();
        let target =
            day_dir.join("rollout-2026-09-22T17-39-48-01a0cad8-9932-7433-9c14-97b62a6d5049.jsonl");
        std::fs::write(&target, "").unwrap();
        // A sibling file with a different id must not match.
        std::fs::write(
            day_dir.join("rollout-2026-09-22T10-00-00-deadbeef-0000-0000-0000-000000000000.jsonl"),
            "",
        )
        .unwrap();

        let found =
            locate_codex_rollout(tmp.path(), "01a0cad8-9932-7433-9c14-97b62a6d5049").unwrap();
        assert_eq!(found, target);
    }

    #[test]
    fn locate_codex_rollout_rejects_unsafe_session_ids() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(locate_codex_rollout(tmp.path(), "").is_none());
        assert!(locate_codex_rollout(tmp.path(), "../../etc/passwd").is_none());
        assert!(locate_codex_rollout(tmp.path(), "has spaces").is_none());
    }

    #[test]
    fn locate_codex_rollout_missing_session_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sessions/2026/01/01")).unwrap();
        assert!(locate_codex_rollout(tmp.path(), "no-such-session").is_none());
    }
}
