//! `ai-memory message watch --rewake`: the Claude Code `Stop` hook that wakes
//! an open, idle session when mail arrives (`docs/design-agent-bus.md`).
//!
//! Claude Code runs a `Stop` hook marked `asyncRewake` in the background when
//! a turn ends, and a hook that exits with code 2 wakes the session at once,
//! showing its stderr to the model as a system reminder. This mode is that
//! hook: it reads the session id from the hook's stdin, long-polls
//! `GET /inbox/watch` for the session's harness in its project, and exits 2
//! with the fixed wake text when mail arrives that may wake the session and
//! that this session was not told about yet. Otherwise it exits 0 when its
//! wait ends, and the next turn's `Stop` hook waits again.
//!
//! What may wake is decided by the server, as for the host supervisor
//! (`POST /inbox/wake-candidates`): mail from another agent of the project,
//! or a receipt. Mail from the session's own harness, from another project,
//! or between agents the loop guard paused never wakes it. What happened is
//! reported back (`POST /inbox/wake-report`) so the sender sees
//! `wake_attempted` or `wake_failed/loop_suspected`.
//!
//! The wake text is the fixed template of every wake; nothing a sender wrote
//! reaches stderr. Nothing else is printed: a hook's output is shown to the
//! model or the user.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ai_memory_core::message_wake::wake_text;
use ai_memory_core::{AgentKind, DeliveryReason, DeliveryState, MessageId};
use anyhow::Result;
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;

use super::message_supervise::{
    CandidateSender, CandidatesReply, CandidatesRequest, ReportReply, ReportRequest,
};
use super::message_watch::{canonical_message_id, refused_credential, transient, valid_cursor};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json_bounded, post_json_bounded};

/// The `timeout` the installed hook entry gives Claude Code, in seconds.
/// Claude Code kills an `asyncRewake` hook at its timeout; the hook ends its
/// own wait a minute earlier and exits 0.
pub(crate) const REWAKE_HOOK_TIMEOUT_SECS: u64 = 43_200;
/// Exit code that wakes the session.
const EXIT_WAKE: i32 = 2;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const REPLY_GRACE: Duration = Duration::from_secs(10);
const MAX_REPLY_BYTES: usize = 1 << 20;
/// Most bytes read from the hook's stdin.
const MAX_STDIN_BYTES: u64 = 1 << 20;
/// How long the hook waits for Claude Code to finish writing its stdin.
const STDIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest single wait; the server holds a request at most this long.
const MAX_WAIT: Duration = Duration::from_millis(ai_memory_hooks::MAX_INBOX_WATCH_WAIT_MS);
/// Retry delay while the server does not answer.
const RETRY: Duration = Duration::from_secs(30);
/// How long a session's state file is kept after its last use.
const STATE_RETENTION: Duration = Duration::from_secs(14 * 24 * 3_600);

#[derive(Debug, Deserialize)]
struct WatchReply {
    cursor: String,
    messages: Vec<WatchedMessage>,
}

#[derive(Debug, Deserialize)]
struct WatchedMessage {
    id: String,
}

/// Ids this session was already told about (or that may never wake it).
#[derive(Debug, Default, Deserialize, Serialize)]
struct Seen {
    seen: BTreeSet<String>,
}

/// The session id Claude Code put on the hook's stdin, when it is a
/// canonical UUID.
fn hook_session(raw: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(raw).ok()?;
    let id = value.get("session_id")?.as_str()?;
    let uuid = uuid::Uuid::parse_str(id).ok()?;
    (uuid.hyphenated().to_string() == id && !uuid.is_nil()).then(|| id.to_owned())
}

async fn read_stdin() -> Vec<u8> {
    let mut raw = Vec::new();
    let mut stdin = tokio::io::stdin().take(MAX_STDIN_BYTES);
    let _ = tokio::time::timeout(STDIN_TIMEOUT, stdin.read_to_end(&mut raw)).await;
    raw
}

