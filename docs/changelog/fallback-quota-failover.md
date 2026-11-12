<!--
Pending CHANGELOG entry for branch fix/llm-quota-failover. CHANGELOG.md had
uncommitted edits when this branch was cut, so the entry lives here until it
is pasted under `## [Unreleased]` in CHANGELOG.md; delete this file then.
The `### Fixed` and `### Added` blocks below go into the matching
subsections.
-->

### Fixed

- An `[[llm_fallbacks]]` chain now moves to the next provider when the current
  one reports that the account is out of credit, quota or billing, not only on
  a `429`, `5xx` or network failure. Anthropic's `400` "credit balance is too
  low" used to count as a malformed request, so with an exhausted Anthropic
  primary every consolidation failed and the configured OpenAI fallback never
  ran. The classification covers Anthropic, OpenAI (`insufficient_quota`,
  billing limits, deactivated accounts), OpenAI-compatible gateways such as
  OpenRouter (`402`), Gemini daily quotas, Codex `usage_limit_reached`,
  Copilot and OpenCode, and reads only the bounded, scrubbed error body. A
  plain invalid-key `401` still does not fall back. Exhausted quota is no
  longer retried in place by the consolidation and bootstrap loops; the
  SessionEnd queue keeps such a job on its backoff. See
  [`docs/llm-provider-fallback.md`](../llm-provider-fallback.md#credit-and-quota-exhaustion).

### Added

- `llm_quota_cooldown_secs` (default 900, env
  `AI_MEMORY_LLM_QUOTA_COOLDOWN_SECS`) sets how long a fallback chain skips a
  provider that is out of credit or quota. The next call after the window
  probes it again, so a topped-up primary takes back over without a restart.
  `ai-memory status` names an exhausted primary and the fallback answering in
  its place, and the health payload's candidates carry a `circuit_reason`.
