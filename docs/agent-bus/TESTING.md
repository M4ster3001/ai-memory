# Testing the agent bus

The agent bus has a regression suite that fails when a change breaks mail
inside one project, mail between projects (with and without a link), or the
contracts agents and plugins are written against. It runs in CI on every pull
request as its own step, `agent-bus regression`, and in the normal test run.

Every test in it has `agent_bus_regression` in its name, so one filter selects
the suite in every crate.

## What the suite protects

| Area | Rule it pins |
| --- | --- |
| Same project | A message to another agent of the project is delivered and wakes it once, with the fixed wake text and no message content in the argv. A reply closes the cycle with a `delivered` receipt. |
| Loop guard | Twenty direction changes, three copies of one body in 30 minutes, or a chain deeper than 40 turns pause that pair only. A human prompt or the dashboard's Resume lifts it. Nine rounds of ordinary ping-pong never trip it. |
| Coalescing | Mail that arrives while earlier mail is unread joins it: one wake, not one per message. A read reopens the gate. |
| Delivery states | `queued`, `awaiting_agent`, `wake_attempted`, `delivered`, `wake_failed` (`session_not_found`, `loop_suspected`) and the rest tell the sender what happened and never lose the mail. |
| Mid-turn notice | A busy agent hears of new mail between tool calls: a count and ids, at most one check every 5 seconds and one notice every 15, once per message, only for mail from another agent of its project, a linked project or a receipt. Never its own mail, never an unlinked project, never text. |
| Idempotency | A pop is idempotent. An expired message is never delivered. A restart of the server or the store duplicates and loses no mail, wake report, receipt or pause. |
| Cross-project, no link | Mail is delivered to the inbox and wakes nobody, and no mid-turn notice names it. The chat page shows its sender and state, never its text. |
| Links | A request is inert. Only the destination confirms: the wrong web page gets 409, the requester never confirms its own request, and a destination agent must have been shown the request and then heard a user prompt. An active link wakes both ways and only for its pair. The clock ends it with no sweep. Closing, purging a project, 8 live links per project and 6 agent requests an hour all hold. One live link per pair, whichever side asked. Every step is in the audit trail. |
| Text safety | Mail text with HTML, bidirectional marks, control characters or more than 2000 characters is escaped, dropped or cut on the chat page. A hostile body never becomes argv, a wake text or a notice. |
| Contracts | The `GET /handoff?inbox_turn=1` JSON, the wake-candidate labels, the delivery state and reason vocabulary, the answers and parameters of the `memory_message_*` and `memory_link_*` tools, and the docs that name them. |
| Upgrade | A store written before the bus (V104) opens at V109 with every message, receipt and delivery event intact. |

## Layers and files

| Layer | Where | What it drives |
| --- | --- | --- |
| Core rules | `crates/ai-memory-core/src/agent_bus_regression.rs` | Wake and notice text, adapter argv, the wake ledger (coalescing, backoff, a restart), vocabularies, link bounds, docs against code |
| Store | `crates/ai-memory-store/tests/suite/agent_bus_regression_{same_project,cross_project,migration}.rs` | A real store in a temp directory: delivery, receipts, loop guard, links, audit, restart, V104 to V109 |
| Hook router | `crates/ai-memory-hooks/tests/suite/agent_bus_regression_notice.rs` | `GET /handoff?inbox_turn=1` and `POST /inbox/wake-candidates` through the real router |
| Notice limits | `crates/ai-memory-cli/src/commands/turn_notice.rs` (module `agent_bus_regression`) | The 5 second and 15 second limits and the once-per-id state, with an explicit clock |
| MCP | `crates/ai-memory-mcp/tests/suite/agent_bus_regression_contract.rs` | `tools/list` and `tools/call` over the real transport |
| Web | `crates/ai-memory-web/tests/suite/agent_bus_regression_chat.rs` | The root-only `/web/chat` page and its routes |
| End to end | `crates/ai-memory-cli/tests/suite/agent_bus_regression_{same_project,cross_project}.rs`, fixture in `agent_bus_regression_support.rs` | The shipped binary: a real `serve`, the real wake supervisor, the native tool hook, the CLI, with a fake `codex` and a fake `claude` |

The end-to-end tests live in a `slow` module, because each starts a server.
Unix only.

## What is not real in these tests

- No model runs. `codex` and `claude` are shell scripts that append their argv,
  working directory and environment to a file and exit. The tests assert on that
  file.
- No network, no token, no operator state. Each test makes its own data
  directory, home directory, tools directory and loopback port, and starts the
  server with the operator's `AI_MEMORY_*` variables removed. The production
  server, `~/.claude`, `~/.codex`, the installed hooks and the systemd unit are
  never touched.
- Time is a number the test passes in, or a stored value the test edits. Nothing
  waits for a link to expire or an interval to pass. The few `sleep`s are
  3 millisecond ticks that keep two writes from sharing a microsecond.

## Running it

Everything the suite needs is in the workspace. From the repository root:

