//! Credit, quota and billing exhaustion detection for provider failures.
//!
//! The marker table lives in [`ai_memory_core::quota`] so the messaging
//! supervisor can classify a harness's output with the same rules. This
//! module keeps the provider-facing entry point: only the bounded, scrubbed
//! body that [`crate::LlmError::Provider`] carries (at most 1 KiB) is read,
//! and the result is a boolean.

/// Whether a failed provider response reports exhausted credit, quota or
/// billing for the account, rather than a malformed request, a bad key or a
/// short rate limit. See [`ai_memory_core::quota::quota_signal`].
pub(crate) fn is_quota_exhaustion(status: u16, body: &str) -> bool {
    ai_memory_core::quota::quota_signal(Some(status), body).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic fixtures that follow each provider's documented error shape.
    const ANTHROPIC_CREDIT: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."},"request_id":"req_011CTest"}"#;
    const ANTHROPIC_BILLING: &str = r#"{"type":"error","error":{"type":"billing_error","message":"There is an issue with your billing or payment information."}}"#;
    const ANTHROPIC_SPEND_LIMIT: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"You have reached your specified API usage limits. You will regain access on 2026-12-01 at 00:00 UTC."}}"#;
    const ANTHROPIC_DISABLED_ORG: &str = r#"{"type":"error","error":{"type":"permission_error","message":"This organization has been disabled."}}"#;
    const ANTHROPIC_RATE_LIMIT: &str = r#"{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed the rate limit for your organization of 50 requests per minute."}}"#;
    const ANTHROPIC_OVERLOADED: &str =
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
    const ANTHROPIC_BAD_REQUEST: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: 200000 > 64000, which is the maximum allowed number of output tokens for claude-sonnet"}}"#;
    const ANTHROPIC_BAD_KEY: &str =
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;

    const OPENAI_INSUFFICIENT_QUOTA: &str = r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details. For more information on this error, read the docs: https://platform.openai.com/docs/guides/error-codes/api-errors.","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}"#;
    const OPENAI_HARD_LIMIT: &str = r#"{"error":{"message":"Billing hard limit has been reached","type":"invalid_request_error","param":null,"code":"billing_hard_limit_reached"}}"#;
    const OPENAI_DEACTIVATED: &str = r#"{"error":{"message":"Your OpenAI account has been deactivated, please check your email for more information.","type":"invalid_request_error","param":null,"code":"account_deactivated"}}"#;
    const OPENAI_RATE_LIMIT: &str = r#"{"error":{"message":"Rate limit reached for gpt-5 in organization org-test on tokens per min (TPM): Limit 30000, Used 29000, Requested 2000. Please try again in 2s.","type":"tokens","param":null,"code":"rate_limit_exceeded"}}"#;
    const OPENAI_BAD_KEY: &str = r#"{"error":{"message":"Incorrect API key provided: sk-proj-****. You can find your API key at https://platform.openai.com/account/api-keys.","type":"invalid_request_error","param":null,"code":"invalid_api_key"}}"#;
    const OPENAI_BAD_REQUEST: &str = r#"{"error":{"message":"Invalid schema for response_format 'out': additionalProperties must be false","type":"invalid_request_error","param":"response_format","code":null}}"#;

    const OPENROUTER_CREDITS: &str = r#"{"error":{"message":"Insufficient credits. Add more using https://openrouter.ai/settings/credits","code":402}}"#;
    const OPENROUTER_AFFORD: &str = r#"{"error":{"message":"This request requires more credits, or fewer max_tokens. You requested up to 64000 tokens, but can only afford 1200.","code":402}}"#;
    const OPENROUTER_KEY_LIMIT: &str = r#"{"error":{"message":"Key limit exceeded","code":403}}"#;
    const DEEPSEEK_BALANCE: &str = r#"{"error":{"message":"Insufficient Balance","type":"unknown_error","param":null,"code":"invalid_request_error"}}"#;
    const OPENROUTER_UPSTREAM_RATE: &str = r#"{"error":{"message":"Provider returned error","code":429,"metadata":{"raw":"model is temporarily rate-limited upstream"}}}"#;

    const GEMINI_DAILY: &str = r#"{"error":{"code":429,"message":"You exceeded your current quota, please check your plan and billing details.","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"quotaMetric":"generativelanguage.googleapis.com/generate_content_free_tier_requests","quotaId":"GenerateRequestsPerDayPerProjectPerModel-FreeTier"}]}]}}"#;
    const GEMINI_ZERO: &str = r#"{"error":{"code":429,"message":"You exceeded your current quota.\n* Quota exceeded for metric: generativelanguage.googleapis.com/generate_content_free_tier_input_token_count, limit: 0, model: gemini-3-pro","status":"RESOURCE_EXHAUSTED"}}"#;
    const GEMINI_PER_MINUTE: &str = r#"{"error":{"code":429,"message":"You exceeded your current quota, please check your plan and billing details.","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"quotaMetric":"generativelanguage.googleapis.com/generate_content_free_tier_requests","quotaId":"GenerateRequestsPerMinutePerProjectPerModel-FreeTier"}]},{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"32s"}]}}"#;
    const GEMINI_SPEND_CAP: &str = r#"{"error":{"code":429,"message":"Your project has reached its monthly spending cap.","status":"RESOURCE_EXHAUSTED"}}"#;
    const GEMINI_BAD_KEY: &str = r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT"}}"#;

    const CODEX_USAGE_LIMIT: &str = r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"plus","resets_in_seconds":7200}}"#;
    const COPILOT_QUOTA: &str = r#"{"error":{"message":"You have exceeded your monthly quota of premium requests.","code":"quota_exceeded"}}"#;
    const OPENCODE_BALANCE: &str = r#"{"type":"error","error":{"type":"CreditsError","message":"Insufficient balance. Add credits to continue."}}"#;

    #[test]
    fn credit_and_quota_bodies_from_every_provider_are_detected() {
        let cases: &[(&str, u16, &str)] = &[
            ("anthropic credit", 400, ANTHROPIC_CREDIT),
            ("anthropic billing", 402, ANTHROPIC_BILLING),
            ("anthropic billing as 400", 400, ANTHROPIC_BILLING),
            ("anthropic spend limit", 400, ANTHROPIC_SPEND_LIMIT),
            ("anthropic disabled org", 403, ANTHROPIC_DISABLED_ORG),
            ("openai insufficient_quota", 429, OPENAI_INSUFFICIENT_QUOTA),
            ("openai hard limit", 400, OPENAI_HARD_LIMIT),
            ("openai deactivated", 401, OPENAI_DEACTIVATED),
            ("openrouter credits", 402, OPENROUTER_CREDITS),
            ("openrouter afford", 402, OPENROUTER_AFFORD),
            ("openrouter key limit", 403, OPENROUTER_KEY_LIMIT),
            ("deepseek balance", 402, DEEPSEEK_BALANCE),
            ("deepseek balance as 400", 400, DEEPSEEK_BALANCE),
            ("gemini daily", 429, GEMINI_DAILY),
            ("gemini zero tier", 429, GEMINI_ZERO),
            ("gemini spend cap", 429, GEMINI_SPEND_CAP),
            ("codex usage limit", 429, CODEX_USAGE_LIMIT),
            // The openai-oauth SSE path reports a terminal stream error as 502.
            ("codex usage limit via sse", 502, CODEX_USAGE_LIMIT),
            ("copilot quota", 429, COPILOT_QUOTA),
            ("copilot quota as 402", 402, COPILOT_QUOTA),
            ("opencode balance", 402, OPENCODE_BALANCE),
            ("opencode balance as 429", 429, OPENCODE_BALANCE),
            ("bare 402", 402, ""),
        ];
        for (label, status, body) in cases {
            assert!(is_quota_exhaustion(*status, body), "{label} ({status})");
        }
    }

    #[test]
    fn rate_limits_bad_requests_and_bad_keys_are_not_quota() {
        let cases: &[(&str, u16, &str)] = &[
            ("anthropic rate limit", 429, ANTHROPIC_RATE_LIMIT),
            ("anthropic overloaded", 529, ANTHROPIC_OVERLOADED),
            ("anthropic bad request", 400, ANTHROPIC_BAD_REQUEST),
            ("anthropic bad key", 401, ANTHROPIC_BAD_KEY),
            ("openai rate limit", 429, OPENAI_RATE_LIMIT),
            ("openai bad key", 401, OPENAI_BAD_KEY),
            ("openai bad schema", 400, OPENAI_BAD_REQUEST),
            (
                "openrouter upstream rate limit",
                429,
                OPENROUTER_UPSTREAM_RATE,
            ),
            ("gemini per-minute", 429, GEMINI_PER_MINUTE),
            ("gemini bad key", 400, GEMINI_BAD_KEY),
            ("empty 401", 401, ""),
            ("empty 429", 429, ""),
            ("empty 503", 503, ""),
        ];
        for (label, status, body) in cases {
            assert!(!is_quota_exhaustion(*status, body), "{label} ({status})");
        }
    }

    #[test]
    fn a_success_or_informational_status_is_never_quota() {
        for status in [100, 200, 204, 302] {
            assert!(!is_quota_exhaustion(status, ANTHROPIC_CREDIT), "{status}");
        }
    }

    #[test]
    fn markers_match_regardless_of_case() {
        assert!(is_quota_exhaustion(
            400,
            "YOUR CREDIT BALANCE IS TOO LOW TO ACCESS THE ANTHROPIC API"
        ));
        assert!(is_quota_exhaustion(
            429,
            "{\"code\":\"INSUFFICIENT_QUOTA\"}"
        ));
    }
}