/// Which of `new` may wake the session, and which the loop guard paused.
fn split_candidates(
    candidates: &CandidatesReply,
    new: &BTreeSet<String>,
) -> (Vec<String>, Vec<String>) {
    let mut wake = Vec::new();
    let mut paused = Vec::new();
    for candidate in &candidates.messages {
        let Some(id) = canonical_message_id(&candidate.id) else {
            continue;
        };
        if !new.contains(&id) {
            continue;
        }
        match candidate.sender {
            CandidateSender::SameProject | CandidateSender::System => wake.push(id),
            CandidateSender::Paused => paused.push(id),
            _ => {}
        }
    }
    (wake, paused)
}

/// Run the hook. Returns the process exit code: 2 to wake the session, 0
/// otherwise. Every failure ends quietly with 0, so a broken server never
/// shows an error in the session.
///
/// # Errors
/// Only an unusable state directory.
pub(crate) async fn run(
    config: &Config,
    agent: AgentKind,
    workspace: String,
    project: String,
    state_dir: PathBuf,
) -> Result<i32> {
    let Some(session) = hook_session(&read_stdin().await) else {
        return Ok(0);
    };
    std::fs::create_dir_all(&state_dir)?;
    prune_old_state(&state_dir);
    let lock_path = state_dir.join(format!("rewake-{session}.lock"));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    // A waiter from an earlier turn of this session still watches.
    if lock.try_lock_exclusive().is_err() {
        return Ok(0);
    }
    let Ok(endpoint) = ServerEndpoint::for_checkout(config).await else {
        return Ok(0);
    };
    let state_path = state_dir.join(format!("rewake-{session}.json"));
    let mut seen: Seen = std::fs::read(&state_path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default();
    let deadline =
        Instant::now() + Duration::from_secs(REWAKE_HOOK_TIMEOUT_SECS.saturating_sub(60));
    let watcher = Watcher {
        endpoint,
        agent,
        workspace,
        project,
    };
    let mut cursor: Option<String> = None;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(0);
        }
        let wait = if cursor.is_none() {
            Duration::ZERO
        } else {
            left.min(MAX_WAIT)
        };
        let reply = watcher.watch(cursor.as_deref(), wait).await;
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                if refused_credential(&error).is_some() || !transient(&error) {
                    return Ok(0);
                }
                cursor = None;
                tokio::time::sleep(RETRY.min(left)).await;
                continue;
            }
        };
        cursor = valid_cursor(&reply.cursor).map(str::to_owned);
        let pending: BTreeSet<String> = reply
            .messages
            .iter()
            .filter_map(|message| canonical_message_id(&message.id))
            .collect();
        let new: BTreeSet<String> = pending.difference(&seen.seen).cloned().collect();
        if new.is_empty() {
            continue;
        }
        let Ok(candidates) = watcher.candidates().await else {
            continue;
        };
        let (wake, paused) = split_candidates(&candidates, &new);
        // Every new id is settled for this session: it either wakes it now,
        // or it may never wake it.
        seen.seen.extend(new);
        seen.seen.retain(|id| pending.contains(id));
        if let Ok(bytes) = serde_json::to_vec(&seen) {
            let _ = ai_memory_wiki::write_atomic(&state_path, &bytes);
        }
        if !paused.is_empty() {
            watcher
                .report(
                    &paused,
                    DeliveryState::WakeFailed,
                    Some(DeliveryReason::LoopSuspected),
                )
                .await;
        }
        let Some(newest) = wake.last().and_then(|id| id.parse::<MessageId>().ok()) else {
            continue;
        };
        watcher
            .report(&wake, DeliveryState::WakeAttempted, None)
            .await;
        eprintln!(
            "{}",
            wake_text(
                agent,
                &watcher.workspace,
                &watcher.project,
                wake.len(),
                newest
            )
        );
        drop(lock);
        return Ok(EXIT_WAKE);
    }
}

