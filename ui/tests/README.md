# Browser checks

Install the browser test dependency once, then run the fixture:

```sh
cd ui/tests
npm ci
npx playwright install chromium
npm test
```

Alternatively, set `PLAYWRIGHT_MODULE` to the absolute path of an existing
Playwright module and `CHROMIUM_EXECUTABLE` to a compatible browser, then run
`node ui/tests/fixture.mjs` from the repository root. `UI_ARTIFACTS` changes the
screenshot directory (the default is `/tmp/acp-slack-adapter-ui`). These are test
dependencies only; the application has no browser package dependency.

The fixture uses a local server with a URL prefix. It exercises setup, credential
handling, actual Socket Mode connection status, independent conversation contexts,
meaningful activity, scoped access tokens, view commands and acknowledgements,
embedding, and the mobile layout. It never connects to Slack or a real ACP agent.
It also verifies that lost cookies and failures loading configuration or a display
produce visible errors and can be retried.

For the full password-to-browser flow on Linux, build the binary and run:

```sh
cargo build --locked
python3 tests/pam_live.py --browser
```

This opt-in check requires `sudo -n`, Node, and the browser dependencies above. It
creates and removes a disposable OS account and HTTPS service. The browser signs
in through real PAM with another app's cookie already present on the same host,
then checks reload, protected view changes, logout and a second login. Passwords
are passed over stdin, never command arguments, environment or reports. The
browser accepts only the test service's temporary certificate for this check;
separate Python HTTPS requests validate that certificate against the test CA.
CI runs this alongside the Rust auth tests, without injecting bearer credentials.

Every ordinary control maps to a management API operation or a semantic view
command. An authenticated page also exposes `window.slackConnector` with
`snapshot()`, `actions()`, `act(name, details)`, and `command(commandOrArray)`.
The `credentials(values)` helper calls the write-only credential API directly;
credentials must never be sent through a view command. Password sign-in remains
the normal authentication endpoint. `snapshot()` includes no secrets.

Use `?embed=1` to hide the standalone navigation chrome. Hosting permissions and
authentication remain the server's responsibility; the query parameter grants
no access. Assets and API requests resolve relative to the current directory,
so an authenticated parent can proxy the interface under a path of its own.

