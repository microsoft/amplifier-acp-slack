# Validation

Run the repository's automated checks from a clean checkout. The source fixtures
use fictional identities and temporary state. They do not contain installed
agents, personal profiles, account credentials, or exported conversations.

```sh
cargo fmt --check
cargo test --locked
cargo build --locked
python3 tests/process_integration.py
python3 tests/media.py
python3 tests/client_capabilities.py
python3 tests/morning_reliability.py
python3 tests/slack_huddles.py
python3 tests/owner_message.py
cargo clippy --locked --all-targets -- -D warnings
```

See the test scripts and CI workflow for prerequisites and platform conditions.
Browser tests use disposable local services and require Playwright/Chromium.
Linux service tests may require PAM development headers; isolated command tests
also need the documented OS isolation prerequisites. Opt-in real account, model,
or messaging tests remain separate from deterministic checks.

The morning-reliability fixture starts a local Slack substitute and an ACP
fixture. It checks that a follow-up preserves outstanding work, an explicit stop
prevents continuation, and reconnect recovers unseen messages without repeating
accepted work. It sends no messages to a real Slack workspace. The automatic
approval tests also exercise concurrent activity writes so a database lock is
not misreported to the agent as a user rejecting an action.

The owner-message fixture uses the actual adapter, its granted MCP tools and
ordinary ACP with a local Slack substitute. It checks that the only possible
recipient is the configured owner's direct conversation, that nothing is sent
about a conversation the owner is not in, from a direct conversation or in
answer to another agent, the
bound per response, and recovery of a send whose acknowledgment was lost. It
does not establish that a real workspace lets a given app open that
conversation, or a real model's judgment about what deserves its owner's
attention.

The [Actions page](https://github.com/microsoft/amplifier-acp-slack/actions) records checks
against the published source. Earlier private deployment reports and their
transcripts are excluded. A passing fixture verifies its tested boundary, not
general agent quality, human approval, or live tenant compatibility.

The side-conversation fixture uses the actual adapter, its granted MCP tools and
ordinary ACP with a local Slack substitute. It checks quiet thread routing and
the original result destination, source/audience limits, and recovery of retained
sends. These checks do not establish real Slack notification preferences or
sustained social judgment; those remain separate from protocol coverage.
