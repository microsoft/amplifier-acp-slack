# Connecting with fewer interactions

Count every required decision, click, page switch, copy, paste, and recovery step.
Moving a requirement behind a disclosure does not remove its cost. Measure the
full journey, including Slack's pages, rather than just this connector's screens.

## Current standalone flow

The connector presents one connection page. The installed ACP endpoint is already
selected. A prepared Slack manifest fills in app settings. The user supplies the
two Slack connection keys, optionally supplies image-management access, then
chooses **Connect to Slack**. That one action saves and verifies Slack access,
checks the agent, and starts the connection. Preferences and launch details remain
available without requiring a visit. Successful setup links to the agent's Slack DM.

| Connector interaction, after signing in | Previous | Current |
| --- | --- | --- |
| Required setup buttons, excluding the Slack app creation link | Continue, Verify & save, Continue, Start | Connect to Slack |
| Enter the already installed ACP endpoint | None | None |
| Supply Slack connection keys | Two paste operations | Two paste operations |
| Check agent connectivity | Optional extra button | Included in Connect |
| Recover after Slack access saves but the agent check fails | Navigate/check again | Retry Connect; saved keys are retained |
| Add optional image access | Separate settings/modal/save flow | Same setup form; still requires Slack configuration access |

These counts do not include Slack sign-in, workspace selection, approval, token
generation, copying, or switching between sites. Those remain significant work.
The current change removes three connector buttons, not all of Slack onboarding.

## Why appearance has different access

Slack assigns different permissions to receiving events, sending messages, and
managing an app. The first two use Socket Mode app-level and bot credentials.
Updating the actual app icon uses `apps.icon.set` and `app_configurations:write`.
An ordinary bot's messaging permission cannot grant itself app-management access.
Configuration credentials have broader access than an individual conversation,
so they remain connector-owned and never enter the agent's prompt or tools.

References: [token types](https://docs.slack.dev/authentication/tokens/),
[app icons](https://docs.slack.dev/reference/methods/apps.icon.set/), and
[manifest management](https://docs.slack.dev/app-manifests/configuring-apps-with-app-manifests/).

## Target teammate experience

The connector must own a self-contained setup and management lifecycle. A shared
Constellation provisioning, enrollment, credential, or identity service is not
required, including for image updates and recovery after initial setup. This
follows the [family contract's installation baseline](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/family.v1-candidate.md#self-contained-installation-baseline).

Starting from a fresh installation, the target is:

1. Connect the selected agent to Slack.
2. Complete the owner's authorization and any required workspace/app approval.
3. Open the agent's DM or choose a conversation.

The local setup tooling should provision access where Slack permits it, receive
credentials privately, discover the workspace and app identity, check
connectivity, and synchronize supported identity information. The target is no
separate image-authorization exercise after connection. Reuse valid authorization
within an installation, with one owner responsible for any rotating credential;
do not copy one rotating refresh token into independent installations. Different
owners or machines may need their own authorization. Count those steps rather
than hiding them behind an administrator's preparation work.

This is a target, not implemented OAuth or provisioning support. Slack's official
CLI is a candidate local setup helper: its installation flow creates apps,
obtains bot and Socket Mode credentials, and applies app icons using the owner's
workspace authorization. Its [deployment hooks](https://docs.slack.dev/tools/slack-cli/reference/hooks/#deploy-optional)
can hand credentials to an installation without manual token copying. Validate
that path end to end with this connector before advertising it as supported.

Ordinary app-install OAuth does not itself authorize creating arbitrary apps.
Slack's separate managed-app installation path is restricted to approved Slack
partners. See
[`managed_apps:install`](https://docs.slack.dev/reference/scopes/managed_apps.install/).
Do not advertise an ordinary OAuth button as sufficient to create and install an
arbitrary fleet of separately named bots. A single shared bot would also change
the individual agent identity and DM experience, so it is not a substitute here.

Each installation must retain its own state and standalone controls for renewal,
image changes, reconnection, and removal. Owners can run different connector
versions and implementations on different machines without coordinated updates.
A future fleet UI may invoke those controls; it must not own the only usable
setup path. A shared provisioning service would require a separate proposal and
must remain optional. Standard ACP and declared optional interfaces still apply.

## Automation and recovery

`window.slackConnector.act("setup.connect", values)` runs the same flow as the
button. Values can contain `app_token`, `bot_token`, `image_token`, and
`image_refresh_token`. Omit the messaging keys when already saved. Secrets stay in
private inputs and authenticated API requests, never in semantic display state.

Without the UI, use the existing management operations: save configuration,
`POST credentials/slack`, optional `PUT identity/credential`, `POST check`, then
`POST setup/complete` after a successful check. All require authenticated access.
The verified new credentials already include a Slack connection check, so the UI
checks only ACP on that attempt. Retries with saved keys check both connections.
Optional image failures report separately and do not prevent messaging.

The semantic view API exposes `setup_images`, `setup_preferences`, and
`setup_manifest_help` filters. `setup.preferences` and `setup.manifest-help` actions
toggle their respective views. `slack.agent.open` returns the same Slack DM link
as the on-screen control. Existing step navigation actions remain compatible and
now show the single connection page.

Browser checks cover defaults, optional controls, secret retention while editing,
exclusion from view history, recovery without repeated key entry, programmatic
setup, image-access failure, narrow layouts, and existing connection controls.
