# ACP permissions and local client tools

Version 0.3.0 adds client-side capabilities without depending on an agent brand,
Agent Instance, persona files, or an agent management API. The default remains
text chat with no connector filesystem or terminal access and no automatic
permission grants.

## A deliberate human decision

Set `acp.client.approval_users` to the Slack member IDs authorized to approve work.
An ACP `session/request_permission` during a live turn creates a request in the
originating Slack conversation/thread. Only the listed human who initiated that
turn may use its buttons. A different group member, bot, workspace, app, message,
stale request, or replay cannot grant permission.

The card offers the endpoint's single-use allow/reject options and cancellation.
Persistent `allow_always`/`reject_always` grants are deliberately omitted. If an
endpoint only offers persistent choices, the request is cancelled. Tool call
updates are merged with the permission request so a request containing only a
tool-call ID can still show its actual arguments.

**Review details** opens a private Slack modal for that same authorized person.
Full request details also appear in the connector dashboard for its owner. Slack
cards show a description and affected file locations, not raw arguments in the
shared room. Long private modal details are explicitly shortened; the dashboard
retains the complete bounded request. The operator should reject an action when
its supplied description is insufficient or misleading.

Requests expire, and editing/deleting the source, changing the conversation,
reconfiguring/pausing the connector, or ending/restarting the turn invalidates
outstanding requests. A standard `cancelled` prompt result ends quietly without
posting partial output or retrying the work. No response means no permission. Restart never replays the
interrupted work. Closed cards lose their buttons when Slack confirms the update;
a stale button remains ineffective even if that visual update fails.

The connector binds every choice to its job, ACP session, configuration revision,
source revision, conversation version, designated member, and Slack message.
Control cards are excluded from the conversation projection so the connector's
own approval UI cannot invalidate a reply or cause another turn.

The management owner can also explicitly decide through the dashboard or API.
That is separate administrator authority, recorded as the actual authenticated
principal; a caller cannot supply or impersonate a Slack identity. An `observe`
or `ui` credential cannot read private tool arguments or approve work.

## Configuration

All settings below live under `acp.client` and appear in **Connection settings →
Advanced settings → Permissions and client tools**. The existing revision-safe
config API and CLI use exactly the same fields.

```json
{
  "read_text_file": true,
  "write_text_file": true,
  "terminal": true,
  "workspace_roots": ["/home/example/agents/alpha/workspace"],
  "approval_users": ["U0123456789"],
  "permission_timeout_seconds": 180,
  "max_file_bytes": 1048576,
  "terminal_output_bytes": 262144,
  "terminal_timeout_seconds": 120
}
```

Workspace roots are **local to the connector host**. The configured agent working
folder must exist inside one of them. No remote path translation is implied by
launching an endpoint through SSH. The connector installation and credentials
cannot be exposed by a client root. These capabilities are independent of tools
that the endpoint implements itself.

- **Filesystem:** standard `fs/read_text_file` and `fs/write_text_file`, including
  one-based line ranges. Paths must be absolute and stay under explicit roots.
  Descriptor-relative opens reject symlinks and traversal, only regular text
  files are accepted, and byte limits apply. Parent folders for writes are
  created as necessary. Available on Unix; disabled on unsupported platforms.
- **Terminal:** standard create/output/wait/kill/release. The connector uses Linux
  bubblewrap, never an unrestricted fallback. Only selected roots and system
  binaries/libraries are mounted; `/etc`, the host home, credentials, and network
  access are absent. Workspace mounts are read-only unless `write_text_file` is
  enabled. Environment is explicitly constructed, stdin is closed, output is
  bounded and truncates at character boundaries, terminal IDs are scoped to the
  current session/turn, and process trees are killed on release, timeout, turn
  completion or service stop. A pending wait does not block a later kill request.

The handshake advertises only enabled capabilities supported on this platform.
Client callbacks are accepted for the established session during a prompt turn;
requests during initialize/session creation/loading have no live human work
binding and are refused. Agents requiring callbacks during session restoration
need further interoperability work. Agent-owned tools remain subject to the
agent's policy: configuring connector roots does **not** constrain an endpoint
that already executes its own commands under the same OS account.

## Observability and automation

- `GET /api/v1/status` includes `client_capabilities` and `pending_permissions`.
- `GET /api/v1/permissions` returns pending and recent requests, decisions, exact
  actors, expiry and complete tool details; requires `admin` access.
- `POST /api/v1/permissions/{id}/decision` with `{"option_id":"<advertised ID>"}`
  selects that single-use option. `{"option_id":null}` cancels. Requires `admin`.
- Activity records request/decision/expiry and callback method/success. It never
  includes file contents, terminal output, tool arguments or provider stderr.
- The dashboard's review modal and decision controls are exposed through
  `window.slackConnector.act("permission.inspect", {permissionId})` and
  `.act("permission.decide", {permissionId, optionId})`; display state also uses
  the existing semantic view API. Omit `optionId` to cancel.

Completed private request records are retained up to 1,000; the UI/API lists
pending requests first and then recent history, capped at 100.

## Verification

`tests/client_capabilities.py` uses an independent ACP implementation and a
loopback Slack Socket Mode service with actual connector processes. It exercises
file reads/writes, path and symlink escapes, mismatched sessions, terminal
isolation/output/release, concurrent wait/kill, actual interaction envelopes,
private detail views, wrong users/rooms/workspaces/apps, replay, denial, expiry,
management decisions, source edits, restart, and exclusion of control messages
from history. This fixture does not establish that any particular model asks for
permission at the right time.

Rust checks additionally enforce readonly terminal mounts, terminal lifetime,
unsupported/default-disabled capabilities, and management authorization. Browser
checks exercise tool-detail review and both visual and programmatic decisions.

Protocol references: [ACP filesystem](https://agentclientprotocol.com/protocol/v1/file-system),
[ACP terminals](https://agentclientprotocol.com/protocol/v1/terminals),
[ACP tool permissions](https://agentclientprotocol.com/protocol/v1/tool-calls),
[Slack interaction payloads](https://docs.slack.dev/reference/interaction-payloads/block_actions-payload/),
[Slack Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode/).
