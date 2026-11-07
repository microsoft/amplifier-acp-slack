# ACP Slack Adapter

Part of [Amplifier Constellation](https://github.com/microsoft/amplifier-constellation).

Give an existing ACP agent its own Slack presence. Install one independent
connector per Slack app, connect it to the agent's ACP endpoint, and invite that
app into conversations. Slack owns the conversations; the agent owns its identity,
instructions, tools and durable agent state.

This is a separate executable, installation folder and service. It can be used
through its CLI, configuration files, HTTPS management API, or packaged browser
interface. It does not require a fleet manager, Agent Instance, or the earlier
workshop implementation.

```text
Slack app ← Socket Mode → this connector ← standard ACP stdio → your agent
                                ↑
                     optional management API + UI
```

## Install one connection

Linux is the supported service host in this release. Windows, Mac, Linux, iOS
and Android can use the browser interface and Slack clients. Native Windows/macOS
service installation is future work.

Install Git, then clone the public source:

```sh
git clone https://github.com/microsoft/amplifier-acp-slack.git
cd amplifier-acp-slack
./install.sh --label "My agent"
```

The installer builds a pinned dependency set, installs missing build prerequisites
on apt-based Linux, installs a PAM policy if needed, copies its executable into
a new private folder, and starts an unconfigured user service. It prints the URL
and opens it when a local desktop is available. Existing folders are never silently
overwritten. Python 3, systemd, and GitHub access are bootstrap requirements.

For an agent already installed with Agent Instance, the optional convenience flag
prefills its ordinary ACP entrypoint and working directory:

```sh
./install.sh --label Helper \
  --home "$HOME/.local/share/acp-slack-adapter/helper" \
  --agent-home "$HOME/.local/share/agent-instance/first-agent"
```

For another ACP implementation, enter its launch command, arguments, and working
directory during setup. The agent must already be installed and authenticated.
The connector neither copies its profile nor installs a replacement agent.

The random management port becomes stable after first startup. `--port` overrides
it; `--bind` defaults to LAN access. `--public-url` advertises an already-configured
HTTPS reverse proxy, including Tailscale Serve. It does not create that proxy.
Direct LAN HTTPS uses a private per-installation CA; `/certificate/root.crt`
downloads its public certificate. Prefer a trusted HTTPS hostname when available.
PAM sign-in uses the account that installed this connector. Passwords never travel
over an HTTP management listener.

## Files and independent ACP runtimes

Supported Slack attachments now become capability-checked ACP content, and native ACP image/audio/document outputs can become Slack files. See [media behavior and limits](docs/media.md). Existing Slack apps need the added `files:read` and `files:write` permissions authorized. The [OpenCode validation](docs/opencode-compatibility.md) demonstrates a genuinely independent ACP implementation through this adapter with real model turns and no live Slack posts.

## Connect Slack

1. Open the printed URL and sign in. The installed agent is already selected.
2. Choose **Connect to Slack**, then **Copy command & open Slack**. In Slackbot,
   paste into the message box and press Enter. Choose **Confirm**, then bring the
   confirmation code back to this page. When the workspace is known, the connector
   opens its Slackbot conversation directly; otherwise choose the workspace first.
3. The connector creates the agent’s app, obtains its message credentials, checks
   the ACP connection, and starts listening. Image updates use the same authorization.
4. Invite the agent into a channel or open its direct messages.

Workspace app approval policies still apply. An administrator may need to approve
installation. Each connector keeps its own authorization and renewal state; no
shared Constellation service is involved. The first authorization downloads a
checksum-verified Slack CLI into this installation. Python 3 and access to GitHub
and Slack are required. The installed agent remains a standard ACP endpoint.

**Advanced: use existing app credentials** retains the manual manifest/token path.
Existing connections keep their apps, credentials, and conversations. They can
use **Connect image updates** when image access is missing, without
reinstalling the app. Healthy connections show status and **Open in Slack**;
additional controls live under **Settings → Advanced**. See [onboarding](docs/onboarding.md) for the flow and boundaries.

## Connected conversation context

Enable **Conversation context tools** under **Settings → Advanced** to give an agent
read-only navigation between a conversation, its parent messages, and thread
replies. The connector supplies a small orientation on each turn and standard
HTTP MCP tools when the ACP agent advertises support. A reachable, trusted HTTPS
public URL is required for the agent host to call back.

An optional owner Slack member ID links the installation owner to their Slack
identity. In that person's verified one-to-one DM, context can include channels
both the owner and bot can access. Group conversations remain within their current
channel. Another person's DM never joins the owner's context. Access is checked
against Slack on retrieval, and source links and coverage accompany results.

To carry context into other experiences, register this provider with an agent
that supports the optional `agent-instance.context/1` contract. Its provider ID
is `slack:<connector-id>`, base URL is the connector's public URL, and its scoped
credential has `context` access. The agent calls `/context/v1/query`; standard
ACP clients receive tools through ordinary MCP session setup. The optional
`io.github.bkrabach.conversation` ACP metadata passes a temporary audience grant,
never a Slack credential. Agents without that extension can still use the
connector's ordinary MCP tools within Slack.

Reading another conversation does not change where a reply is delivered. Agents
and connectors do not read each other's databases. This is source retrieval,
not an automatic merge of all sessions or a durable preference-memory system.
See [the continuity design](docs/conversation-continuity.md) for the contract and
remaining work.

Slack Socket Mode makes an outbound connection, so Slack does not need a public
URL to your machine. [Slack Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode/)

## Operate without the browser

```sh
acp-slack-adapter --home /path/to/connection status
acp-slack-adapter --home /path/to/connection check --target all
acp-slack-adapter --home /path/to/connection stop
acp-slack-adapter --home /path/to/connection start
acp-slack-adapter --home /path/to/connection config get
acp-slack-adapter --home /path/to/connection api GET descriptor
acp-slack-adapter --home /path/to/connection api GET activity
acp-slack-adapter --home /path/to/connection api POST views --body-file view.json
```

`api METHOD PATH --body-file FILE` exposes the same operations as the UI, including
its display state. Credential values belong in private input files or password
fields, never command arguments. `secret NAME --file FILE` writes a named ACP
environment secret; `acp.secret_env` maps environment variables to those names.

Use `config set --file FILE` for online revision-checked updates. `--offline` is
available only while the service is stopped. Once a connection has been used,
changing the Slack identity or ACP endpoint requires another installation so an
unrelated agent cannot inherit its transcripts accidentally. Upgrading an agent
behind an unchanged entrypoint remains independent of this connector.

## What the connector preserves

- Standard ACP initialization, session creation, prompt delivery and optional
  session loading. Generic ACP agents can return normal text in addressed mode.
- Independent sessions for simultaneous rooms/threads. If the agent cannot reload
  sessions, each invocation receives only that conversation's bounded Slack view.
- Persist-before-ack ingestion, event/message deduplication, self-echo suppression,
  per-conversation serialization, and bounded parallel conversations.
- Source edits, deletes and changed conversation scope are checked before delivery.
  A changed conversation can be reconsidered without silently losing an addressed
  request. Previously performed actions must not be blindly repeated.
- Explicit rate-limit retry. An ambiguous send is marked uncertain, not automatically
  retried. A matching Slack echo can confirm it. There is no exactly-once guarantee
  for external tool effects or a send already in flight when configuration changes.
- Meaningful activity and delivery outcomes. UI polling, draft updates and render
  acknowledgements do not fill the activity feed.

The connector supports [interactive permissions and optional local client tools](docs/client-capabilities.md).
The single approval policy, `acp.client.approvals_required`, defaults to `true`.
The human who initiated a live turn can review and allow or deny its single-use
ACP tool requests in Slack, without a separate `acp.client.approval_users` list.
The connector owner can also explicitly decide through its dashboard/API using
separate administrator authority. Turning the policy off automatically selects
an advertised `allow_once` option for each eligible tool request and records the
decision without posting an approval card. It does not grant persistent permission,
supply credentials, enable client tools, or bypass agent/tool/service restrictions.
Sign-in and clarification questions still require interaction.

Filesystem and isolated Linux terminal callbacks are opt-in and bounded to local
workspace roots. Agent-owned tools remain subject to the chosen agent's policy.
Supported attachments are translated between Slack files and ACP content within
the [media capabilities and limits](docs/media.md). Proactive cross-room posting
is not implemented; this is not a general Slack automation API. Process/folder
independence under one OS account does not isolate a hostile agent implementation.

## Fleet integration and verification

[Codex compatibility](docs/codex-compatibility.md) records two independent upstream
Codex instances working through this connector without Agent Instance, including
actual Slack tool tasks, restored sessions, concurrent rooms and the remaining
client-capability limits.

[Architecture and management boundary](docs/architecture.md) explains discovery,
scoped access, embedding and semantic UI control. [Validation](docs/validation.md)
records real process, browser, PAM and optional live-agent checks. The future fleet
experience can attach to these interfaces; neither the agent nor connector needs
that experience to keep working.

## Contributing

> [!NOTE]
> This project is not currently accepting external contributions, but we're actively working toward opening this up. We value community input and look forward to collaborating in the future. For now, feel free to fork and experiment!

Most contributions require you to agree to a
Contributor License Agreement (CLA) declaring that you have the right to, and actually do, grant us
the rights to use your contribution. For details, visit [Contributor License Agreements](https://cla.opensource.microsoft.com).

When you submit a pull request, a CLA bot will automatically determine whether you need to provide
a CLA and decorate the PR appropriately (e.g., status check, comment). Simply follow the instructions
provided by the bot. You will only need to do this once across all repos using our CLA.

This project has adopted the [Microsoft Open Source Code of Conduct](https://opensource.microsoft.com/codeofconduct/).
For more information see the [Code of Conduct FAQ](https://opensource.microsoft.com/codeofconduct/faq/) or
contact [opencode@microsoft.com](mailto:opencode@microsoft.com) with any additional questions or comments.

## Trademarks

This project may contain trademarks or logos for projects, products, or services. Authorized use of Microsoft
trademarks or logos is subject to and must follow
[Microsoft's Trademark & Brand Guidelines](https://www.microsoft.com/legal/intellectualproperty/trademarks/usage/general).
Use of Microsoft trademarks or logos in modified versions of this project must not cause confusion or imply Microsoft sponsorship.
Any use of third-party trademarks or logos are subject to those third-party's policies.

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

Automatic Slack app portraits from compatible agents: [agent images](docs/agent-images.md).

Verified, installation-local updates are available for Linux systemd installations.
See [Updating one Slack connector](docs/updates.md) for the CLI, management API,
and optional owner-DM agent controls.

Bounded agent collaboration and durable delivery waits: [collaboration waits](docs/collaboration-waits.md).

### Optional shared attention

When an ACP endpoint advertises enabled `com.microsoft.amplifier.attention` v1,
ambient group events use its `_amplifier/attention` method after session setup.
The connector supplies bounded public channel/thread facts and the original Slack
timestamp. It checks the source again before attaching the returned one-use offer
to `session/prompt`. Instance owns the evaluator and budgets; no decision provider,
credential or evaluator executable is configured in this adapter. A hold creates
no prompt; an offer skips the legacy LLM participation preflight. The agent still
may return pass. Ordinary ACP endpoints retain the existing bounded preflight;
DMs, accepted-work continuation and explicit mentions-only installations retain
their existing behavior. Do not run the superseded adapter-owned helper alongside
this path. Source changes, permission checks and delivery receipts still apply.

ACP chunks reach this adapter as they arrive. Public group output is buffered
until the final speak/pass decision, so private drafts and pass envelopes do not
leak into Slack. Work/tool progress can update separate activity cards. Attention
admission does not itself change this presentation policy.

### Speech and activity

Thinking and tool activity are temporary status indicators. Completed public commentary is a new, permanent Slack message, so people and other agents can reply between contributions. Spoken messages are never edited into later answers. Explicit corrections are new messages.

When an agent negotiates the shared ACP message-completion capability, commentary can be delivered before its turn ends. Without that capability, explicitly identified commentary waits for a successful turn result. Plain ACP and opaque agents still work; a pause in text or an unfinished draft is not assumed to be a message boundary. Private thought chunks are never published. Final-answer selection remains separate from commentary.

A durable per-message delivery record prevents automatic replay after uncertain sends. Confirmed Slack events or history can settle a missing receipt. Commentary and the final answer share the turn’s existing reply allowance. The status API exposes `public_speech` delivery counts, and Activity records bounded delivery receipts without duplicating message contents.

[Saved connections](docs/saved-connections.md) documents optional Instance Add/Connect/Disconnect integration and installation-owned setup.
