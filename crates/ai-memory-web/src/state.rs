//! Web router state — the handle a request handler receives.
//!
//! Holds the read-only store pool + the wiki handle. Cheap to clone
//! (everything inside is `Arc`-shaped already), so axum's
//! `State<Arc<WebState>>` extractor stays free of clone-heavy code.

use ai_memory_store::ReaderPool;
use ai_memory_wiki::Wiki;
use std::sync::Arc;

/// Shared state for every web route. Construct once via
/// [`crate::router`].
#[derive(Clone)]
pub struct WebState {
    /// Read-only SQLite pool — drives FTS5 search, page metadata,
    /// project list aggregates.
    pub reader: ReaderPool,
    /// Wiki handle — reads page bodies from disk.
    pub wiki: Wiki,
    /// Process-lifetime, content-free hook-ingestion counters. `None` keeps
    /// standalone embedders and tests backward-compatible.
    pub ingest_metrics: Option<Arc<ai_memory_core::IngestMetrics>>,
}

impl WebState {
    /// Build a new shared state.
    #[must_use]
    pub fn new(reader: ReaderPool, wiki: Wiki) -> Self {
        Self {
            reader,
            wiki,
            ingest_metrics: None,
        }
    }

    /// Attach the server's shared hook-health counters to a web surface.
    #[must_use]
    pub fn with_ingest_metrics(mut self, metrics: Arc<ai_memory_core::IngestMetrics>) -> Self {
        self.ingest_metrics = Some(metrics);
        self
    }
}
