# ACP Slack Adapter

Give an independently configured agent a Slack presence: invite it into a channel,
work with it in a thread, or use a direct conversation. Slack remains the source
of its conversations; the chosen ACP endpoint supplies the agent. This is the ecosystem’s
[Connection](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/connection.md)
into the platform’s own conversations and experience, independent of the agent’s
internal architecture.

Part of [Amplifier Constellation](https://github.com/microsoft/amplifier-constellation).
The [vision](docs/VISION.md) explains the value and design space. The
[contract map](contracts/README.md) locates shared promises and Slack-owned boundaries.

The supplied connector path:

```text
Slack app ← Socket Mode → this connector ← standard ACP stdio → your agent
                                ↑
                    installation management API
```

## Start here

Download a [native Slack Connector release](https://github.com/microsoft/amplifier-acp-slack/releases) for Windows, Mac or Linux, extract the complete download, and open the app. Choose a local connector password, then connect an already installed and authorized ACP agent and your Slack workspace. No developer toolchain or system Python is required. See [native installation and recovery](docs/native-installation.md) for platform details.

For an existing Linux server installation:

```sh
git clone https://github.com/microsoft/amplifier-acp-slack.git
cd amplifier-acp-slack
./install.sh --label "My agent"
```

The Linux installer creates a private connector installation and prints its
management URL. The agent must already be installed and authenticated. Follow
[installation](docs/installation.md) and [Slack authorization](docs/onboarding.md)
for prerequisites, credential renewal and external workspace approval.

The packaged browser interface is a development and administration surface, not
the intended product experience. Its controls also have CLI and management API
forms. No Constellation Instance host or fleet manager is required.

## Find the boundary for your work

- **Bring an idea:** [vision](docs/VISION.md), [contract map](contracts/README.md), and [working instructions](AGENTS.md).
- **Design Slack behavior:** [local connector agreement](contracts/connector.md).
- **Route and deliver:** [architecture and management API](docs/architecture.md), [public speech and attention](docs/harness.md), [side conversations](docs/side-conversations.md), [temporary work status](docs/work-status.md), [collaboration waits](docs/collaboration-waits.md).
- **Retrieve authorized sources:** [conversation context](docs/conversation-continuity.md).
- **Ask or act:** [permissions, client tools and questions](docs/client-capabilities.md), [media](docs/media.md).
- **Operate an installation:** [saved connections](docs/saved-connections.md), [updates](docs/updates.md), [agent images](docs/agent-images.md).
- **Check a change:** [validation](docs/validation.md). Inspect current code and advertised capabilities for the outcome at hand; a fixture pass is not live Slack or model-quality proof.

## Contributing

Invited collaborators: follow [Join the work](https://github.com/microsoft/amplifier-constellation#join-the-work)
within your agreed access and scope. The notice below covers general public intake.

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