struct Watcher {
    endpoint: ServerEndpoint,
    agent: AgentKind,
    workspace: String,
    project: String,
}

impl Watcher {
    async fn watch(&self, after: Option<&str>, wait: Duration) -> Result<WatchReply> {
        let wait_ms = wait.as_millis().to_string();
        let mut query = vec![
            ("workspace", self.workspace.as_str()),
            ("project", self.project.as_str()),
            ("agent", self.agent.as_str()),
            ("include_shared", "false"),
            ("wait_ms", wait_ms.as_str()),
        ];
        if let Some(after) = after {
            query.push(("after", after));
        }
        get_json_bounded(
            &self.endpoint,
            "/inbox/watch",
            &query,
            wait + REPLY_GRACE,
            MAX_REPLY_BYTES,
        )
        .await
    }

    async fn candidates(&self) -> Result<CandidatesReply> {
        post_json_bounded(
            &self.endpoint,
            "/inbox/wake-candidates",
            &CandidatesRequest {
                workspace: &self.workspace,
                project: &self.project,
                agent: self.agent.as_str(),
                include_shared: false,
                wake_from: &[],
            },
            REQUEST_TIMEOUT,
            MAX_REPLY_BYTES,
        )
        .await
    }

    /// Best effort: a sender's delivery state is a convenience, the wake is
    /// what matters.
    async fn report(&self, ids: &[String], state: DeliveryState, reason: Option<DeliveryReason>) {
        let _: Result<ReportReply> = post_json_bounded(
            &self.endpoint,
            "/inbox/wake-report",
            &ReportRequest {
                workspace: &self.workspace,
                project: &self.project,
                agent: self.agent.as_str(),
                message_ids: ids,
                state,
                reason,
                retry_after_secs: None,
            },
            REQUEST_TIMEOUT,
            MAX_REPLY_BYTES,
        )
        .await;
    }
}

/// Remove the state files of sessions not seen for two weeks.
fn prune_old_state(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("rewake-") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age > STATE_RETENTION);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0190f0a4-5c1e-7a2b-9c3d-4e5f60718293";

    #[test]
    fn the_session_comes_from_the_hook_payload_as_a_canonical_uuid() {
        let payload = format!(
            r#"{{"session_id":"{ID}","hook_event_name":"Stop","cwd":"/x","stop_hook_active":false}}"#
        );
        assert_eq!(hook_session(payload.as_bytes()).as_deref(), Some(ID));
        for bad in [
            "",
            "{}",
            r#"{"session_id":"not-a-uuid"}"#,
            r#"{"session_id":"0190F0A4-5C1E-7A2B-9C3D-4E5F60718293"}"#,
            r#"{"session_id":"00000000-0000-0000-0000-000000000000"}"#,
            r#"{"session_id":12}"#,
        ] {
            assert_eq!(hook_session(bad.as_bytes()), None, "{bad:?}");
        }
    }

    #[test]
    fn only_new_mail_from_another_agent_or_a_receipt_wakes() {
        let id = |n: u8| format!("0190f0a4-5c1e-7a2b-9c3d-4e5f607182{n:02}");
        let reply: CandidatesReply = serde_json::from_value(serde_json::json!({
            "messages": [
                {"id": id(1), "sender": "same_project"},
                {"id": id(2), "sender": "same_agent"},
                {"id": id(3), "sender": "paused"},
                {"id": id(4), "sender": "system"},
                {"id": id(5), "sender": "unlisted"},
                {"id": id(6), "sender": "a_label_from_a_newer_server"},
                {"id": id(7), "sender": "same_project"},
            ],
            "unresolved_wake_from": 0,
        }))
        .unwrap();
        let new: BTreeSet<String> = (1..=6).map(id).collect();
        let (wake, paused) = split_candidates(&reply, &new);
        assert_eq!(wake, [id(1), id(4)], "id 7 was already announced");
        assert_eq!(paused, [id(3)]);
    }
}
