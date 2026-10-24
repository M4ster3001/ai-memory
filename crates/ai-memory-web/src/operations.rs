//! View data shared by project, harness and session diagnostics.

use crate::templates::{
    SessionRow, TokenUsagePanel, TokenUsageRow, agent_label, duration_between, fmt_tokens,
    humanize, project_href, session_href, short_id,
};
use ai_memory_core::AgentKind;
use ai_memory_store::{AgentUsage, SessionSummary, TokenUsageReport};

pub(crate) struct AgentCard {
    pub kind: String,
    pub label: &'static str,
    pub href: String,
    pub compose_href: String,
    pub sessions: u64,
    pub open: u64,
    pub measured: u64,
    pub coverage: u64,
    pub calls: u64,
    pub tokens: String,
    pub input: String,
    pub output: String,
    pub cache: String,
}

pub(crate) fn agent_card(row: AgentUsage, workspace: &str, project: &str) -> AgentCard {
    let href = format!("{}/agents/{}", project_href(workspace, project), row.agent);
    AgentCard {
        label: agent_label(AgentKind::from_wire(&row.agent)),
        kind: row.agent.clone(),
        href,
        compose_href: format!(
            "inbox?workspace={}&project={}&agent={}#compose",
            crate::templates::encode_segment(workspace),
            crate::templates::encode_segment(project),
            row.agent
        ),
        sessions: row.sessions,
        open: row.open,
        measured: row.measured_sessions,
        coverage: row
            .measured_sessions
            .saturating_mul(100)
            .checked_div(row.sessions)
            .unwrap_or(0),
        calls: row.calls,
        tokens: if row.measured_sessions == 0 {
            "—".into()
        } else {
            fmt_tokens(row.input + row.output + row.cache_read + row.cache_write)
        },
        input: fmt_tokens(row.input),
        output: fmt_tokens(row.output),
        cache: fmt_tokens(row.cache_read + row.cache_write),
    }
}

pub(crate) fn session_row(s: SessionSummary, workspace: &str, project: &str) -> SessionRow {
    let id = s.session_id.to_string();
    SessionRow {
        agent_label: agent_label(AgentKind::from_wire(&s.agent_kind)),
        duration: duration_between(&s.started_at, s.ended_at.as_deref()),
        tokens: s
            .usage
            .as_ref()
            .map(|u| {
                format!(
                    "{} in / {} out",
                    fmt_tokens(u.input_tokens),
                    fmt_tokens(u.output_tokens)
                )
            })
            .unwrap_or_else(|| "—".into()),
        href: session_href(workspace, project, &id),
        agent_kind: s.agent_kind,
        short_id: short_id(&id),
        observation_count: s.observation_count,
        started_relative: humanize(&s.started_at),
        open: s.ended_at.is_none(),
    }
}

pub(crate) fn usage_panel(
    report: TokenUsageReport,
    workspace: &str,
    project: &str,
) -> TokenUsagePanel {
    let input_total = report.input + report.cache_read + report.cache_write;
    let mean = (input_total + report.output)
        .checked_div(report.call_count)
        .unwrap_or(0);
    TokenUsagePanel {
        count: report.call_count,
        input: fmt_tokens(report.input),
        output: fmt_tokens(report.output),
        cache_read: fmt_tokens(report.cache_read),
        cache_write: fmt_tokens(report.cache_write),
        hit_rate: if input_total > 0 {
            format!(
                "{:.0}%",
                report.cache_read as f64 / input_total as f64 * 100.0
            )
        } else {
            "—".into()
        },
        rows: report
            .calls
            .into_iter()
            .map(|c| {
                let context = c.input + c.cache_read + c.cache_write;
                let total = context + c.output;
                let signal = if report.call_count >= 5 && total > mean.saturating_mul(3) {
                    "Above 3× average"
                } else if context >= 100_000 {
                    "Large context"
                } else if c.output >= 20_000 {
                    "Long output"
                } else {
                    "—"
                };
                TokenUsageRow {
                    total_value: total,
                    observed_at: c.observed_at.clone(),
                    href: session_href(workspace, project, &c.session_id),
                    id: c.call_id,
                    model: c.model,
                    agent: agent_label(AgentKind::from_wire(&c.agent)).into(),
                    input: fmt_tokens(c.input),
                    output: fmt_tokens(c.output),
                    cache: format!(
                        "{} read / {} write",
                        fmt_tokens(c.cache_read),
                        fmt_tokens(c.cache_write)
                    ),
                    total: fmt_tokens(total),
                    when: humanize(&c.observed_at),
                    signal: signal.into(),
                }
            })
            .collect(),
    }
}
