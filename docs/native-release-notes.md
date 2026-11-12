Native Slack Connector downloads include the connector and its own private setup runtime. Open the app, choose a local connector password, and use the existing Slack authorization and agent connection screens. No developer toolchain, system Python or WSL is required.

Windows x64, Windows 11 ARM64, Mac Intel, Apple silicon, and Linux x64/ARM64 are built and tested on their respective platforms. The connector and Python are native for each processor. Slack currently publishes only an x64 Windows CLI; the optional guided Slack setup uses Windows 11's x64 compatibility on ARM. Manual Slack app credentials remain supported.

These preview downloads are not publisher-signed or notarized. Verify the published checksum; do not disable operating-system security protections. Linux existing PAM/HTTPS installations and external update enrollment retain their configuration and ownership.

Independent native installations offer published-release checks and reversible connector updates. Those operations preserve credentials and conversation state and wait for work and delivery to settle. They do not update the connected agent runtime or model.
