# Self-contained Slack setup

The default setup authorizes the connector once using Slack’s CLI login flow:
a private `/slackauthticket` command, Slack approval, then a confirmation code.
**Copy & open Slack** copies that command and opens the selected workspace; the
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
app. Confirmation finishes creation and starts the new connection immediately.
Existing manual connections are preserved and can opt into managed access;
confirming that access returns to the connection page without reinstalling or
restarting a deliberately paused connection. Working legacy credentials do not
trigger an authorization screen.
No shared provisioning service or Constellation agent extension is required for
messaging. Portable images remain an optional ACP identity extension.

`GET /api/v1/setup/slack` reports `authorization` (`managed`, `legacy`, `missing`,
or `needs_attention`), `connection_ready`, `connection_running`, and `next_action`; authenticated administrator
`POST` actions are `authorize`, `confirm` (write-only `challenge`), and `connect`.
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

References: [CLI authorization](https://docs.slack.dev/tools/slack-cli/guides/authorizing-the-slack-cli/),
[deploy hook](https://docs.slack.dev/tools/slack-cli/reference/hooks/#deploy-optional),
[app distribution](https://docs.slack.dev/app-management/distribution/).

The normal connected page shows the agent and
**Open in Slack**. Preferences and troubleshooting stay under **Settings**, with
credentials and connection internals under **Advanced**. Saved views from an
older version cannot reopen a completed authorization flow.
