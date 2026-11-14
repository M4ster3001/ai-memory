//! Writer-to-reader wake-up for agent inboxes.
//!
//! The writer actor bumps a generation after it commits a change that makes
//! a message claimable (a send, or an edited message returning to the
//! queue). A long-poll waiter (`GET /inbox/watch`) holds a receiver for the
//! inbox it watches and wakes on the bump instead of re-reading on a timer.
//!
//! The signal is a hint sent after commit, never data: it carries no message
//! text, only a number, and the waiter's read that follows is authoritative.
//! A coalesced or missed bump (a project rename that moves pending mail, for
//! one) delays a wake until the waiter's own timeout and loses nothing.
//!
//! Generations come from one counter seeded with the wall clock, so a value is
//! never handed out twice in a process and practically never across restarts.
//! A client that presents a generation the server no longer tracks therefore
//! gets an immediate answer instead of a missed wake.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ai_memory_core::{AgentKind, ProjectId, WorkspaceId};
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

/// Most long-poll waiters the server holds at once. A waiter past this is
/// refused at once, so watchers cannot pin unbounded tasks.
pub const MAX_INBOX_WATCHERS: usize = 64;

/// Upper bound on tracked `(inbox, recipient)` keys. Each live waiter holds at
/// most two, so the cap is never reached by waiters alone; idle keys are
/// dropped when it is.
const MAX_TRACKED_KEYS: usize = 4_096;

/// One inbox and the recipient a message is addressed to (`None` is mail any
/// harness of the project may claim).
type SignalKey = (WorkspaceId, ProjectId, Option<AgentKind>);

/// Per-inbox commit signals, owned by the writer handle.
#[derive(Clone)]
pub struct InboxSignals {
    inner: Arc<SignalsInner>,
}

struct SignalsInner {
    next: AtomicU64,
    keys: Mutex<HashMap<SignalKey, watch::Sender<u64>>>,
    waiters: Arc<Semaphore>,
}

impl InboxSignals {
    pub(crate) fn new(max_waiters: usize) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |elapsed| {
                u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX / 2)
            });
        Self {
            inner: Arc::new(SignalsInner {
                next: AtomicU64::new(seed),
                keys: Mutex::new(HashMap::new()),
                waiters: Arc::new(Semaphore::new(max_waiters)),
            }),
        }
    }

    fn fresh_generation(&self) -> u64 {
        self.inner.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// A message addressed to `recipient` (or shared mail, `None`) became
    /// claimable in `(workspace, project)`. Call only after the commit.
    pub(crate) fn notify(
        &self,
        workspace: WorkspaceId,
        project: ProjectId,
        recipient: Option<AgentKind>,
    ) {
        let keys = self.inner.keys.lock();
        if let Some(sender) = keys.get(&(workspace, project, recipient)) {
            sender.send_replace(self.fresh_generation());
        }
    }

    /// A message of unknown recipient became claimable in the inbox: wake
    /// every waiter of that inbox. Used by the requeue after an edit, whose
    /// guarded `UPDATE` does not return the row's `to_agent`.
    pub(crate) fn notify_inbox(&self, workspace: WorkspaceId, project: ProjectId) {
        let keys = self.inner.keys.lock();
        for ((ws, proj, _), sender) in keys.iter() {
            if *ws == workspace && *proj == project {
                sender.send_replace(self.fresh_generation());
            }
        }
    }

    /// Start watching the mail `agent` can be woken for in an inbox: mail
    /// addressed to it, plus shared mail when `include_shared` is set.
    ///
    /// Returns `None` when [`MAX_INBOX_WATCHERS`] waiters are already held;
    /// the caller answers "too many requests" instead of queueing.
    #[must_use]
    pub fn try_watch(
        &self,
        workspace: WorkspaceId,
        project: ProjectId,
        agent: AgentKind,
        include_shared: bool,
    ) -> Option<InboxWatch> {
        let permit = self.inner.waiters.clone().try_acquire_owned().ok()?;
        let mut keys = self.inner.keys.lock();
        if keys.len() >= MAX_TRACKED_KEYS {
            keys.retain(|_, sender| sender.receiver_count() > 0);
        }
        let mut subscribe = |recipient: Option<AgentKind>| {
            keys.entry((workspace, project, recipient))
                .or_insert_with(|| watch::Sender::new(self.fresh_generation()))
                .subscribe()
        };
        let addressed = subscribe(Some(agent));
        let shared = include_shared.then(|| subscribe(None));
        Some(InboxWatch {
            addressed,
            shared,
            _permit: permit,
        })
    }
}

