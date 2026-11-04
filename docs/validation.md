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

The [Actions page](https://github.com/microsoft/amplifier-acp-slack/actions) records checks
against the published source. Earlier private deployment reports and their
transcripts are excluded. A passing fixture verifies its tested boundary, not
general agent quality, human approval, or live tenant compatibility.
