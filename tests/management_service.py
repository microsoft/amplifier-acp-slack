#!/usr/bin/env python3
"""Exercise the real local service, TLS, CLI and management APIs without Slack/model work."""
import argparse
import getpass
import importlib.util
import json
import pathlib
import socket
import ssl
import subprocess
import tempfile
import time
import uuid
import urllib.error
import urllib.parse
import urllib.request

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=pathlib.Path, default=pathlib.Path(__file__).resolve().parents[1] / "target/debug/acp-slack-adapter")
args = parser.parse_args()
binary = str(args.binary.resolve())
checks = 0
# The source installer's own argument builder, so its flags and `init` stay in step.
spec = importlib.util.spec_from_file_location("installer", pathlib.Path(__file__).resolve().parents[1] / "scripts/install.py")
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)

def check(condition, label):
    global checks
    assert condition, label
    checks += 1

with tempfile.TemporaryDirectory(prefix="adapter-management-") as root:
    home = pathlib.Path(root) / "instance"
    def cli(*cmd):
        completed = subprocess.run([binary, "--home", str(home), *cmd], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        return json.loads(completed.stdout)
    created = cli("init", "--label", "Management verification", "--owner", getpass.getuser(), *installer.workspace_arguments("T0EXAMPLE1", "Example Workspace"))
    process = subprocess.Popen([binary, "--home", str(home), "serve"], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        deadline = time.monotonic() + 20
        while not (home / "endpoints.json").exists():
            assert process.poll() is None, "Service exited before readiness"
            assert time.monotonic() < deadline, "Service startup timed out"
            time.sleep(0.05)
        endpoints = json.loads((home / "endpoints.json").read_text())
        base = endpoints["base_url"]
        context = ssl.create_default_context(cafile=endpoints["trust_certificate_file"])
        # Python 3.13 enables this by default; exercise the same chain rules on
        # older supported test interpreters too, without relaxing hostname/trust.
        context.verify_flags |= ssl.VERIFY_X509_STRICT
        token = (home / "secrets/control-token").read_text().strip()
        def api(method, path, body=None, credential=token):
            data = None if body is None else json.dumps(body).encode()
            headers = {"Content-Type": "application/json"}
            if credential:
                headers["Authorization"] = f"Bearer {credential}"
            request = urllib.request.Request(base + path, data=data, method=method, headers=headers)
            try:
                with urllib.request.urlopen(request, context=context, timeout=5) as r:
                    return r.status, json.load(r), r.headers
            except urllib.error.HTTPError as e:
                return e.code, json.load(e), e.headers
        while True:
            try:
                result = api("GET", "/healthz", credential=None)
                break
            except (OSError, urllib.error.URLError) as error:
                if isinstance(error, ssl.SSLCertVerificationError) or isinstance(
                        getattr(error, "reason", None), ssl.SSLCertVerificationError):
                    raise
                if process.poll() is not None:
                    raise AssertionError("Service exited before TLS readiness")
                assert time.monotonic() < deadline, "TLS listener startup timed out"
                time.sleep(0.05)
        check(result[1]["ok"], "Trusted local TLS health")
        parsed = urllib.parse.urlparse(base)
        def refused_tls(trust, hostname, verify_code, label):
            with socket.create_connection((parsed.hostname, parsed.port), timeout=3) as connection:
                try:
                    with trust.wrap_socket(connection, server_hostname=hostname):
                        raise AssertionError(label)
                except ssl.SSLCertVerificationError as error:
                    check(error.verify_code == verify_code, label + ": " + error.verify_message)
        untrusted = ssl.create_default_context()
        untrusted.verify_flags |= ssl.VERIFY_X509_STRICT
        refused_tls(untrusted, parsed.hostname, 20, "Untrusted local CA refused")
        refused_tls(context, "wrong-host.invalid", 62, "Trusted CA does not bypass hostname verification")
        with socket.create_connection((parsed.hostname, parsed.port), timeout=3) as connection:
            connection.sendall(b"GET /api/v1/config HTTP/1.0\r\nHost: localhost\r\n\r\n")
            try:
                data = connection.recv(4096)
            except (ConnectionResetError, socket.timeout):
                data = b""
            check(b"200 OK" not in data and b"label" not in data, "No management over plaintext")
        check(api("GET", "/api/v1/config", credential=None)[0] == 401, "Unauthenticated config refused")
        check(api("GET", "/api/v1/auth/session", credential=None)[1]["authenticated"] is False, "Sign-in state available")
        status = cli("status")
        check(status["label"] == "Management verification" and status["enabled"] is False, "CLI talks to real TLS API")
        # The api command adds /api/v1 itself. Repeating it reaches no route; say so, with the status.
        wrong = subprocess.run([binary, "--home", str(home), "api", "GET", "api/v1/status"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        check(wrong.returncode != 0 and "HTTP 404" in wrong.stderr and "relative to /api/v1" in wrong.stderr and 'try "status"' in wrong.stderr and "EOF" not in wrong.stderr, "A wrong API path reports its HTTP status and the relative form")
        check(cli("api", "GET", "status")["label"] == "Management verification", "The relative API path answers")
        code, config, _ = api("GET", "/api/v1/config")
        config["server"]["embed_origins"] = ["https://fleet.example"]
        saved = api("PUT", "/api/v1/config", {"expected_revision": config["revision"], "config": config})
        check(saved[0] == 200, "Revision-aware configuration update")
        check(api("PUT", "/api/v1/config", {"expected_revision": config["revision"], "config": config})[0] == 409, "Stale save refused")
        check("https://fleet.example" in api("GET", "/healthz", credential=None)[2]["Content-Security-Policy"], "Explicit iframe origin applied")
        issued = api("POST", "/api/v1/tokens", {"name": "observer", "scopes": ["observe"], "expires_in_seconds": 300})[1]
        check(api("GET", "/api/v1/status", credential=issued["token"])[0] == 200, "Observer reads status")
        check(api("POST", "/api/v1/lifecycle", {"action": "start"}, issued["token"])[0] == 403, "Observer cannot start adapter")
        secretfile = pathlib.Path(root) / "secret"
        secretfile.write_text("private-service-canary")
        check(cli("secret", "provider-key", "--file", str(secretfile))["stored"], "CLI write-only secret")
        check("private-service-canary" not in json.dumps(api("GET", "/api/v1/secrets")[1]), "Secret names do not return values")
        check("private-service-canary" not in json.dumps(api("GET", "/api/v1/activity")[1]), "Activity excludes secret values")
        check(api("POST", "/api/v1/setup/complete", {})[0] == 400, "Incomplete connection cannot start")
        check(api("GET", "/api/v1/setup/slack", credential=None)[0] == 401, "Slack setup requires sign-in")
        check(api("POST", "/api/v1/setup/slack", {"action":"authorize"}, issued["token"])[0] == 403, "Observer cannot authorize Slack")
        check(api("GET", "/api/v1/setup/slack")[1]["authorized"] is False, "Fresh setup has no authorization")
        setup = api("GET", "/api/v1/setup/slack")[1]
        check(setup["authorization"] == "missing" and setup["next_action"] == "authorize" and not setup["connection_ready"], "Fresh setup exposes one next action")
        check(api("POST", "/api/v1/setup/slack", {"action":"token"})[0] == 400, "Internal token operation is never exposed")
        refusal_config = api("GET", "/api/v1/config")[1]
        config_before_refusal = (home / "adapter.json").read_bytes()
        expired_code, expired, _ = api("POST", "/api/v1/setup/slack", {"action":"confirm","challenge":"code"})
        check(expired_code == 200 and expired.get("confirmation_refusal") == {
            "schema": 1, "action": "confirm", "code": "authorization_expired",
            "id": refusal_config["id"], "config_revision": refusal_config["revision"],
        }, "Expired authorization returns the exact installation-bound typed refusal")
        check(expired["authorization"] == "missing" and expired["next_action"] == "authorize"
              and all(expired[key] is False for key in ("authorized", "authorization_pending", "connection_ready", "connection_running", "setup_complete"))
              and "authorization_command" not in expired,
              "Expired confirmation preserves saved unconnected status without a new authorization")
        check((home / "adapter.json").read_bytes() == config_before_refusal,
              "Expired confirmation leaves exact configuration unchanged")
        # A workspace declared at installation is shown everywhere, but it is not a connection.
        declared = {"team_id": "T0EXAMPLE1", "name": "Example Workspace"}
        check(created["intended_workspace"] == declared and status["slack_workspace"] == declared and status["setup_complete"] is False, "Installer arguments declare the workspace through init and status")
        check(setup["intended_workspace"] == declared and setup["authorized_workspace"] is None and setup["workspace_mismatch"] is False, "Setup reports the intended workspace apart from any approval")
        config = api("GET", "/api/v1/config")[1]
        config["slack"].update(team_id="example-workspace", workspace_name="Other Workspace")
        check(api("PUT", "/api/v1/config", {"expected_revision": config["revision"], "config": config})[0] == 400, "A workspace name or address is refused as its ID")
        config["slack"]["team_id"] = "T0OTHER0001"
        check(api("PUT", "/api/v1/config", {"expected_revision": config["revision"] - 1, "config": config})[0] == 409, "Declaring a workspace is revision-checked")
        check(api("PUT", "/api/v1/config", {"expected_revision": config["revision"], "config": config})[0] == 200, "The workspace can be declared later through the config API")
        setup = api("GET", "/api/v1/setup/slack")[1]
        check(setup["intended_workspace"] == {"team_id": "T0OTHER0001", "name": "Other Workspace"} and setup["next_action"] == "authorize" and not setup["connection_ready"], "A declared workspace does not look connected")
        check(api("POST", "/api/v1/lifecycle", {"action": "start"})[0] == 400, "A declared workspace cannot start without its Slack app")
        refused = api("POST", "/api/v1/setup/slack", {"action": "connect", "team_id": "T0EXAMPLE1"})
        check(refused[0] == 400 and "Other Workspace (T0OTHER0001)" in refused[1]["error"], "Connect creates nothing in a workspace other than the declared one")
        check(api("POST", "/api/v1/setup/slack", {"action": "discard"}, issued["token"])[0] == 403, "Observer cannot discard a Slack approval")
        discarded = api("POST", "/api/v1/setup/slack", {"action": "discard"})
        check(discarded[0] == 200 and discarded[1]["authorized"] is False and discarded[1]["next_action"] == "authorize", "Discarding when nothing was approved changes nothing")
        with urllib.request.urlopen(base + "/", context=context) as response:
            check(b"<title>Management verification: Slack Connector</title>" in response.read(), "Pre-login tab identifies connection")

        api_schema = api("GET", "/api/v1/openapi.json")[1]
        check(api_schema["paths"]["/api/v1/views/{id}/commands"]["post"]["requestBody"]["required"], "Programmatic UI contract discoverable")
        entry = cli("connection-enrollment")
        command = dict(schema=1, connection_id=entry["id"], revision=entry["revision"], action="disconnect", request_id=str(uuid.uuid4()))
        check(api("POST", "/api/v1/saved-connection", command, issued["token"])[0] == 403, "Observer cannot change a saved connection")
        def connection_call(command):
            p = subprocess.run([binary,"--home",str(home),"connection",command["action"]],input=json.dumps(command),capture_output=True,text=True,check=True)
            return json.loads(p.stdout)
        check(connection_call(command)["context_quiesced"], "Actual fixed CLI drains before context shutdown")
        command["action"]="status"
        terminal=connection_call(command)
        check(terminal["settled"] and terminal["state"]=="disconnected", "Actual fixed CLI confirms paused state")
        check((home/"connection-gate.json").exists(), "Disconnect remains paused durably")
        check((home/"secrets/provider-key").read_text()=="private-service-canary", "Saved credentials retained")
        root_cert = (home / "tls/root-ca.crt").read_bytes()
        tls_before_restart = {name: (home / "tls" / name).read_bytes()
                              for name in ("root-ca.crt", "root-ca.key", "cert.pem", "key.pem", "managed.json")}
        port = parsed.port
        process.terminate()
        process.wait(timeout=10)
        check(process.returncode == 0, "Graceful management shutdown")
        process = subprocess.Popen([binary, "--home", str(home), "serve"], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        deadline = time.monotonic() + 20
        while True:
            try:
                api("GET", "/healthz", credential=None)
                break
            except (OSError, urllib.error.URLError):
                assert time.monotonic() < deadline
                time.sleep(0.05)
        check(connection_call(command)==terminal, "Original operation reconciles after real service restart")
        connect=dict(command,action="connect",request_id=str(uuid.uuid4()))
        check(connection_call(connect)["context_ready"], "Connect exposes context-ready before activation")
        connect["action"]="status"
        failure=connection_call(connect)
        check(failure["settled"] and failure["failed"] and failure["state"]=="disconnected", "Missing authorization is a settled failure, never fake success")
        check((home / "tls/root-ca.crt").read_bytes() == root_cert, "CA stable across restart")
        check(all((home / "tls" / name).read_bytes() == data for name, data in tls_before_restart.items()),
              "Ordinary restart preserves existing managed certificates and pins")
        check(json.loads((home / "endpoints.json").read_text())["base_url"].endswith(f":{port}"), "Port stable across restart")
        # Explicit operator renewal: stop, retain the old leaf, then remove it
        # from its serving path. Never remove the CA or managed.json.
        process.terminate()
        process.wait(timeout=10)
        (home / "tls/cert.pem").rename(home / "tls/previous-cert.pem")
        process = subprocess.Popen([binary, "--home", str(home), "serve"], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        deadline = time.monotonic() + 20
        while True:
            try:
                api("GET", "/healthz", credential=None)
                break
            except (OSError, urllib.error.URLError):
                assert time.monotonic() < deadline, "Renewed TLS listener startup timed out"
                time.sleep(0.05)
        check(all((home / "tls" / name).read_bytes() == tls_before_restart[name]
                  for name in ("root-ca.crt", "root-ca.key")), "Explicit leaf renewal preserves CA trust and key")
        check(all((home / "tls" / name).read_bytes() != tls_before_restart[name]
                  for name in ("cert.pem", "key.pem")), "Explicit renewal replaces only the leaf certificate and key")
        check(api("GET", "/api/v1/status")[0] == 200, "Existing management credential works after strict TLS renewal")
        refused_tls(context, "wrong-host.invalid", 62, "Renewed leaf still refuses the wrong hostname")
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=10)
print(f"{checks} real management-service checks passed (no Slack messages or model calls).")
