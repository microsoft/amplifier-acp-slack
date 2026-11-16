Slack 0.17.6 keeps each connection bound to its intended workspace. Setup shows the intended and approved workspaces separately, refuses mismatched credentials, and asks for confirmation before creating an app when no workspace was declared. A lost deployment reply keeps the saved access available for reconciliation. The connected page also explains how to invite the agent into a channel.

This patch improves API error reporting, writes terminal job status together with its explanation, and gives image synchronization a brief bounded wait for its lock. Newly installed Linux source services restart after outside termination while intentional service-manager stops remain stopped. Existing installations must apply the documented unit change before relying on that policy; updating the executable alone does not change an existing unit. See [the installation guide](https://github.com/microsoft/amplifier-acp-slack/blob/main/docs/installation.md#staying-up).

Workspace setup and channel guidance are covered by disposable fixtures. The workspace-ID-only Slackbot link is not a verified live Slack route; the workspace link and manual Slackbot instructions remain available. Existing credentials, conversation state, intentional pauses and update ownership are preserved.

Slack 0.17.5 adds an owner-local update command for existing Linux container installations using the supported fixed service recipe. It reuses the installation’s update records and host controls while preserving the disabled in-app update policy, container and service configuration, credentials, sessions, and work. Failed readiness restores the prior executable without replacing newer conversation data. This path requires the existing host Python, Docker, user systemd, and authenticated GitHub tooling; it does not add an agent-callable host command.

Slack 0.17.4 lets authorized clients inspect the outcome of an exact original Slack submission, including retained delivery evidence and last observed work references from compatible Agent Instances. Lookup reuses existing records and does not resend the input. Optional source/work support is negotiated; unsupported or unknown outcomes remain explicit.

Native Slack Connector downloads include the connector and its own private setup runtime. Open the app, choose a local connector password, and use the existing Slack authorization and agent connection screens. No developer toolchain, system Python or WSL is required.

Windows x64, Windows 11 ARM64, Mac Intel, Apple silicon, and Linux x64/ARM64 are built and tested on their respective platforms. The connector and Python are native for each processor. Slack currently publishes only an x64 Windows CLI; the optional guided Slack setup uses Windows 11's x64 compatibility on ARM. Manual Slack app credentials remain supported.

These preview downloads are not publisher-signed or notarized. Verify the published checksum; do not disable operating-system security protections. Linux existing PAM/HTTPS installations and external update enrollment retain their configuration and ownership.

Independent native installations offer published-release checks and reversible connector updates. Those operations preserve credentials and conversation state and wait for work and delivery to settle. They do not update the connected agent runtime or model.
