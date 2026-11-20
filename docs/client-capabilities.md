# ACP permissions and local client tools

Client-side capabilities do not depend on an agent brand,
the Constellation Instance host, persona files, or an agent management API.
The default remains text chat with no connector filesystem or terminal access and no automatic
permission grants.

## Optional agent-host directories

Operators can set `acp.additional_directories` through the existing revision-safe
config API or CLI, for example `"additional_directories": ["/srv/projects/shared"]`
inside `acp`. It defaults to an empty list and is omitted from saved configuration
when empty, so the default remains readable by older connector packages.
Clear it before rolling back to a package that does not recognize this field.
Slack messages never supply or modify this setting.

Like `acp.cwd`, these are absolute paths on the **agent host**, using the
connector platform's native absolute-path convention. Up to 16 paths of 4096
bytes each are allowed; relative paths, control characters (including NUL),
parent traversal and unrestricted filesystem roots are rejected. The connector
does not resolve, canonicalize, or check the existence of these remote paths.
The endpoint remains responsible for authorizing and enforcing their scope.

A nonempty list requires an object-valued
`agentCapabilities.sessionCapabilities.additionalDirectories` advertisement
(`{}` is sufficient). Otherwise setup fails before any session request or prompt;
the connector never silently drops the requested scope. When configured, it sends
`additionalDirectories` on session new/load/resume, operator probes and
conversation-mode loads, including reconnect verification. When empty it omits
the wire field, even for a capable endpoint.

Directory edits do not change the endpoint key: the next turn loads or resumes the
same saved session with the current list, including omission after clearing.
A failed load with a nonempty list preserves that binding and sends neither a
replacement session request nor a prompt; review the grant and endpoint policy
before retrying. Default-empty load fallback and resume failure behavior remain
unchanged. These settings do not change `acp.client.workspace_roots`, tool
approvals, transport, identity or Slack scopes, and do not grant connector-local
filesystem access.

## One approval setting

`acp.client.approvals_required` defaults to `true`, including when upgrading an
existing configuration. Turn it off to let this connector automatically approve
the agent's individual ACP tool requests. Each automatic decision selects only
an advertised `allow_once`, is recorded in Activity and permission history as
`policy:automatic`, and posts no approval card. Unknown, ambiguous, denial-only,
or persistent-only choices are never treated as permission. Hard restrictions
in the agent, tool, filesystem, network, or connected service remain in force.
This setting does not supply credentials or enable client tools.

The policy applies to admitted human and bot turns; it does not admit new channels,
users, or bot traffic. Every automatic decision still requires a live, unchanged
source/job/conversation binding and the correct ACP session. Ordinary questions
and sign-in prompts remain interactive; the connector never invents answers.

With approvals required, an ACP `session/request_permission` during a live
human-initiated turn creates a request in the originating Slack conversation or
thread. The human who initiated that turn may use its buttons, without a separate
list of approvers. The legacy `approval_users` field remains readable for old
configurations but is deprecated and no longer restricts requesting humans. A different group member, bot, workspace, app, message,
stale request, or replay cannot grant permission.

The card offers the endpoint's `allow_once`, `reject_once`, `allow_always`, and
`reject_always` choices when supplied, plus cancellation. Slack returns the exact
selected option ID to that same live request; it does not invent a reusable rule
or automatically match future requests. The agent owns the scope, persistence,
and revocation of an Always choice. Review the agent's stated scope before
choosing it, and manage or revoke remembered rules in the agent's settings.
Unknown choices are omitted; a request with no supported choices is cancelled.
With automatic approval enabled, the connector still selects only `allow_once`,
never a persistent choice. Tool call updates are merged with the permission
request so a request containing only a tool-call ID can still show its actual
arguments.

**View request** opens a private Slack modal for that same authorized person.
Other members receive a private explanation that the details belong to the
request recipient; no request contents or recipient identity are shown. If Slack
cannot open the window, the connector attempts one private notice to the clicking
member in the original channel. These controls never change the decision.
Full request details also appear in the connector dashboard for its owner. Slack
cards show a description and affected file locations, not raw arguments in the
shared room. Long private modal details are explicitly shortened; the dashboard
retains the complete bounded request. The operator should reject an action when
its supplied description is insufficient or misleading.

Each turn defaults to 15 minutes of human approval time;
`acp.client.permission_timeout_seconds` accepts 1–86,400 seconds. Human waiting
does not consume the agent's execution timeout. Successive requests expire after
the remaining allowance and show that reduced time on their cards. Once the
allowance is exhausted, further requests return cancelled without posting another
card. Existing explicit values remain unchanged until the owner updates them.

Editing/deleting the source, changing the conversation, reconfiguring/pausing the
connector, or ending/restarting the turn invalidates outstanding requests. A
cancelled direct conversation receives a brief stopped-work notice; partial agent
output is withheld and work is never retried automatically. No response means no
permission. Closed cards retain the original action summary and private details,
remove decision buttons, and distinguish approval from actual execution. A stale
button remains ineffective even if its visual update fails.

