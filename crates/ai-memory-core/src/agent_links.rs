//! Links between two projects (`docs/agent-bus/CROSS_PROJECT.md`,
//! `docs/design-agent-bus.md` section 8).
//!
//! A link lets mail between exactly two projects wake like mail inside one
//! project. It never exists by default: a request is filed, the destination
//! project confirms it, and it ends by itself. Every decision is a rule on
//! the server; nothing here calls a model or reads a message body.
//!
//! This module holds the vocabulary and the pure rules, so the store and the
//! routes agree on them: states, bounds, who may act, and how a validity
//! request becomes an expiry.

use crate::{MemoryError, MemoryResult};

/// Default validity of a link, counted from its confirmation: 2 hours.
pub const DEFAULT_LINK_SECS: u64 = 2 * 3_600;
/// Largest validity a request may ask for: 24 hours.
pub const MAX_LINK_SECS: u64 = 24 * 3_600;
/// Smallest validity: 5 minutes.
pub const MIN_LINK_SECS: u64 = 300;
/// How long an unconfirmed request waits before it expires: 24 hours.
pub const REQUEST_TTL_SECS: u64 = 24 * 3_600;
/// Longest reason, in characters.
pub const MAX_LINK_REASON_CHARS: usize = 300;
/// Live links (requested or active) one project may be a side of.
pub const MAX_LIVE_LINKS_PER_PROJECT: i64 = 8;
/// Requests an agent of one project may file per hour.
pub const MAX_AGENT_REQUESTS_PER_HOUR: i64 = 6;
/// How old, at most, the user prompt that confirms a request may be.
pub const CONFIRM_PROMPT_MAX_AGE_SECS: i64 = 30 * 60;

/// Where a link is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkState {
    /// Filed, waiting for the destination project. Never wakes anyone.
    Requested,
    /// Confirmed by the destination and not yet over: mail between the two
    /// projects wakes like mail inside one project.
    Active,
    /// Ended by a person, an agent of either project, or the removal of a
    /// project.
    Closed,
    /// Ended by its clock.
    Expired,
}

impl LinkState {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Active => "active",
            Self::Closed => "closed",
            Self::Expired => "expired",
        }
    }

    /// Parse the stored spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        [Self::Requested, Self::Active, Self::Closed, Self::Expired]
            .into_iter()
            .find(|state| state.as_str() == raw)
    }

    /// Whether the link still counts toward the one-per-pair rule.
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Requested | Self::Active)
    }
}

/// How a link was confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmVia {
    /// The root user pressed Confirm on the destination project's page.
    Web,
    /// The root user opened the link from the "Connect projects" form and
    /// confirmed for both projects in the same act.
    WebBoth,
    /// An agent of the destination project, after the user of that project
    /// answered in a prompt the server saw after the request was shown.
    Agent,
}

impl ConfirmVia {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::WebBoth => "web_both",
            Self::Agent => "agent",
        }
    }
}

/// Why a link ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// A person or an agent closed it, or declined the request.
    Closed,
    /// Its clock ran out.
    Expired,
    /// A project of the pair was removed.
    ProjectPurged,
}

impl CloseReason {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Expired => "expired",
            Self::ProjectPurged => "project_purged",
        }
    }

    /// The state a link takes when it ends for this reason.
    #[must_use]
    pub const fn state(self) -> LinkState {
        match self {
            Self::Expired => LinkState::Expired,
            Self::Closed | Self::ProjectPurged => LinkState::Closed,
        }
    }
}

/// Why the server refused an agent's confirmation. Each is a rule that ran
/// on the server; none depends on what a message said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmRefusal {
    /// The link is not waiting for a confirmation (unknown, active, ended or
    /// past its request time).
    NotRequested,
    /// The caller is not an agent of the destination project. This is the
    /// rule that keeps an agent from confirming its own request.
    NotDestination,
    /// The caller is the session that filed the request.
    OwnRequest,
    /// The caller has no recorded hook session in the destination project.
    NoSession,
    /// The request was never shown to the caller's session with
    /// `memory_link_list`.
    NotPresented,
    /// The caller's session has no user prompt after the request was shown
    /// (a wake text is not a prompt), or the newest one is too old.
    NoUserAnswer,
}

