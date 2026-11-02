# Slack files and ACP content

Version 0.4.0 adds surface-specific media translation. Agent identity, tool behavior and original ACP content stay at the agent endpoint; Slack uploads, access tokens, content negotiation and delivery rules belong here.

The Slack app needs `files:read` and `files:write` in addition to its existing permissions. The generated manifest includes both. Existing app installations need their permissions updated and authorized before files can be read or published. Configuring media does not grant new Slack access.

## Incoming files

Only files attached to the current triggering Slack message are downloaded. History preserves bounded file metadata, not file bytes or signed private URLs. A fresh `files.info` lookup checks the file ID, external-file status, size and MIME type. External/remote-file links are never downloaded. Only HTTPS on exactly `files.slack.com` port 443 may receive the bot token. Automatic redirects are disabled; each redirect is independently validated before attaching credentials. Downloads have declared-size and actual streamed-byte limits.

| Slack upload | ACP representation |
| --- | --- |
| UTF-8 plain text, Markdown, CSV, JSON | Embedded text resource when `embeddedContext` is advertised; otherwise clearly labelled untrusted text |
| PNG, JPEG, GIF, WebP | Native image block only if `promptCapabilities.image` is advertised |
| MP3, WAV | Native audio block only if `promptCapabilities.audio` is advertised |
| PDF | Embedded binary resource only if `promptCapabilities.embeddedContext` is advertised |
| Other format, unavailable file, capability mismatch or limit | Explicit “not read” note; no claim that the file’s contents are known |

Magic-byte/type checks run before translation. Text is limited to 200KB, other supported files to 2MiB, and each turn accepts at most four. Capability support establishes a protocol representation, not proof that a model understood a particular document.

## Outgoing files

Native `agent_message_chunk` image/audio blocks and embedded resources with actual contents are converted to Slack files. No tool output is automatically published. Resource references do not cause local filesystem reads or arbitrary network fetching. Text resources become UTF-8 `.txt` documents. Unsupported or oversized final attachment content produces a visible note.

The adapter uses Slack’s current three-stage upload API: obtain an upload URL, upload bytes without the bot bearer token, then complete the upload into the fixed destination. The answer remains standard Markdown passed to Slack’s `markdown_text` renderer. Files appear under that answer’s thread, or the original thread when replying there.

File bytes are uploaded privately before the text message is sent. Once text delivery begins, a failure completing file sharing is **uncertain**, including rate limits at that stage. A text echo cannot mark the whole media reply delivered. The connector does not blindly resend the text or repeat model/tool work. Inspect Slack before deciding how to recover a partial delivery. Unshared staged uploads may remain until Slack expires them.

Output file bytes are stored in the private durable outbox so pending replies survive restarts without rerunning the agent. Existing text-only records migrate with an empty file list.

## Controls and tests

Configuration and dashboard offer `media.incoming`, `media.outgoing`, `media.max_files` (1–4), and `media.max_file_bytes` (1–2097152). UI controls use the same configuration API and semantic view drafts as other settings. The defaults enable supported files within these bounds.

`python3 tests/media.py` uses actual connector/ACP processes and a local Slack fixture. It checks image negotiation, text/resource fallback, external-file refusal, redirect and actual-byte bounds, private URL exclusion, output file upload, destination threads, unsupported resource fallback and uncertain partial delivery without duplicate sends. No real Slack messages are posted. Rust tests additionally reject deceptive hostnames, non-TLS URLs, alternate ports, local paths and invalid content signatures.

Primary references: [Slack file handling](https://docs.slack.dev/messaging/working-with-files/), [file object authentication](https://docs.slack.dev/reference/objects/file-object/), [external upload URL](https://docs.slack.dev/reference/methods/files.getUploadURLExternal), [complete upload](https://docs.slack.dev/reference/methods/files.completeUploadExternal).

The optional browser regression (`tests/media_browser.mjs`, with `PLAYWRIGHT_MODULE` and `CHROMIUM_PATH`) verifies media and continuity settings save through the UI/API and fit desktop and phone layouts. Version 0.4.1 corrects those form field mappings.

## Check installed permissions

Configuration → Attachments and images now has **Check file access**. It reads the
actual bot token's `x-oauth-scopes` response header from `auth.test`; the generated
manifest is not treated as proof that the app was reinstalled. Missing read/write
scopes are listed with a direct link to the existing app's OAuth settings. Add
those Bot Token Scopes and reinstall that app to the workspace, then check again.
**Enable attachments** verifies the grants again before updating both media options.

The same operations are exposed as authenticated administrator calls:
`POST /api/v1/media/readiness` and `POST /api/v1/media/enable`. Neither posts a
message, uploads a file, opens another socket, or invokes an agent. The cached
scope result and its check time are visible in `GET /api/v1/status` under `media`.
An absent scope header is reported as unverified, never as granted access.

## Explicit live smoke check

The operator-only example is excluded from automatic test runs. It accepts only
an existing DM belonging to the configured owner, verifies installed file grants,
and limits its three fixtures to 16KB total. It does not create channels, invoke a
model, or retry an uncertain send. Run it only when a real test message is wanted:

```sh
cargo run --example media_live -- --send-one-live-check /path/to/adapter D_EXISTING_DM tests/media_live_fixture.json
```

It returns a content-free receipt with file IDs, MIME types, byte hashes and the
message link. If checking a previously posted verification, use the read-only
mode to avoid another notification:

```sh
cargo run --example media_live -- --verify-existing-check /path/to/adapter D_EXISTING_DM tests/media_live_fixture.json MESSAGE_TIMESTAMP
```

The ordinary CI suite exercises 12 media scenarios with independent ACP and
loopback Slack processes, including the actual WAV MIME alias and refusal when
an endpoint does not advertise audio. These fixture checks send no real Slack
messages.

Reference: [Slack's granted-scope response header](https://docs.slack.dev/authentication/installing-with-oauth/).
