# Updating one Slack connector

The optional updater changes this installation's connector executable. Its
agent, credentials, profiles, configuration, saved sessions and message store
stay in place. It uses the Linux systemd user-service installation currently
provided by `scripts/install.py`. macOS and Windows update controllers are not
implemented. Custom service commands and systemd drop-ins require operator
review and are rejected by this first implementation.

An operator enables updates once with the authenticated management API:

```json
PUT /api/v1/updates/policy
{"expected_revision":0,"enabled":true,"agent_enabled":false}
```

Set `agent_enabled` to true to additionally allow the connected agent to manage
this connector **from the verified owner's Slack DM only**. The owner must already
be linked in connection settings. This is an explicit management grant;
`context:owner` or a group conversation does not grant update authority. Turning
the policy off revokes queued work before cutover. Changes during a critical
cutover still permit recovery of the known-good binary.

Use `GET /api/v1/updates` or `acp-slack-adapter --home <installation> update`
to inspect status. The same `POST /api/v1/updates` supports:

- `{"action":"plan"}` to prepare the latest trusted build.
- `{"action":"apply","plan_id":"…"}` to queue that prepared plan.
- `{"action":"rollback","plan_id":"…"}` to queue rollback of that successful update.

The CLI accepts `update plan`, `update apply --plan-id …`, and
`update rollback --plan-id …`. Authorized owner-DM agents discover equivalent
`slack_connector_update_*` MCP tools. They supply no shell command, source URL,
repository, destination, service name, credential, or arbitrary binary path.
Other experiences can use the management API with administrator authority.

The source is fixed to `microsoft/amplifier-acp-slack` main. Planning requires a
successful push run of its `check.yml` workflow at that exact source revision,
then downloads the matching platform artifact and verifies GitHub's recorded
SHA-256 artifact digest. The plan records source revision, workflow/run identity,
archive digest and extracted binary digest. This is provenance from the trusted
GitHub repository and build service; it is not a claim of reproducible builds or
an independent security review. The host's GitHub CLI must be installed at
`/usr/bin/gh` and authenticated to download Actions artifacts. Credentials stay
with that host account and are never tool arguments or model context.

Apply runs in a detached, installation-specific systemd helper. New Slack
messages continue to be recorded while starting new agent turns is briefly
paused. Existing turns, human decisions and pending replies finish first;
uncertain delivery defers cutover instead of guessing, and releases the pause
so further agent work can continue. The same plan can be retried after delivery
is reconciled. The asking agent can
finish its turn and send its acknowledgement before its connector restarts.

The helper snapshots the prior binary, config and database, atomically replaces
only the executable, restarts the fixed service, and checks authenticated
management identity plus Slack connection health. Health failure restores the
prior binary automatically. It never restores an old database over messages
received during the update. A crash during cutover resumes with recovery of the
prior binary. If rollback itself cannot become healthy, the receipt reports
`recovery_required` and new agent turns stay paused. An operator can retry the
same plan’s rollback after resolving the host issue. Backups remain available
for recovery. Successful updates therefore require backward-compatible
state migrations; this controller does not claim to reverse arbitrary future
schema migrations.

Validation uses disposable loopback installations and fake platform boundaries;
`python3 tests/updates.py` makes no live Slack posts or model calls.