```bash
# The whole suite, every crate, the slow end-to-end tier included (what CI runs).
cargo nextest run --workspace --all-targets --profile ci -E 'test(agent_bus_regression)'

# The same without the end-to-end tier: seconds, no server.
cargo nextest run -E 'test(agent_bus_regression)'

# One layer.
cargo nextest run -p ai-memory-store -E 'test(agent_bus_regression)'

# Without nextest (the end-to-end tests still need the built binary, which cargo builds).
cargo test -p ai-memory-core -p ai-memory-store -p ai-memory-hooks -p ai-memory-mcp \
  -p ai-memory-web -p ai-memory-cli agent_bus_regression
```

The everyday `cargo t` profile skips modules named `slow`, so it skips the
end-to-end layer. `--profile ci` and `cargo tf` run it.

The CI step lists the tests first and fails if fewer than 75 match, so a rename
that makes the filter match nothing cannot leave the step green. Raise the
number in `.github/workflows/ci.yml` when you add cases.

## Adding a case

1. Put the check where the rule lives. A rule that is a pure function or a
   store query belongs in the core or store layer, where a test takes a clock as
   an argument and finishes in milliseconds. Use the end-to-end layer only for
   what the layers below cannot see: the supervisor's argv, the hook binary, a
   restart of the server.
2. Name it `agent_bus_regression_<the rule as a sentence>`. The name is what a
   person reads in a failing CI run, so say what must stay true.
3. Do not sleep for time to pass. Pass a later `now` (`wake_candidates_at`,
   `sweep_message_deadlines`, the wake ledger), or move a stored time through a
   second connection (`raw` in the store support, `move_link_clock` in the
   end-to-end fixture). In the end-to-end layer use a new tool-hook session id
   instead of waiting out the 5 second check interval.
4. Assert on what an operator or an agent sees (a delivery state, an argv, a
   page). Where the rule is "never", also show the allowed case happening, so
   the test cannot pass on an empty read.
5. Declare a new file in the crate's `tests/suite/mod.rs` (`main.rs` for the
   CLI). The layout test rejects an undeclared file.
6. Raise the floor in the CI step.

When a tool, an answer, a delivery state or a notice changes on purpose, the
contract test fails. Update it in the same commit as the docs it names:
[FOR_AGENTS.md](FOR_AGENTS.md), [`docs/agent-messaging.md`](../agent-messaging.md),
the routing skill under `crates/ai-memory-core/src/routing_skills/` and the
plugin skills.

## Checking that the suite bites

Before trusting a new test, break the rule it guards, watch it fail, and restore
the code. Do this by hand and restore with `git checkout -- <file>`; never leave
a mutation committed. Five critical rules were checked when the suite
was written:

| Rule | Mutation | Tests that failed |
| --- | --- | --- |
| The link clock | `l.expires_at > ?6` in `wake_candidates_at` replaced by a condition that is always true | `the_link_clock_ends_waking_with_no_sweep`, `a_link_survives_a_restart_with_its_clock` |
| Only the destination confirms | The destination check removed from the web confirmation, then from the agent confirmation (two runs) | `only_the_destination_confirms` (both runs), `every_step_of_a_link_is_in_the_trail` (web), and in the web crate `only_the_destinations_page_confirms_a_link` |
| No link, no wake | Every cross-project message labelled `linked` | In the store, `without_a_link_mail_is_delivered_but_does_not_wake` and seven more; in the hook router, `mail_from_another_project_without_a_link_makes_no_notice` and two more |
| Wake text with no content | Extra data appended to the template of `wake_text` | `wake_text_is_a_fixed_template_with_no_message_content` |
| Loop guard | The ping-pong signal made never trip, then the repeated-content signal | `twenty_direction_changes_pause_the_pair_and_resume_lifts_it`; `three_equal_bodies_in_thirty_minutes_pause_the_pair_until_a_human_prompt`, `copies_older_than_thirty_minutes_do_not_count`, `a_restart_repeats_no_mail_no_wake_report_and_no_receipt` |

Each mutation ran the store, hook router, web or core tests only; the
end-to-end layer was not re-run for them.

## What is not covered

- Real Claude Code, Codex or Gemini CLI. The wake adapters are tested up to the
  process boundary: the argv, working directory, environment and the
  classification of an exit. Whether `codex queue`, `claude -p --resume` or
  `gemini --resume` behave as documented is checked by hand against the real
  tools, and the Gemini adapter has never run against a real Gemini CLI.
- The harness reading the notice. The tests prove the hook prints the right
  `additionalContext`. They cannot prove that a given Codex or Claude Code
  release puts it in the model's context.
- Installed hooks and the user service. `agent-bus install` is covered by
  `message_supervise_e2e` against a throwaway home, not by this suite.
- The terminal approval prompt for configured supervisor entries.
- Long real-time behaviour: the 12 hour `Stop` hook wait, the 2 hour link
  default and the 10 minute coalescing hold as wall-clock time. The limits are
  tested as numbers against an explicit clock.
- Windows and macOS end-to-end runs. The end-to-end layer is Unix only; CI runs
  it on Linux, and on macOS when the `full-ci` label is set.
- A multi-user server with database users. The chat and link routes refuse
  anyone but the root user, and that is tested; per-user project grants for the
  bus are covered by the existing authorization suites.
