# Self-contained Slack setup

The default setup authorizes the connector once using Slack’s CLI login flow:
a private `/slackauthticket` command, Slack approval, then a confirmation code.
**Copy command & open Slack** copies that command and opens the selected workspace; the
confirmation field remains on the connector page. If clipboard access or a new
tab is blocked, the same page offers the command and a direct workspace link.
Expired saved access goes directly to renewal instead of another failing install.
The page follows `next_action` rather than treating a stored credential as valid.
The connector creates a separate Slack app for the agent and privately receives
its Socket Mode and bot tokens through the CLI’s documented deploy hook. The
same local authorization manages the app icon; there is no separate image-token
step. Workspace administrators retain their installation approval policy.

Each installation owns its pinned Slack CLI, private credential store, refresh
rotation, app association, and recovery state. Retrying setup reuses the recorded
app. For a [declared workspace](#one-connection-one-workspace), confirmation
finishes creation and starts the new connection immediately; otherwise its owner
confirms the approved workspace first.
Existing manual connections are preserved and can opt into managed access;
confirming that access returns to the connection page without reinstalling or
restarting a deliberately paused connection. Working legacy credentials do not
trigger an authorization screen.
No shared provisioning service or Constellation agent extension is required for
messaging. Portable images remain an optional ACP identity extension.

`GET /api/v1/setup/slack` reports `authorization` (`managed`, `legacy`, `missing`,
or `needs_attention`), `connection_ready`, `connection_running`, and `next_action`; authenticated administrator
`POST` actions are `authorize`, `confirm` (write-only `challenge`), `connect`
(optional `team_id`: the approved workspace the caller was shown), and `discard`.
Status also reports `intended_workspace` (`team_id`, `name`) and `authorized_workspace`
(`team_id`, `domain`) separately, each `null` when unknown, and `workspace_mismatch`
when a saved approval is for another workspace than the declared one.
The CLI’s generic API command and `window.slackConnector.act` expose the same
workflow. Ticket commands and confirmation codes never enter persisted UI state,
audit events, or agent prompts. Internal credential retrieval is not a public API.

Slack still uses different credential types internally. This flow manages them
for the user rather than presenting them as separate setup tasks. It is not a
generic Slack device-code OAuth grant, and catalog distribution is not required.
The manual manifest/token flow remains under Advanced setup.

Validated with Slack CLI 4.12.0: a disposable app was created and installed,
both message credentials reached the private deploy hook, and the test app was
removed. Automated tests cover retries, workspace binding, credential isolation,
expired challenges, and the guided browser flow. Human Slack approval is still
required on a new installation; the existing fleet was not reauthorized or reinstalled.
Declared and confirmed workspaces are covered by fixtures with fictional
identifiers only; they were not exercised against a real Slack workspace.

References: [CLI authorization](https://docs.slack.dev/tools/slack-cli/guides/authorizing-the-slack-cli/),
[deploy hook](https://docs.slack.dev/tools/slack-cli/reference/hooks/#deploy-optional),
[app distribution](https://docs.slack.dev/app-management/distribution/).

The normal connected page shows the agent and
**Open in Slack**. Preferences and troubleshooting stay under **Settings**, with
credentials and connection internals under **Advanced**. Saved views from an
older version cannot reopen a completed authorization flow.

### One connection, one workspace

Two connections for one agent look alike until each names its workspace, and
Slack approves the setup command in whichever workspace it is sent. The setup
page therefore always states its target, beside **Connect to Slack** and beside
the setup command.

A workspace is declared in `slack.team_id`, with an optional `slack.workspace_name`,
by the [installer or the configuration API](installation.md#declare-the-slack-workspace).
Until the agent’s app exists that ID is only an intention: `connection_ready`
stays false and the connection cannot start. With a declared workspace:

- an approval from that workspace creates the app in one step, as before;
- an approval from any other workspace creates nothing. The refusal names where
  access was approved and where the connection belongs. Slack has used that
  ticket, so the staged approval and the ticket are both removed and the next
  attempt starts with a new command. Previously saved access is untouched;
- manually pasted app credentials from another workspace are refused the same way.

With no declared workspace, `confirm` saves the approval and stops at
`next_action: "connect"`. The page says which workspace Slack approved and asks
before creating the agent there. `connect` creates the app; `discard` removes
that local approval so another workspace can be authorized. `discard` refuses
once app setup has been prepared, even if no app was recorded: a missing local
record does not prove a timed-out deploy created nothing at Slack. Saved access
is kept so that attempt can be reconciled; discard sends no request or retry.
An unused approval removed before setup is not revoked at Slack, and its token
expires on Slack's own schedule. A connected installation
is never asked to authorize again; its page shows **Connected to** its workspace.

### Slack authorization destination

A workspace-only Slack link can restore any previously opened channel. When the
CLI status supplies a workspace domain, the browser opens
`https://<workspace>.slack.com/messages/@slackbot` instead. Slack resolves the
private conversation for the signed-in person. This route was verified in a live
browser; if sign-in loses the destination, the page offers **Open Slack again**.
The setup command is copied to the clipboard, never included in the destination
URL.

A declared workspace has no domain before its first authorization. With only its
ID, the browser opens `https://slack.com/app_redirect?team=<ID>&channel=USLACKBOT`.
Slack’s [deep linking](https://docs.slack.dev/interactivity/deep-linking) documents
`app_redirect` with `team`, the target workspace, and `channel`, the conversation
to open; `USLACKBOT` is Slackbot’s member ID in every workspace. That page does
not itself promise that a member ID is accepted as `channel`, and this route has
not been verified in a live browser. The page therefore also links the workspace
itself (`https://app.slack.com/client/<ID>`) in case Slack shows a missing page;
its instructions to open Slackbot still apply. An unknown workspace falls back to
Slack’s workspace selection, with explicit instructions to open Slackbot.

The approval and challenge code appear only after the person sends the command
in Slack. The connector labels that step explicitly and keeps the manual copy
fallback available. See Slack’s [CLI authorization flow](https://docs.slack.dev/tools/slack-cli/guides/authorizing-the-slack-cli/).

### After connecting: invite the agent into a channel

A newly connected agent is a member of no channel. Slack delivers channel
messages only where the agent’s app is a member, and the app has no permission
to join a channel by itself. Until a person adds it, the agent hears direct
messages only, and nothing in Slack says why a channel stays quiet.

To bring it into a channel, open that channel in Slack and send
`/invite @<agent name>`. Direct messages need no invitation: they work as soon
as the connection is ready, unless **Allow direct messages** is turned off.

The connected page states this next step under **Open in Slack**. It says
**Not in any channel yet** when Slack listed no channel for the agent at its
last connection and no channel message has arrived since. `status` reports the
same observation as `state.in_channel`: `false`, `true`, or `null` before the
first listing. The connector takes it from the `users.conversations` call it
already makes to recover missed messages after each connection, and from any
later channel message. It adds no Slack permission and no polling, so an
invitation shows on the page when the first message from that channel arrives
or at the next reconnection, and leaving the last channel shows at the next
reconnection. This was checked with fixtures, not against a real Slack workspace.
