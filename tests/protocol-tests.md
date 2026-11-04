# Process and protocol integration

Build the service and run the integration test from the repository root:

```sh
cargo build
python3 -m venv /tmp/acp-slack-protocol-tests
/tmp/acp-slack-protocol-tests/bin/pip install aiohttp
/tmp/acp-slack-protocol-tests/bin/python tests/process_integration.py
/tmp/acp-slack-protocol-tests/bin/python tests/client_capabilities.py
```

Set `ACP_SLACK_TEST_BINARY` to an absolute executable path to test a release.
The fixture prints individual checks and a final JSON report; a failed check
returns a nonzero exit code and prints the temporary service diagnostics.

The test creates two independent installed adapter processes, each with private
configuration, state, and an ordinary Python ACP endpoint. One endpoint supports
`session/load`; the other does not. A local HTTP/WebSocket fixture speaks the
Slack API subset used by the connector. Only loopback addresses and `test-*`
credentials are used. Management checks validate HTTPS with the generated root
certificate and authenticate with the temporary installation's local credential.
No production credentials, real Slack messages, or model requests are involved.

Checks cover message/mention deduplication, self echoes, ambient silence, ordinary
text replies, DMs, independent rooms and threads, session resumption or fallback,
permission cancellation, output isolation, edits during work, malformed decisions,
workspace/app/channel boundaries, explicit rate-limit retry, uncertain delivery
after restart, process-tree shutdown, lifecycle pause/resume, and observations.
Initialization failures are exercised separately: one transient failure recovers
before prompting, a permanent failure stops after two attempts, and a failed
prompt is not retried by the initialization mechanism.

The first installation also receives and answers a DM with read-only update
controls. The test checks that their permissions remain unchanged. To exercise
an actual read-only container mount, set `ACP_SLACK_TEST_UPDATES` to a mounted
directory containing an empty `admission.lock`; use directory mode 0500 and
file mode 0400. The test then uses that mount instead of its local fixture.
The updater tests separately verify that host lock contention, drain leases,
and critical recovery continue to prevent new turns.

The endpoint's deterministic policy exists only to make these boundary conditions
repeatable. Passing these tests establishes connector behavior under those
conditions; it does not establish Slack service behavior, compatibility with every
ACP implementation, model judgment, or the quality of social participation.

The client-capability checks also require Linux, `/usr/bin/bwrap`, and permission
for bubblewrap to create user namespaces. On hosts with AppArmor restrictions,
an operator can review `ops/apparmor/bwrap.profile`; do not disable the global
restriction. CI installs this narrow executable profile only on its disposable
runner when necessary. The production installer does not change host policy.
