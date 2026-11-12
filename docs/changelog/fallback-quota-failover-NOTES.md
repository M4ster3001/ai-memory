<!--
Technical hand-off notes for branch fix/llm-quota-failover (base 745b0d60,
release/3.3). Not a released changelog section; delete after the work is
re-applied on upstream main.
-->

# Quota/credit failover: hand-off notes

## What changed

- `crates/ai-memory-llm/src/quota.rs` (new): `is_quota_exhaustion(status, body)`,
  the single place that reads the bounded (1 KiB, scrubbed) error body. `402`
  always counts; any other `4xx`/`5xx` counts only with an explicit marker.
- `error.rs`: `FailoverClass { Transient, QuotaExhausted, Permanent }`,
  `LlmError::failover_class()`, `is_quota_exhausted()`, `should_fail_over()`.
  `is_transient()` is now `failover_class() == Transient`, so a `429
  insufficient_quota` is no longer retried in place. `class()` (no longer
  `const`) returns `quota-exhausted` for such provider errors.
- `fallback.rs`: advances on `should_fail_over()`; quota failures open the
  circuit for `quota_cooldown` (`DEFAULT_QUOTA_COOLDOWN` = 900 s,
  `with_quota_cooldown`), stretched by a longer retry hint up to
  max(1 h, cooldown). `circuit_reason` per candidate; skipped candidates appear
  in the aggregate as `p/m: skipped (circuit open: quota-exhausted)`. Separate
  warn line "out of credit or quota" (redacted fields only). The probe is lazy:
  the first real call after the window tries the primary again.
- `health.rs`: `CandidateHealth.circuit_reason: Option<String>`
  (`serde(default, skip_serializing_if = none)`).
- `ai-memory-cli`: `llm_quota_cooldown_secs` (default 900, range 30–86400, env
  `AI_MEMORY_LLM_QUOTA_COOLDOWN_SECS`) wired into `llm_provider_chain`;
  `status` headline "primary X exhausted (credit/quota); using fallback Y";
  SessionEnd queue keeps quota errors on its backoff (not terminal).
- Docs: `llm-provider-fallback.md` (new "Credit and quota exhaustion" section),
  `llm-providers.md`, `config-properties.md`; pending CHANGELOG text in
  `docs/changelog/fallback-quota-failover.md` (CHANGELOG.md had uncommitted
  user edits and was not touched).

## Classification by provider (case-insensitive body markers)

| Provider | QuotaExhausted signal |
| --- | --- |
| anthropic / anthropic-oauth | `credit balance is too low` (400), `billing_error` (402), `reached your specified api usage limits`, `organization has been disabled` (403) |
| openai | `insufficient_quota` (429), `billing_hard_limit_reached`, `billing_not_active`, `account_deactivated` / `account has been deactivated|disabled` |
| openai-compat (OpenRouter, DeepSeek…) | any 402; `insufficient credits|_credits`, `insufficient balance|_balance`, `out of credits`, `requires more credits`, `key limit exceeded` |
| gemini | `RESOURCE_EXHAUSTED` + (`PerDay`/`per_day`/`per day` or `limit: 0`); `spending cap`; `credits are depleted` |
| openai-oauth / codex | `usage_limit_reached`, `usage limit has been reached` (also via the SSE 502 path) |
| copilot | `quota_exceeded`, `monthly quota` |
| opencode | any 402; balance/credit markers above |

Not markers on purpose: "exceeded your current quota" (Gemini uses it for
per-minute limits), generic rate-limit text. Plain 401 (invalid/revoked key)
never fails over; 401/403 fail over only with an explicit billing/quota/
disabled-account marker.

## Tests

- `quota.rs`: synthetic fixtures per provider (positive and negative: rate
  limits, overloaded, bad request, bad key, Gemini per-minute).
- `error.rs`: class/transient/fail-over split.
- `fallback.rs`: exhausted→fallback and primary skipped for the window;
  every status (400/429/402/403) advances; probe after window (still
  exhausted → reopen; topped up → takes back over, circuit closed); retry-hint
  stretch/clamp; permanent error after a quota failover still stops; generic
  401 stops; all exhausted → redacted aggregate incl. "skipped" entries; log
  line has no body.
- `tests/suite/fallback_provider.rs` (wiremock, real providers via
  `build_provider`): Anthropic 400 credit → OpenAI answers, no hammering,
  recovery after 1 s window; OpenAI 429 insufficient_quota + OpenRouter 402 →
  third candidate, each hit once; Anthropic 401 does not fail over; all three
  out of credit → aggregate without bodies or keys.
- `ai-memory-cli`: config default/range/round-trip, documented example loads,
  configured cooldown reaches the chain (raw tokio 402 server), status
  headline, queue classification.

Results on this base: `cargo test -p ai-memory-llm` 448 passed / 0 failed.
`cargo test -p ai-memory-cli` 2192 passed / 13 failed; all 13 are Node/TypeScript
runtime fixtures (`ERR_NO_TYPESCRIPT`: local Node lacks TS support:
install_hooks identity_parity/opencode2/omp, openclaw_plugin, render_shared
capture-owner gate), unrelated to this change. Build target had to live off
the vfat drive (noexec): CARGO_TARGET_DIR on the ext4 partition.

## Pending

- `cargo clippy --workspace --all-targets -- -D warnings` not run yet.
- Docker image `ai-memory:quota-fallback` not built (plan changed); no
  container touched.
- Re-apply on upstream main: upstream already reworked `health.rs` and the
  fallback cooldown ("preserve the longer fallback cooldown across
  overlapping failures") and mentions quota in fallback.rs/error.rs; check
  overlap before porting `cooldown_for`, `circuit_reason` and the classifier.
- Humanizer pass on doc prose (AGENTS.md rule) not done.