impl ConfirmRefusal {
    /// The code returned to the caller and written to the audit trail.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotRequested => "not_requested",
            Self::NotDestination => "not_destination",
            Self::OwnRequest => "own_request",
            Self::NoSession => "no_session",
            Self::NotPresented => "not_presented",
            Self::NoUserAnswer => "no_user_answer",
        }
    }

    /// A plain sentence for the caller.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::NotRequested => "this link is not waiting for a confirmation",
            Self::NotDestination => {
                "only an agent of the destination project can confirm; the requester never can"
            }
            Self::OwnRequest => "an agent cannot confirm a request its own session filed",
            Self::NoSession => {
                "the confirming agent needs a hook-recorded session in the destination project; \
                 confirm on the web page instead"
            }
            Self::NotPresented => {
                "show the request to your user first: call memory_link_list in this session, ask \
                 the user, then confirm after they answer"
            }
            Self::NoUserAnswer => {
                "no user prompt was recorded in this session after the request was shown (or it \
                 is older than 30 minutes); ask your user and wait for the answer"
            }
        }
    }
}

/// The validity a request asks for, in seconds, bounded to what a link may
/// last. `None` is the default.
///
/// # Errors
/// [`MemoryError::InvalidInput`] outside `5 minutes ..= 24 hours`.
pub fn link_validity(requested: Option<u64>) -> MemoryResult<u64> {
    let secs = requested.unwrap_or(DEFAULT_LINK_SECS);
    if (MIN_LINK_SECS..=MAX_LINK_SECS).contains(&secs) {
        Ok(secs)
    } else {
        Err(MemoryError::InvalidInput(format!(
            "a link lasts {MIN_LINK_SECS} seconds to {MAX_LINK_SECS} seconds (24 hours)"
        )))
    }
}

/// The reason for a link: trimmed, 1 to [`MAX_LINK_REASON_CHARS`] characters,
/// no control characters (a newline becomes a space).
///
/// # Errors
/// [`MemoryError::InvalidInput`] for an empty or oversized reason.
pub fn link_reason(raw: &str) -> MemoryResult<String> {
    let cleaned: String = raw
        .chars()
        .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
        .filter(|c| {
            !c.is_control()
                && !matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect();
    let cleaned = cleaned.trim().to_owned();
    if cleaned.is_empty() || cleaned.chars().count() > MAX_LINK_REASON_CHARS {
        return Err(MemoryError::InvalidInput(format!(
            "a link reason is 1 to {MAX_LINK_REASON_CHARS} characters"
        )));
    }
    Ok(cleaned)
}

/// Whether a link whose clock reads `expires_at` is over at `now`
/// (unix microseconds). The one place the expiry comparison is written, so
/// the wake gate and the sweep cannot disagree.
#[must_use]
pub const fn is_past(expires_at: i64, now: i64) -> bool {
    expires_at <= now
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validity_defaults_to_two_hours_and_never_passes_a_day() {
        assert_eq!(link_validity(None).unwrap(), 7_200);
        assert_eq!(link_validity(Some(86_400)).unwrap(), 86_400);
        assert!(link_validity(Some(86_401)).is_err());
        assert!(link_validity(Some(299)).is_err());
        assert!(link_validity(Some(0)).is_err());
    }

    #[test]
    fn a_reason_is_one_bounded_line_of_text() {
        assert_eq!(
            link_reason("  share the API\nshape ").unwrap(),
            "share the API shape"
        );
        assert!(link_reason("   ").is_err());
        assert!(link_reason(&"x".repeat(301)).is_err());
        assert_eq!(link_reason("a\u{202e}b").unwrap(), "ab");
    }

    #[test]
    fn states_round_trip_and_only_two_are_live() {
        for state in [
            LinkState::Requested,
            LinkState::Active,
            LinkState::Closed,
            LinkState::Expired,
        ] {
            assert_eq!(LinkState::parse(state.as_str()), Some(state));
        }
        assert!(LinkState::Requested.is_live() && LinkState::Active.is_live());
        assert!(!LinkState::Closed.is_live() && !LinkState::Expired.is_live());
    }

    #[test]
    fn the_expiry_instant_itself_is_already_over() {
        assert!(is_past(10, 10));
        assert!(is_past(10, 11));
        assert!(!is_past(10, 9));
    }
}
