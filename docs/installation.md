# Install and operate a Slack connection

For a Windows, Mac or Linux download, follow [native installation](native-installation.md).
It includes the connector's private Python runtime; Git, system Python and build
tools are not prerequisites. The source installation below is a separate path
for developers and server administrators.

## Install one connection from source

This source installer supports Linux service hosts. Windows, Mac, Linux, iOS
and Android can use the browser interface and Slack clients. The browser is independent of the service host operating system.

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

For an agent already installed with Constellation Instance, the optional convenience flag
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
shared Constellation service is involved. Guided authorization downloads a
checksum-verified Slack CLI into this installation when needed; it needs access
to GitHub and Slack on either installation path. Source installations need
Python 3; native downloads use their included interpreter. The installed agent
remains a standard ACP endpoint.

**Advanced: use existing app credentials** retains the manual manifest/token path.
Existing connections keep their apps, credentials, and conversations. They can
use **Connect image updates** when image access is missing, without
reinstalling the app. Healthy connections show status and **Open in Slack**;
additional controls live under **Settings → Advanced**. See [onboarding](onboarding.md) for the flow and boundaries.

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

## Process boundary

Separate installation folders and processes under one OS account do not isolate
a hostile agent implementation. Select and authorize the ACP endpoint accordingly.
