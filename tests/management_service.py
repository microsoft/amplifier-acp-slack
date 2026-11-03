#!/usr/bin/env python3
"""Exercise the real local service, TLS, CLI and management APIs without Slack/model work."""
import argparse
import getpass
import json
import pathlib
import socket
import ssl
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=pathlib.Path, default=pathlib.Path(__file__).resolve().parents[1] / "target/debug/acp-slack-adapter")
args = parser.parse_args()
binary = str(args.binary.resolve())
checks = 0

def check(condition, label):
    global checks
    assert condition, label
    checks += 1

with tempfile.TemporaryDirectory(prefix="adapter-management-") as root:
    home = pathlib.Path(root) / "instance"
    def cli(*cmd):
        completed = subprocess.run([binary, "--home", str(home), *cmd], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        return json.loads(completed.stdout)
    cli("init", "--label", "Management verification", "--owner", getpass.getuser())
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
            except (OSError, urllib.error.URLError):
                assert time.monotonic() < deadline, "TLS listener startup timed out"
                time.sleep(0.05)
        check(result[1]["ok"], "Trusted local TLS health")
        try:
            urllib.request.urlopen(base + "/healthz", timeout=3)
            raise AssertionError("Fresh local CA was unexpectedly trusted by the system")
        except urllib.error.URLError:
            checks += 1
        parsed = urllib.parse.urlparse(base)
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
        check(api("POST", "/api/v1/setup/slack", {"action":"confirm","challenge":"code"})[0] == 400, "Expired authorization gives actionable error")
        with urllib.request.urlopen(base + "/", context=context) as response:
            check(b"<title>Management verification: Slack Connector</title>" in response.read(), "Pre-login tab identifies connection")

        api_schema = api("GET", "/api/v1/openapi.json")[1]
        check(api_schema["paths"]["/api/v1/views/{id}/commands"]["post"]["requestBody"]["required"], "Programmatic UI contract discoverable")
        root_cert = (home / "tls/root-ca.crt").read_bytes()
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
        check((home / "tls/root-ca.crt").read_bytes() == root_cert, "CA stable across restart")
        check(json.loads((home / "endpoints.json").read_text())["base_url"].endswith(f":{port}"), "Port stable across restart")
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=10)
print(f"{checks} real management-service checks passed (no Slack messages or model calls).")
