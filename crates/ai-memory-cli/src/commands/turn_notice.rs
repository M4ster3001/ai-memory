//! The agent bus's mid-turn mail notice (`docs/agent-bus/`).
//!
//! An agent busy in a long turn is not woken by the bus: a Codex chat runs
//! a queued turn only when it is idle, and a Claude Code `Stop` hook only
//! fires when the turn ends. But both harnesses run `PreToolUse` and
//! `PostToolUse` hooks around every tool call, and both read
//! `hookSpecificOutput.additionalContext` from them into the model's context.
//! The native hook therefore checks, at most every [`CHECK_INTERVAL_SECS`],
//! whether mail arrived that may interrupt the agent, and adds one fixed line
//! ([`turn_notice_text`]) at most every [`NOTICE_INTERVAL_SECS`], once per
//! message. It never carries a subject, body or sender, and it never asks a
//! model anything.
//!
//! The check costs one local request with a [`CHECK_TIMEOUT`] budget; a
//! server that does not answer in time leaves the tool call unchanged.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ai_memory_core::AgentKind;
use ai_memory_core::message_wake::turn_notice_text;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Shortest gap between two server checks of one session.
pub(crate) const CHECK_INTERVAL_SECS: i64 = 5;
/// Shortest gap between two notices to one session.
pub(crate) const NOTICE_INTERVAL_SECS: i64 = 15;
/// Budget of one check, so a slow server never delays the tool call.
pub(crate) const CHECK_TIMEOUT: Duration = Duration::from_millis(300);
/// Most ids remembered per session.
const MAX_SEEN: usize = 512;

/// What one session was told, kept beside the hook spool.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TurnNoticeState {
    #[serde(default)]
    checked_at: i64,
    #[serde(default)]
    announced_at: i64,
    #[serde(default)]
    seen: BTreeSet<String>,
}

impl TurnNoticeState {
    /// Whether a check may run at `now`: neither a check nor a notice too
    /// recent.
    pub(crate) fn due(&self, now: i64) -> bool {
        now.saturating_sub(self.checked_at) >= CHECK_INTERVAL_SECS
            && now.saturating_sub(self.announced_at) >= NOTICE_INTERVAL_SECS
    }

    /// Record a check at `now` and return the ids among `pending` never
    /// announced to this session, oldest first. Ids no longer pending are
    /// forgotten, so the state stays small.
    pub(crate) fn take_new(&mut self, pending: &[String], now: i64) -> Vec<String> {
        self.checked_at = now;
        let new: Vec<String> = pending
            .iter()
            .filter(|id| !self.seen.contains(*id))
            .cloned()
            .collect();
        self.seen = pending.iter().cloned().collect();
        while self.seen.len() > MAX_SEEN {
            let first = self.seen.iter().next().cloned();
            if let Some(first) = first {
                self.seen.remove(&first);
            }
        }
        if !new.is_empty() {
            self.announced_at = now;
        }
        new
    }
}

/// The server's answer to `GET /handoff?inbox_turn=1`.
#[derive(Debug, Deserialize)]
struct TurnNoticeReply {
    workspace: String,
    project: String,
    ids: Vec<String>,
}

