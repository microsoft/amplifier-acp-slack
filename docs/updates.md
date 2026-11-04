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

## CLI walkthrough

Run these commands as the installation owner, with its service running and the
update policy enabled above. Replace `/path/to/installation` with the connector's
installation folder:

```sh
/path/to/installation/bin/acp-slack-adapter --home /path/to/installation update plan
/path/to/installation/bin/acp-slack-adapter --home /path/to/installation update status
```

The plan command returns JSON with an `id`; save that value as your plan ID.
Status wraps the latest plan in `current`: check that `current.id` matches your
saved ID and wait for `current.state` to become `ready`. Inspect its
`source_commit`, `run_url`, `artifact_digest`, `binary_sha256` and `up_to_date`
before deciding to apply. Planning alone does not replace the executable.
Replace `PLAN_ID` below with that returned `id` (not the source commit):

```sh
/path/to/installation/bin/acp-slack-adapter --home /path/to/installation update apply --plan-id PLAN_ID
/path/to/installation/bin/acp-slack-adapter --home /path/to/installation update status
```

Queuing is not completion: both plan and apply start asynchronous work. Check
status again until the same plan reaches its result; `succeeded` means the update
completed its health checks. If it is `deferred`, existing work or uncertain
reply delivery still needs attention: inspect `current.reason` and
`current.waiting_for` (`running_turns`, `pending_replies`, `uncertain_deliveries`).
Let work finish or reconcile uncertain delivery before retrying apply with the
same plan ID. Do not treat `failed`, `rolled_back` or `recovery_required` as a
successful update; inspect the reason, and resolve host issues before recovery.
The management endpoint may be briefly unavailable during restart; check status
again once the service is reachable.

For a plan in `succeeded`, rollback uses the same ID; `recovery_required` also
allows retrying that plan's rollback after the host issue is resolved:

```sh
/path/to/installation/bin/acp-slack-adapter --home /path/to/installation update rollback --plan-id PLAN_ID
/path/to/installation/bin/acp-slack-adapter --home /path/to/installation update status
```

Rollback is asynchronous too: confirm `current.id` and a `rolled_back` result,
not just a queued receipt. Normal rollback requires the installed binary still
to match that plan's updated binary and its backup to pass digest verification;
it is not an arbitrary version selector and does not restore the old database.

## How updates run

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
prior binary automatically. The managed server certificate may renew during restart
when it expires or network addresses change; authenticated HTTPS must still verify
against the unchanged trust root. Credentials and CA material remain protected.
Unexpected protected-file changes are reported separately from connection-health
failures. It never restores an old database over messages
received during the update. A crash during cutover resumes with recovery of the
prior binary. If rollback itself cannot become healthy, the receipt reports
`recovery_required` and new agent turns stay paused. An operator can retry the
same plan’s rollback after resolving the host issue. Backups remain available
for recovery. Successful updates therefore require backward-compatible
state migrations; this controller does not claim to reverse arbitrary future
schema migrations.

Validation uses disposable loopback installations and fake platform boundaries;
`python3 tests/updates.py` makes no live Slack posts or model calls.
