# Independent OpenCode validation

On November 1, 2026, OpenCode **1.18.22** was exercised through this adapter using its unmodified `opencode acp --pure` entrypoint. This is a separate implementation from Codex ACP and our agents. See [OpenCode’s ACP documentation](https://dev.opencode.ai/docs/acp/).

The normal `command`/`args`/`cwd`/environment configuration was used, with no Agent Instance dependency or private endpoint API. A separate HOME and XDG data/config/cache/state tree isolated the run from the owner’s global OpenCode setup. A named provider credential was copied only into the temporary connector’s private secret store, never printed, and deleted when the test ended. Model: `anthropic/claude-sonnet-4-5`.

Five real model turns passed:

1. A first private conversation returned OpenCode’s identity and a freshly generated room marker.
2. A simultaneous second conversation returned its own requested answer without the first room’s marker.
3. A follow-up in the first conversation used ACP `session/load` and recalled that marker.
4. An unaddressed human-to-human group exchange produced an ambient pass and no message.
5. A direct group mention received the requested response.

The endpoint advertised session loading, retained distinct session IDs for the two rooms and reused the first room’s session. One startup initialization retry occurred before any prompt; no model turn was replayed. There were four loopback deliveries and **zero real Slack posts**. These observations demonstrate generic ACP negotiation and the tested routing/participation contracts; they do not establish every OpenCode tool or model combination.

Reproduce with an ordinary ACP configuration and an explicitly selected credential environment name:

```sh
python3 tests/live_agent.py --acp-config /private/path/opencode-acp.json \
  --expected-name OpenCode --credential-environment ANTHROPIC_API_KEY \
  --allow-model-call --group-check --output /private/path/report.json
```

The harness never calls Slack’s real API. `--allow-model-call` explicitly opts into the selected bounded provider calls. Fixture media/client-tool tests are separate from this independent runtime evidence. The local report for this run is under ignored `private/opencode-validation/report.json`.
