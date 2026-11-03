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

1. Open the printed URL and sign in. Confirm the prefilled agent connection.
2. Use **Create app in Slack**. If Slack shows app choices, select **From a manifest → Continue**. Choose the workspace and create the prepared app.
3. In Slack's **Basic Information → App-Level Tokens**, create a token with
   `connections:write`. In **OAuth & Permissions**, install the app and copy its
   bot token. Paste both into the connector's two private credential fields.
4. Verify and start. In Slack, add the app to a channel, or open its direct message.

The setup derives app/workspace IDs and checks that the two credentials belong to
the same app. A separate app per agent gives each its own visible identity and
event stream. Do not start two connector installations with the same app tokens.

**When addressed** is the default: DMs and explicit mentions invite a reply.
Ordinary unmentioned messages still update that conversation's context.
**When useful** asks the agent to decide whether to speak or remain silent for
ordinary group messages too. One-to-one DMs and explicit @mentions still expect
a reply; the user can explicitly ask for silence. Group mode requests a speak/pass JSON decision through a
normal prompt; it is an optional behavior convention, not a standard ACP feature.
Malformed ambient decisions are withheld and recorded, not published as JSON.

Invite the same app into more channels to use the same individual in more places.
A channel allowlist can narrow its joined rooms. Every channel's main discussion
and every thread have separate session bindings; nothing automatically merges
their transcripts. The agent may intentionally share its own higher-level state.

### Message formatting

Agents return ordinary Markdown over ACP. At delivery, this connector uses
[Slack's `markdown_text` field](https://docs.slack.dev/reference/methods/chat.postMessage/)
so Slack renders bold, links, lists, headings, quotes, code, and tables as rich
content. The connector does not send Markdown through Slack's older `text`/mrkdwn
field or ask the agent to adopt a Slack-specific writing format. Code and URLs
reach the renderer unchanged, and the outbox retains the original Markdown.

Replies retain their destination, thread, and delivery ID; link and media previews
remain disabled. The existing 3,800-character contribution limit is below Slack's
12,000-character Markdown limit. Delivery failures follow the same retry policy;
an uncertain response is never retried in another format. This affects new replies;
previously posted messages are not rewritten.

## Connected conversation context

Enable **Conversation context tools** in Connection settings to give an agent
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
An explicitly configured human can review and allow or deny a single ACP request
in Slack; the connector owner has the same explicit action in its dashboard/API.
Filesystem and isolated Linux terminal callbacks are opt-in and bounded to local
workspace roots. Agent-owned tools remain subject to the chosen agent's policy.
Attachments and proactive cross-room posting are not implemented. These are text
conversations, not a general Slack automation API. Process/folder independence
under one OS account does not isolate a hostile agent implementation.

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

With `acp.client.approval_users` configured, ordinary ACP form and URL questions
appear as a **Respond privately** control. Only the initiating allowed human can
open its Slack dialog. Other channel members see no question contents or answers.
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
