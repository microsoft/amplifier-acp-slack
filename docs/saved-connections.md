# Saved Slack connections

A Slack connector remains an independent installation. An optional Instance
Connections page can save and operate it using the installation's existing host
helper. This does not change its ACP engine or grant agent access to host commands.

`acp-slack-adapter --home /fixed/home connection-enrollment` prints a descriptor
for the installation, including its fixed `status`, `connect` and `disconnect`
commands. Merge it into the helper's `service_connections.connections` by ID,
preserving other enrollment and policies. Enroll a service entry with id `slack`
and label `Slack`. A service need not advertise Add until setup is available.
The installation binary and registration are owner controlled, outside agent mounts.
The emitted executable path is the current binary; use the installation's stable
binary path when generating it, not a temporary build or version directory.

Commands accept the [Instance service connection protocol](https://github.com/microsoft/amplifier-constellation-instance/blob/main/docs/service-connections.md)
on stdin, with an immutable request ID and revision. The authenticated local API
is `/api/v1/saved-connection`; raw arbitrary commands are never accepted.

Disconnect blocks new reply admission, waits for running jobs and uncertain or
pending speech delivery, then reports `context_quiesced` before disabling the
socket supervisor. The host must acknowledge the report before the next status
phase. It retains credentials, configuration, conversation bindings and history.
Connect reports `context_ready` under closed admission, then starts the existing
configured socket and waits for fresh connection evidence from the current
supervisor. Lost replies reconcile through status; commands are not replayed.
A known authorization/configuration failure remains paused and is shown as failed.
Uncertain delivery or interruption remains pending for inspection.

Disconnect controls this connector's delivery. Slack's displayed presence dot
is managed separately by Slack; it is not proof of connection health. Connect
confirms the current socket, not a synthetic model response or every historical
agent session. No model prompt is sent by connection control.

## Add another Slack connection

The optional Linux provider `ops/connections/setup.py` uses the existing domain
installer to create a separate, initially paused connection. The owner preinstalls
the release binary, installer and PAM policy, and reserves actual reachable HTTPS
routes and free local ports. This provider does not grant network access or install
system packages. Its control state must be outside connector homes/agent mounts.

Example owner configuration (paths/routes are installation-specific):

```json
{
  "schema": 1,
  "state": "/owner/slack-setup/state",
  "data_root": "/owner/slack-homes",
  "agent_home": "/owner/instance",
  "binary": "/owner/releases/acp-slack-adapter",
  "installer": "/owner/slack-tools/scripts/install.py",
  "slots": [{
    "id": "second-workspace", "label": "Second workspace",
    "bind": "127.0.0.1", "port": 18480,
    "public_url": "https://approved-route.example"
  }]
}
```

Run `python /fixed/setup.py --config /fixed/config.json --action enrollment` to
emit its service definition. The browser can supply only a label and one approved
slot. The provider validates both, journals before effects and never overwrites an
existing home. After timeout/restart, status inspects the same owned setup without
rerunning the installer. Partial setup is retained for reconciliation. Successful
Add returns the fixed connection descriptor; the host atomically appends it without
rewriting other components. Setup does not enroll updates automatically.

Open the saved connection's Settings to complete Slack's own account approval.
Finishing authorization preserves the admission gate; use Connect in the Instance
page to begin receiving work. Where configured, `saved-connections.json` contains
`{"url":"https://own-instance.example/#connections"}` and the connector displays
Manage saved connections. Existing standalone installations retain their current
Start/Pause controls until enrolled. Do not point that link at another agent.

The connection operation and update participant share the existing admission lock.
Enrolled updates and connection actions must use the same host worker lock;
operator-side changes must also coordinate with this installation lifecycle.
