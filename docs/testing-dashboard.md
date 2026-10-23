# Dashboard regression tests

The dashboard is built and distributed with Cargo alone. Browser testing uses
Node 22+ and Chromium separately; it adds no JavaScript dependency to the Rust
server, its assets, Docker image or native packages.

## Run the checks

Run the repository's normal gates from `AGENTS.md`, including the full Rust
suite. The browser suite needs a built executable and an installed Chromium:

```bash
cargo build --locked --bin ai-memory
# Pinned test browser; installation follows Playwright's browser installer.
npx --yes playwright@1.63.0 install --with-deps --only-shell chromium
node tests/e2e/web_dashboard_browser.mjs
node tests/e2e/web_dashboard_mutations.mjs
python3 scripts/check-dashboard-core-mutations.py
# Focused mailbox isolation probe (the CI gate runs every case).
python3 scripts/check-dashboard-core-mutations.py --case expose-restricted-mailbox-projects
```

Use `AI_MEMORY_BIN` for another built executable and `AI_MEMORY_BROWSER` for an
existing Chromium executable. On Linux, the runner also discovers Chromium in
the Playwright cache. An absent executable, missing browser, failed setup or
JavaScript exception fails the command. The Linux CI job installs the pinned
browser and runs both mutation gates without `continue-on-error` or retries.
The [browser installation reference](https://playwright.dev/docs/browsers)
describes the test browser's system dependencies and headless shell.

The browser runner starts its own Rust server, uses synthetic projects and a
fake root token, and drives Chromium through its native debugging protocol.
It uses no DOM replacements and contacts no running production server. Its
messages are sent through HTTP and claimed through the native CLI. Every run
preserves logs and a JSON report in the printed artifact directory; failed
browser cases also retain a screenshot.

Run the Rust mutation command alone, without another build or an editor writing
the selected source files. It temporarily changes clean tracked source files,
stores exact originals, and restores them in `finally`. A concurrent edit is
refused rather than overwritten. Each probe requires a passing baseline and a
specific failed test assertion; compiler errors and timeouts do not qualify.
Logs and originals are kept under `target/dashboard-core-mutations-*`.

## Behaviors protected

| Behavior | Automated regression guard |
| --- | --- |
| Only the selected tab is visible/accessibility-exposed; keyboard, history, reload and old section links work | `web_dashboard_browser.mjs`: `tabs`; Rust `operations_project_tabs_have_one_default_panel_and_matching_controls` |
| Recipient filtering includes workspace and project, clears stale selection, disables empty projects and recovers | Browser `recipients`; Rust `operations_inbox_filters_initial_agents_by_both_project_coordinates` |
| Inbox queue counts name each workspace/project, links and selectors open the matching mailbox, empty states do not claim new mail, reading leaves messages queued, collection reduces counts | Browser `mailbox-context`; Rust `operations_inbox_queue_context_hides_foreign_projects_and_preserves_mail` also refuses foreign/partial scopes and hides restricted project names with legitimate read/root controls |
| Sidebar notifications appear for actual queued mail, refresh without reload, link to the right workspace/project, disappear after collection and clear on authorization loss | Browser `notifications`; Rust `operations_notifications_are_read_only_and_hide_restricted_projects` covers access-filtered totals, bounded links, zero state, no caching and non-consumption |
| Pending notification links find older messages behind collected history | Rust `operations_pending_mail_is_filtered_before_history_pagination`; server state predicate runs before LIMIT/OFFSET |
| Resumed substantive work reopens scoped API/detail reads, leaves completed listings, refuses a foreign project and closes after another SessionEnd | Browser `resumed-session`; store `session_activity` tests preserve the persisted watermark and owner/project controls |
| Legacy browser reads cannot send; invalid tokens are refused and cleared; authorized sends reach the chosen harness once | Browser `delivery`, against real HTTP auth and native CLI claims; Rust `operations_message_write_obeys_root_user_and_project_grants`, `operations_message_requires_write_access_to_both_project_coordinates`, store `addressed_messages_are_claimed_once_only_by_the_selected_harness` |
| Sanitization, admission rejection, foreign origin and unknown recipients | Rust `operations_browser_messages_use_the_configured_sanitizer_and_admission`, `operations_browser_send_refuses_cross_origin_unknown_recipient_and_empty_body`; browser `delivery` checks escaped message text |
| Usage reaches the server before session-end, repeated reports do not add calls, refresh preserves active tab/filter/focus | Browser `live-usage`; hooks `live_token_calls_update_open_sessions_without_an_end_event` |
| The native hook actually reads Claude/Codex transcripts for each supported event | CLI `native_hook_reports_live_usage_for_every_supported_event_and_harness` uses the built executable and inspects the delivered spool payload |
| Streaming/replayed counters cannot decrease the displayed response or disagree with session totals | Store `token_calls_keep_partial_reports_monotonic_in_rows_and_aggregates` |
| Reused response ids stay separate across sessions, agents and projects | Store `token_calls_group_by_session_and_agent_without_merging_foreign_responses`; web ownership/aggregate tests |
| Codex cached input is excluded from fresh input | CLI `live_token_codex_reads_modern_usage_without_counting_cached_input_twice`; migration `v90_normalizes_codex_cache_once_and_preserves_claude` |
| Private/malformed response ids are refused; private model labels are scrubbed before the 64-character bound | Hooks `live_token_calls_scrub_private_labels_before_truncating` checks a private value crossing the limit, with a public-label control |
| Light/dark selection persists; system mode follows OS changes; palettes differ | Browser `themes` |
| Navigation remains visible with usable targets and no page overflow at 320, 390 and 768 px | Browser `responsive` |
| Bundled guide HTML/Markdown/JSON, search, release selection, mount prefixes, traversal refusal | `tests/suite/documentation.rs`; browser `documentation` uses the real authenticated server |
| Changing a release loads its notes without a button, keeps links/history/reload consistent and works with manual submission without JavaScript | Browser `documentation` |

## Prove the guards detect faults

The browser campaign first runs every case against the original application.
It then changes one served HTML response at a time through Chromium's network
interception. The tested controller still runs in Chromium. A mutant counts
only if the response was changed and the intended behavioral case failed.

| Deliberate fault | Required failing case |
| --- | --- |
| Display every tab panel | `tabs` |
| Ignore recipient workspace | `recipients` |
| Ignore recipient project | `recipients` |
| Retain the write credential | `delivery` |
| Enable duplicate sending while a request is pending | `delivery` |
| Enable sending to an empty project after a request completes | `delivery` |
| Disable automatic usage refresh | `live-usage` |
| Stop saving the selected theme | `themes` |
| Capture browser navigation shortcuts in the tab controller | `tabs` |
| Disable automatic release submission | `documentation` |
| Restore a stale release selection on browser history navigation | `documentation` |
| Show a fabricated new-message alert with no queued mail | `mailbox-context` |
| Ignore the workspace when selecting a mailbox | `mailbox-context` |
| Expose restricted projects in the mailbox overview | Mailbox context isolation regression |
| Stop loading sidebar notifications | `notifications` |
| Ignore a notification's workspace | `notifications` |
| Expose restricted notification totals or project names | Notification isolation regression |
| Apply history pagination before its state filter | Pending-mail pagination regression |
| Treat resumed substantive session work as ended | Store `resumed_summary_is_open_and_preserves_the_persisted_end` |
| Capture usage only on session-end | Native-hook event/harness integration test |
| Count Codex cache as fresh input | Codex transcript parser regression |
| Overwrite a response with lower replayed counters | Response/session consistency regression |
| Remove harness checks from the claim lookup and atomic update | Directed harness claim regression |

Browser results are summarized in `target/web-dashboard-mutations.json`.
Source anchors for injecting a fault need updating when the implementation is
reorganized; a fault that was not injected fails the campaign. Assertions stay
about the behavior above rather than a particular function body.

These are bounded probes, not proof against every possible defect. Rust tests
also run the existing authorization, isolation, concurrency and recovery suites.
The browser gate currently covers Chromium on Linux; it does not establish
Firefox/Safari rendering or pixel-perfect appearance. A behavior-preserving
refactor should pass the regression tests.