/// A held long-poll slot and the receivers for one watcher.
pub struct InboxWatch {
    addressed: watch::Receiver<u64>,
    shared: Option<watch::Receiver<u64>>,
    _permit: OwnedSemaphorePermit,
}

impl InboxWatch {
    /// The current generation, marked as seen. A client that presents this
    /// value on its next request has read everything committed before it.
    pub fn generation(&mut self) -> u64 {
        let addressed = *self.addressed.borrow_and_update();
        self.shared.as_mut().map_or(addressed, |shared| {
            addressed.max(*shared.borrow_and_update())
        })
    }

    /// Wait for the next bump after the last [`Self::generation`]. Returns
    /// `false` only when the signals were dropped (server shutdown).
    pub async fn changed(&mut self) -> bool {
        match self.shared.as_mut() {
            Some(shared) => tokio::select! {
                result = self.addressed.changed() => result.is_ok(),
                result = shared.changed() => result.is_ok(),
            },
            None => self.addressed.changed().await.is_ok(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn inbox() -> (WorkspaceId, ProjectId) {
        (WorkspaceId::new(), ProjectId::new())
    }

    #[tokio::test]
    async fn a_bump_wakes_only_the_addressed_recipient() {
        let signals = InboxSignals::new(4);
        let (ws, proj) = inbox();
        let mut codex = signals
            .try_watch(ws, proj, AgentKind::Codex, false)
            .unwrap();
        let mut claude = signals
            .try_watch(ws, proj, AgentKind::ClaudeCode, false)
            .unwrap();
        let before = codex.generation();
        claude.generation();

        signals.notify(ws, proj, Some(AgentKind::Codex));

        tokio::time::timeout(Duration::from_secs(1), codex.changed())
            .await
            .expect("the addressed watcher wakes");
        assert!(codex.generation() > before);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), claude.changed())
                .await
                .is_err(),
            "another harness's mail must not wake this watcher"
        );
    }

    #[tokio::test]
    async fn shared_mail_wakes_only_watchers_that_include_it() {
        let signals = InboxSignals::new(4);
        let (ws, proj) = inbox();
        let mut with_shared = signals.try_watch(ws, proj, AgentKind::Codex, true).unwrap();
        let mut without = signals
            .try_watch(ws, proj, AgentKind::Codex, false)
            .unwrap();
        with_shared.generation();
        without.generation();

        signals.notify(ws, proj, None);

        tokio::time::timeout(Duration::from_secs(1), with_shared.changed())
            .await
            .expect("shared mail wakes a watcher that includes it");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), without.changed())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn another_inbox_never_wakes_the_watcher() {
        let signals = InboxSignals::new(4);
        let (ws, proj) = inbox();
        let mut watcher = signals.try_watch(ws, proj, AgentKind::Codex, true).unwrap();
        watcher.generation();
        let other = ProjectId::new();
        signals.notify(ws, other, Some(AgentKind::Codex));
        signals.notify_inbox(ws, other);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), watcher.changed())
                .await
                .is_err()
        );
        // Control: the requeue form does reach this inbox's watchers.
        signals.notify_inbox(ws, proj);
        tokio::time::timeout(Duration::from_secs(1), watcher.changed())
            .await
            .expect("a requeue in the watched inbox wakes it");
    }

    #[test]
    fn waiters_past_the_cap_are_refused_and_a_released_slot_is_reused() {
        let signals = InboxSignals::new(2);
        let (ws, proj) = inbox();
        let first = signals.try_watch(ws, proj, AgentKind::Codex, false);
        let second = signals.try_watch(ws, proj, AgentKind::Codex, false);
        assert!(first.is_some() && second.is_some());
        assert!(
            signals
                .try_watch(ws, proj, AgentKind::Codex, false)
                .is_none(),
            "a waiter past the cap must be refused"
        );
        drop(first);
        assert!(
            signals
                .try_watch(ws, proj, AgentKind::Codex, false)
                .is_some()
        );
    }

    #[test]
    fn generations_are_never_reused_after_a_key_is_dropped() {
        let signals = InboxSignals::new(1);
        let (ws, proj) = inbox();
        let mut first = signals
            .try_watch(ws, proj, AgentKind::Codex, false)
            .unwrap();
        let seen = first.generation();
        drop(first);
        signals.inner.keys.lock().clear();
        let mut again = signals
            .try_watch(ws, proj, AgentKind::Codex, false)
            .unwrap();
        assert_ne!(
            again.generation(),
            seen,
            "a re-created key must not repeat a generation a client may hold"
        );
    }
}
