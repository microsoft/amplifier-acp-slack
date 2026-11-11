# A connection that can stand alone

Each installation has a stable connector ID, its own copied executable, config,
management address, credentials, SQLite conversation projection, inbox, outbox,
session references, and activity. It owns no agent persona, provider authentication
or model runtime. ACP is the work boundary. Management is a separate optional
interface belonging to the connector itself.

The optional `init --agent-home` import is an operator convenience: it resolves a
standard stdio command. Runtime code has no dependency on Agent Instance. A remote
stdio endpoint can be launched through a deliberately configured SSH command;
`acp.cwd` is an absolute directory on the agent's host, not the connector's host.
Native network transports can be added as separate transport implementations as
ACP standardizes them. Its management HTTP API is not ACP.

## Conversation/session routing

The context key is a UUID derived from Slack workspace, channel, and thread.
The main channel discussion and its individual threads are separate contexts.
One context has at most one active turn; separate contexts can work concurrently.

For each turn, the connector launches the configured ACP client command, initializes
the endpoint, loads the recorded session when `agentCapabilities.loadSession` is
advertised, or creates a session otherwise. It sends a bounded current Slack view
for that context. The endpoint supplies its own identity and instructions. Native
session state remains with that endpoint; the connector stores its opaque ID and
whether it can be resumed. The current driver records `session_replaced` and
creates a new session after
a non-authentication load failure, rebuilding context from Slack. This describes
the implementation; it does not establish successful restoration or an exception
to the shared [preservation and recovery intent](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/harness-lifecycle.v1-candidate.md).
Evaluate that path when changing recovery behavior. Session separation here is
routing, not an assertion about
the backend's memory or filesystem isolation.

If ACP initialization fails, the connector terminates that process and attempts
the handshake once more after a short delay, within the original turn/check
timeout. No session or prompt has been sent at that point. This handles transient
endpoint startup failures without recognizing a particular agent implementation.
The retry is recorded as `acp_initialization_retry`; raw backend error payloads
are not placed in activity. Session creation and failed prompts are not retried
by this mechanism, because work may already have started.

The durable outbox transaction checks conversation version, source revision,
deletion, and current channel/participation scope before changing ready to sending.
After a multipart reply has begun, rate-limit retries still check its source and
connection scope, but its own delivered parts do not invalidate the remainder.
State already marked sending at a crash becomes
uncertain. Work interrupted during an ACP turn is not automatically replayed.
The conversation projection and service observations are private local data;
they are not a workspace-wide archive or message search service.

### Long replies

ACP replies remain Markdown. Replies up to 100,000 characters are delivered as
ordered Slack messages of at most 10,000 characters, preserving their conversation
or thread. Boundaries prefer complete lines and paragraphs; long code blocks close
and reopen their fence. Links and inline code are not cut in half. An indivisible
Markdown element larger than one message is rejected with a delivery diagnostic;
the complete source remains in the local outbox rather than being silently cut.

Every part has a durable client message ID and receipt. A rate-limit retry skips
confirmed parts. A network ambiguity or process interruption never automatically
replays an attempted part. Source edits and revoked connection scope can stop the
remaining delivery. `response_delivery` includes each part's state and Slack link;
a partly delivered response is not presented as wholly received.

### Generated responses and actual delivery

ACP history can contain a response that the connector withheld after a newer
Slack message arrived. The next turn is told explicitly that this draft was not
posted. An already queued job receives that notice without losing
its newer trigger, and coalescing preserves it.

Every prompt also includes `response_delivery`: at most twelve recent completed
jobs from this agent in this exact conversation/thread, with job ID, trigger
message ID, delivery state, and a Slack message ID when available. States
distinguish delivered, not delivered, pending, uncertain, no response, and unknown.
The observation is dated and bounded; absence of a receipt proves nothing.
It includes neither draft text nor tool output and is recorded with the turn's
activity metadata for inspection through the existing API.

The delivery instructions tell the agent to compare its private draft with the
actual conversation before contributing again. An unpublished proposal is not
shared agreement. Pending and uncertain sends are not automatically reposted,
and tool work must not be repeated merely because its response was withheld.
This is connector context over ordinary ACP, not an agent-specific protocol
extension or a guarantee that the model will always interpret it correctly.

## Management provider

`GET /api/v1/descriptor` advertises implementation/version, connector kind,
capabilities, schema, OpenAPI, UI and control profile. Its namespace is
`io.github.bkrabach.acp-slack-adapter`; its management profile is
`acp-slack-adapter.management/1`. Those are implementation-specific extension
contracts and are not ACP standards.

| Interface | Purpose | Access |
| --- | --- | --- |
| `GET status`, `activity`, `conversations` | Health, decisions, deliveries and session references | observe |
| `GET permissions`, `POST permissions/{id}/decision` | Private ACP tool requests and explicit single-use decisions | admin |
| `GET descriptor`, `schema`, `openapi.json` | Capability discovery and automation | observe |
| `GET/PUT config` | Draft configuration, validation and revision-safe changes | admin |
| `POST credentials/slack` | Verify/store app and bot credentials; derive IDs | admin |
| `GET secrets`, `PUT/DELETE secrets/{name}` | Private ACP credential references | admin |
| `POST check`, `setup/complete`, `lifecycle` | Validate, enable and pause this connection | admin |
| `GET/POST tokens`, `DELETE tokens/{id}` | Create scoped, expiring management access | admin |
| `GET/POST views`, `GET views/{id}` | Independent display state | ui |
| `POST views/{id}/commands`, `ack` | Semantic display control and rendered revision | ui |

All paths in the table are below `/api/v1`. The packaged UI uses these same
operations, and the CLI `api` command can invoke every one. A fleet controller
should request only the scopes it needs. Agent/work credentials do not implicitly
grant connector-management access. Browser PAM sessions use HttpOnly secure
cookies with CSRF protection; browser view state never stores credentials.

Configuration writes require `expected_revision`. Display commands independently
require their view's current revision. A controller can navigate, edit a draft,
select a conversation, set filters, focus a control, scroll, and open/close a modal.
The browser acknowledges the revision after applying it. Secret fields use the
write-only credential endpoint rather than the draft/view channel.

## Composing management clients

Use `?embed=1` to hide the standalone navigation. Assets and API URLs are relative,
so a parent can place an authenticated same-origin proxy under a component path.
Explicit `server.embed_origins` allow other HTTPS frame ancestors, but embedding
itself grants no access; third-party browser cookie restrictions still apply.

A fleet console should keep three distinct provider roles: agent work over ACP,
component management over advertised optional interfaces, and host provisioning
through a separately authorized installer/host provider. An ACP-only agent stays
attachable even if it offers no management UI. The fleet does not own the lifetime
of installed agents or connectors merely because it displays them.

The descriptor and API are enough for a fleet to inspect or control this connector
without reproducing its UI. The same UI can be embedded or opened independently.
Creating new connectors on another machine still requires explicit host access;
neither ACP nor an observe credential grants that power.

## Optional ACP client capabilities

[Client capabilities](client-capabilities.md) describes the single approval policy
setting, including human decisions or audited automatic single-use approval,
local file/terminal authority and the relationship to the endpoint’s own tools.
Slack interactions are accepted only over the verified app Socket Mode connection;
there is no unauthenticated public approval webhook. Management decisions use
separate authenticated administrator authority.
