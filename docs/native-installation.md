# Native Slack Connector

The app runs on Windows x64, Windows 11 ARM64, Mac Intel, Apple silicon, and Linux x64/ARM64. Each download includes the native connector and a private Python runtime. Extract the whole download; open **Slack Connector.app** on Mac or **acp-slack-adapter** on Windows/Linux. The current preview is unsigned and unnotarized. Verify the published checksum and follow the operating system's normal review flow; do not disable its security controls.

The app opens its own first-account screen in your browser. Choose a connector password, then use the existing [Slack authorization](onboarding.md) and agent connection screens. It does not choose an agent runtime, create a conversation session, or start work. The optional startup checkbox registers only this connector for the current user. Settings includes a control to change that choice later.

Guided Slack authorization downloads the fixed, checksum-verified official Slack CLI when needed. Slack's Windows CLI is x64; Windows 11 ARM uses the operating system's x64 compatibility for that vendor tool. The connector and private Python are ARM64 on that platform. Actual workspace authorization remains subject to Slack permissions and administrator approval. Manual app credentials remain available in the existing setup screen.

## Existing installations

Linux PAM and HTTPS remain the defaults for explicit `init` and installer-created installations. Opening an existing installation preserves its owner, authentication, network settings, credentials, sessions and update ownership. Fresh native app installations use a local connector password and an exact loopback HTTP address; the browser does not require a locally generated certificate. This address does not expose the service over the LAN. A required local API proof binds its identity and origin before any local management credential is sent, on the same connection.

A native app is installed under the current user's private application data directory, with a `default` installation folder. `--home` selects a separate installation explicitly. Existing folders are not overwritten. The downloaded app keeps setup dependencies within that installation; it does not install Python or the Slack CLI system-wide.

## Update and recover

For an independent native installation, open **Settings → Advanced settings → Connector updates**. **Check for updates** prepares a published native release. **Install update** defers while a turn, message delivery, or setup operation is unresolved. The worker temporarily closes new turn admission, restarts only this connector, and verifies the result before reopening admission. Incoming Slack evidence remains in the existing store.

Only the connector executable and its private setup runtime change. A failed verification restores the previous versions. Credentials, agent configuration and conversation databases are never rewound. **Restore previous version** uses the retained backup. An interrupted pre-cutover request can be closed through **Recover interrupted update** without redispatching it. Critical-phase recovery restores the retained previous executable/runtime. If automatic recovery is incomplete, the UI reports it and admission stays closed; retain the installation folder and its `native-updates` receipts for recovery rather than deleting or recreating it.

Reopening an interrupted installation keeps its management and recovery controls available without starting the Slack connection. Unreadable update records are reported and preserved. A replacement process may start only for its still-current saved launch intent, so an old delayed process cannot cross a newer recovery operation.

An installation with externally enrolled saved-connection/update controls keeps that owner; this native updater does not compete with it. Existing Linux systemd update controls are unchanged. No separate fleet service or new scheduler is required.

## Management surface

Existing authenticated management APIs remain available. Native additions are:

- `GET auth/session`: `auth_mode` and `first_account_needed` describe the sign-in screen.
- `POST auth/first-account`: `{setup_token, password, start_at_signin}` consumes the private first-launch capability and creates a protected Argon2id credential once. A lost reply cannot replace an existing password; sign in to reconcile.
- `GET local-proof?nonce=…`: a domain-separated installation proof. It does not authenticate the caller.
- `GET/PUT desktop/startup`: observed registration receipt and `{enabled}` current-user startup control.
- `GET/POST desktop/updates`: current update receipt or `{action, request_id, plan_id?}`. Actions are `check`, `apply`, `rollback`, and `recover`; request IDs are durable and cannot be reused for another operation. No URL, executable, service name or shell command is accepted.

The private worker's `desktop/update-readiness` and `desktop/update-stop` calls are tied to its existing installation and saved plan. They are not agent tools. The [umbrella installation boundary](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/installations.md) remains authoritative for shared promises.
