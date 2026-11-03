# Keep an agent's Slack image in sync

Upload an image in the connected agent's identity settings. Guided Slack setup
uses its existing authorization to manage the icon automatically. Existing
connections can choose **Connection settings → Agent image → Authorize Slack for
image updates**, approve once, and keep their existing app and message credentials.
No separate image key is needed on this path. Slack CLI owns credential renewal
inside this installation; its refresh token is never duplicated into another store.

**Advanced image access** retains the manual app-configuration token option.
Use a separate token pair per connector when choosing that path. Authorization
stays in this connector's private secret store and never enters ACP prompts,
configuration exports, or persisted UI state.

The connector reads the optional `agent-identity/1` extension over its configured
ACP connection every two minutes while running. It uploads changed image bytes
using Slack's documented [`apps.icon.set`](https://docs.slack.dev/reference/methods/apps.icon.set/)
API. This changes the actual app icon, including DMs and profile surfaces, rather
than overriding only the appearance of individual messages. No public image
hosting, `chat:write.customize`, transcript access or model prompt is needed.
The app configuration token needs `app_configurations:write` and access to this
app. Manager-app tokens can only change apps they created. Slack remains the
authority on whether a token may change the selected app.

The bot identity independently verifies the destination app before an upload.
Icons are deduplicated by checksum; accepted updates and retry deadlines survive
restarts. A replacement is picked up automatically. Removing a previously synced
portrait uploads the agent's neutral fallback. An agent without an uploaded
portrait leaves a manually selected Slack icon alone. Disconnecting image access
stops sync and leaves Slack's existing icon in place. Slack clients may briefly
cache old icons after an accepted update.

Failures appear in image status and do not interrupt conversations. Rate limits
respect `Retry-After`; uncertain responses may safely retry the idempotent icon
replacement after a delay. Expiring configuration tokens rotate using
[`tooling.tokens.rotate`](https://docs.slack.dev/reference/methods/tooling.tokens.rotate/)
when a refresh token is configured. A lost/revoked token needs reconnecting.

## Generic ACP compatibility

Portable identity is optional, **not part of base ACP**. Any ACP implementation
can implement the [documented extension](https://github.com/microsoft/amplifier-constellation-instance/blob/main/docs/agent-identity.md).
The connector only calls the exact advertised method and accepts bounded,
checksum-verified PNG bytes. It does not read the agent's installation files,
use its management token, or fetch an agent-provided image URL. Agents without
this extension keep their normal Slack icon and ordinary ACP behavior.

## Automation

- `GET /api/v1/identity`: synchronization status (`observe`).
- `POST /api/v1/identity` with `{}`: sync now (`admin`), respecting retry deadlines.
- `PUT /api/v1/identity/credential` with `{ "token": "...", "refresh_token": "..." }`:
  save private configuration access (`admin`). Optional `expires_at` is Unix seconds.
- `DELETE /api/v1/identity/credential`: stop synchronization and remove legacy image access. Guided Slack authorization remains available for app management.

The UI exposes `identity.open`, `identity.credentials`, `identity.sync` and
`identity.disconnect` through `slackConnector.act`. Its view/modal state uses the
existing authenticated view API. Credential input values are excluded from it.