/// Where a session's state lives: one small file per harness and session
/// under the data directory. The name is a hash, so no session id or path
/// becomes a file name.
pub(crate) fn state_path(data_dir: &Path, agent: &str, session: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    for part in [agent, session] {
        hasher.update(part.len().to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let digest: String = hasher.finalize()[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    data_dir
        .join("inbox-watch")
        .join(format!("turn-{digest}.json"))
}

pub(crate) fn load(path: &Path) -> TurnNoticeState {
    std::fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

pub(crate) fn save(path: &Path, state: &TurnNoticeState) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec(state) {
        let _ = ai_memory_wiki::write_atomic(path, &bytes);
    }
}

/// The notice for a server answer, or `None` when it names nothing new.
/// Updates `state`; only canonical message ids count.
pub(crate) fn notice_for(
    state: &mut TurnNoticeState,
    agent: AgentKind,
    body: &str,
    now: i64,
) -> Option<String> {
    let Ok(reply) = serde_json::from_str::<TurnNoticeReply>(body) else {
        state.checked_at = now;
        return None;
    };
    let pending: Vec<String> = reply
        .ids
        .iter()
        .filter_map(|id| super::message_watch::canonical_message_id(id))
        .collect();
    let new = state.take_new(&pending, now);
    let newest = new.last()?.parse().ok()?;
    Some(turn_notice_text(
        agent,
        &reply.workspace,
        &reply.project,
        new.len(),
        newest,
    ))
}

/// The hook output that hands `notice` to the model: the
/// `hookSpecificOutput.additionalContext` both Codex and Claude Code read
/// from `PreToolUse` and `PostToolUse`.
pub(crate) fn envelope(event_name: &str, notice: &str) -> serde_json::Value {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event_name,
            "additionalContext": notice,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> String {
        format!("0190f0a4-5c1e-7a2b-9c3d-4e5f607182{n:02}")
    }

    #[test]
    fn checks_are_spaced_and_notices_are_rate_limited() {
        let mut state = TurnNoticeState::default();
        let t0 = 1_000_000;
        assert!(state.due(t0));
        assert_eq!(state.take_new(&[id(1)], t0), [id(1)]);
        assert!(
            !state.due(t0 + CHECK_INTERVAL_SECS),
            "a notice holds the next one"
        );
        assert!(!state.due(t0 + NOTICE_INTERVAL_SECS - 1));
        assert!(state.due(t0 + NOTICE_INTERVAL_SECS));
        // A check that found nothing new only spaces the next check.
        let t1 = t0 + NOTICE_INTERVAL_SECS;
        assert!(state.take_new(&[id(1)], t1).is_empty());
        assert!(!state.due(t1 + CHECK_INTERVAL_SECS - 1));
        assert!(state.due(t1 + CHECK_INTERVAL_SECS));
    }

    #[test]
    fn each_message_is_announced_once_and_popped_ids_are_forgotten() {
        let mut state = TurnNoticeState::default();
        assert_eq!(state.take_new(&[id(1), id(2)], 1), [id(1), id(2)]);
        assert_eq!(state.take_new(&[id(1), id(2), id(3)], 100), [id(3)]);
        assert!(state.take_new(&[id(3)], 200).is_empty());
        assert_eq!(state.seen.len(), 1, "popped ids are dropped");
    }

    #[test]
    fn the_notice_names_only_counts_ids_and_the_inbox() {
        let mut state = TurnNoticeState::default();
        let body = format!(
            r#"{{"workspace":"default","project":"linha","ids":["{}","not-an-id","{}"]}}"#,
            id(1),
            id(2)
        );
        let notice = notice_for(&mut state, AgentKind::Codex, &body, 10).unwrap();
        assert!(
            notice.starts_with("ai-memory: 2 new message(s) arrived for codex in default/linha"),
            "{notice}"
        );
        assert!(notice.contains(&id(2)));
        assert!(notice_for(&mut state, AgentKind::Codex, &body, 100).is_none());
        // An older server answers the count-only text: no notice.
        let mut fresh = TurnNoticeState::default();
        assert!(notice_for(&mut fresh, AgentKind::Codex, "📬 ai-memory: 2 messages", 5).is_none());
        assert!(!fresh.due(6), "the failed check still spaces the next one");
        let out = envelope("PostToolUse", &notice);
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert_eq!(out["hookSpecificOutput"]["additionalContext"], notice);
    }

    #[test]
    fn state_files_are_hashed_per_agent_and_session() {
        let dir = Path::new("/d");
        let a = state_path(dir, "codex", "s1");
        assert_ne!(a, state_path(dir, "codex", "s2"));
        assert_ne!(a, state_path(dir, "claude-code", "s1"));
        assert!(!a.to_string_lossy().contains("s1"));
    }
}