An endpoint's `$/cancel_request` withdraws only the matching pending approval,
including during session setup. Its card closes without ending the conversation;
later clicks cannot grant permission and a separate current request can still be
answered. Withdrawal does not undo a decision already recorded or establish
whether an action ran. It adds no approval time or automatic retry.

The connector binds every choice to its job, ACP session, configuration revision,
source revision, conversation version, designated member, and Slack message.
Control cards are excluded from the conversation projection so the connector's
own approval UI cannot invalidate a reply or cause another turn.

The management owner can also explicitly decide through the dashboard or API.
That is separate administrator authority, recorded as the actual authenticated
principal; a caller cannot supply or impersonate a Slack identity. An `observe`
or `ui` credential cannot read private tool arguments or approve work.

## Configuration

All settings below live under `acp.client`. The approval setting is exposed in
Connection settings; client tools and limits remain advanced options. The
revision-safe config API and CLI use exactly the same fields.

```json
{
  "read_text_file": true,
  "write_text_file": true,
  "terminal": true,
  "workspace_roots": ["/home/example/agents/alpha/workspace"],
  "approvals_required": true,
  "permission_timeout_seconds": 900,
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
session creation/loading callbacks are bound to the initiating live turn and
the candidate session ID. Operator probes and mode-management requests do not
permit tool callbacks. Agent-owned tools remain subject to the
agent's policy: configuring connector roots does **not** constrain an endpoint
that already executes its own commands under the same OS account.

## Observability and automation

- `GET /api/v1/status` includes `client_capabilities` and `pending_permissions`.
- `GET /api/v1/permissions` returns pending and recent requests, decisions, exact
  actors, expiry and complete tool details; requires `admin` access.
- `POST /api/v1/permissions/{id}/decision` with `{"option_id":"<advertised ID>"}`
  selects that exact advertised option. Any remembered scope belongs to the
  agent. `{"option_id":null}` cancels. Requires `admin`.
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

`tests/approval_policy.rs` also exercises a separate ACP peer with automatic
approval: no Slack credential/card is needed, only single-use choices are selected,
source/session changes cannot gain authority, and bot turns follow the configured
policy. The Slack fixtures verify requesting humans can decide without a separate
approver list and ordinary questions still require their answers.

Rust checks additionally enforce readonly terminal mounts, terminal lifetime,
unsupported/default-disabled capabilities, and management authorization. Browser
checks exercise tool-detail review and both visual and programmatic decisions.

Protocol references: [ACP filesystem](https://agentclientprotocol.com/protocol/v1/file-system),
[ACP terminals](https://agentclientprotocol.com/protocol/v1/terminals),
[ACP tool permissions](https://agentclientprotocol.com/protocol/v1/tool-calls),
[Slack interaction payloads](https://docs.slack.dev/reference/interaction-payloads/block_actions-payload/),
[Slack Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode/).

An active tool request survives replies from other agents in the same conversation. A newer human message requires reassessment before further actions. Editing or deleting the initiating message, stopping the turn, or changing the approval policy still invalidates its old requests. The same rule applies to automatic single-use decisions and pending human decisions.

Slack thread-bookkeeping events (for example, changed reply counts with unchanged message content) do not advance the source revision or interrupt active work. Actual message, attachment, sender, routing or deletion changes remain material and require reassessment.

## Agent questions and sign-in

Ordinary ACP form and URL questions, including clarification and sign-in prompts,
appear as a **Respond privately** control regardless of
`acp.client.approvals_required`. Only the human who initiated the live turn can
open and answer its Slack dialog; no `acp.client.approval_users` list is required.
Other channel members see no question contents or answers. Automatic tool approval
does not answer questions or complete sign-in.
The operator may also inspect `GET /api/v1/questions` and explicitly answer via
`POST /api/v1/questions/{id}/decision` with `{action, content?}` using the separate
admin credential. That is recorded as an operator decision, not impersonation.

Supported agent login methods run on the same ACP connection and retry session
setup once after explicit choice. Provider pages open only on a click; opening a
page is consent, not proof of completed authentication. Terminal-only methods
remain a host sign-in step. Prompts are never retried for authentication.

Question bodies and responses are memory-only. Cancellation, source edits,
config changes, timeout, or process exit invalidate them. Controls are bound to
the workspace, app, initiating person, message, and opened dialog. Slack limits
on fields, choices, and URL length are enforced before display; unsupported
schemas are rejected instead of silently dropping constraints. Secret inputs
belong on the provider's page. Setup callbacks must use the current request ID
or an already established session ID; tool-scoped questions require a known tool.

Validation uses independent ACP peers and a local Socket Mode service, including
login, URL opening, typed answers, wrong-user/view rejection, invalid input and
transcript isolation. Live Slack UI confirmation is a separate check.
