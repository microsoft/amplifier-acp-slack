#!/usr/bin/env python3
"""Opt-in Linux PAM check with a disposable OS user and loopback-only instance.

Requires sudo -n for useradd/chpasswd/runuser/chage/userdel and test cleanup.
Never supplies passwords in command arguments, environment, reports or logs.
It does not create a systemd service or configure a backend.
"""
import argparse
import datetime
import grp
import json
import os
from pathlib import Path
import pwd
import secrets
import shutil
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


ROOT = Path(__file__).resolve().parent.parent


def privileged(*args, input_text=None, check=True):
    return subprocess.run(["sudo", "-n", "--", *map(str, args)], input=input_text, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=check)


def check(condition, description, results):
    if not condition:
        raise RuntimeError(description)
    results.append(description)


def read_owned(path):
    result = privileged("cat", "--", path)
    return result.stdout


def request(base, context, path, *, method="GET", data=None, cookie=None, csrf=None, origin=None):
    headers = {"Accept": "application/json"}
    if cookie:
        headers["Cookie"] = cookie
    if csrf:
        headers["x-csrf-token"] = csrf
    if origin:
        headers["Origin"] = origin
    payload = None
    if data is not None:
        payload = json.dumps(data).encode()
        headers["Content-Type"] = "application/json"
    call = urllib.request.Request(base + path, data=payload, headers=headers, method=method)
    try:
        response = urllib.request.urlopen(call, context=context, timeout=25)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        body = response.read(1024 * 1024)
        try:
            value = json.loads(body)
        except (ValueError, UnicodeDecodeError):
            value = None
        return response.status, response.headers, value


def ensure_pam_profile():
    expected = ROOT / "ops/pam/acp-slack-adapter"
    installed = Path("/etc/pam.d/acp-slack-adapter")
    if installed.exists():
        if privileged("cmp", "-s", "--", expected, installed, check=False).returncode:
            raise RuntimeError("Existing PAM profile differs; it was preserved and this test stopped")
        return "existing matching profile"
    if not Path("/etc/pam.d/common-auth").is_file() or not Path("/etc/pam.d/common-account").is_file():
        raise RuntimeError("This test requires the Debian/Ubuntu common-auth and common-account policies")
    privileged("install", "-m", "0644", expected, installed)
    return "installed matching profile"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/acp-slack-adapter")
    parser.add_argument("--report", type=Path)
    parser.add_argument("--browser", action="store_true", help="Also test real browser login (Node and Playwright required)")
    args = parser.parse_args()
    if sys.platform != "linux" or os.geteuid() == 0:
        parser.error("Run as an ordinary Linux user with sudo -n, not as root")
    binary = args.binary.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("Build acp-slack-adapter first or supply --binary")
    privileged("true")
    for executable in ["useradd", "chpasswd", "runuser", "chage", "userdel", "groupdel", "pkill"]:
        if not shutil.which(executable, path="/usr/sbin:/usr/bin:/sbin:/bin"):
            parser.error("Missing host test prerequisite: " + executable)

    pam_profile = ensure_pam_profile()
    temporary = Path(tempfile.mkdtemp(prefix="acp-slack-adapter-pam-"))
    temporary.chmod(0o711)  # only traversal for the disposable account
    username = "asa-pam-" + secrets.token_hex(5)
    password = secrets.token_urlsafe(36)
    wrong_password = secrets.token_urlsafe(36)
    user_created = False
    process = None
    log_stream = None
    uid = None
    gid = None
    results = []
    failure = None
    cleanup_errors = []
    started = time.monotonic()
    try:
        account_home = temporary / "user"
        privileged("useradd", "--create-home", "--home-dir", account_home, "--shell", "/bin/sh", "--user-group", username)
        user_created = True
        account = pwd.getpwnam(username)
        uid, gid = account.pw_uid, account.pw_gid
        privileged("chmod", "0700", account_home)
        privileged("chpasswd", input_text=username + ":" + password + "\n")
        copied_binary = temporary / "acp-slack-adapter"
        shutil.copyfile(binary, copied_binary)
        copied_binary.chmod(0o755)
        instance = account_home / "instance"
        privileged("runuser", "-u", username, "--", copied_binary, "init", "--home", instance,
                   "--label", "PAM integration fixture", "--owner", username, "--bind", "127.0.0.1", "--port", "0")
        context = None
        log_path = temporary / "service.log"
        log_stream = log_path.open("xb")
        log_path.chmod(0o600)
        process = subprocess.Popen(["sudo", "-n", "--", "runuser", "-u", username, "--", str(copied_binary),
                                    "serve", "--home", str(instance)], stdin=subprocess.DEVNULL,
                                   stdout=log_stream, stderr=log_stream, start_new_session=True)
        deadline = time.monotonic() + 25
        base = None
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError("Disposable service exited before becoming ready")
            try:
                endpoints = json.loads(read_owned(instance / "endpoints.json"))
                base = endpoints["base_url"]
                if context is None:
                    context = ssl.create_default_context(cadata=read_owned(instance / "tls/root-ca.crt"))
                if not base.startswith("https://127.0.0.1:"):
                    raise RuntimeError("Disposable service did not remain loopback-only")
                status, _, _ = request(base, context, "/api/v1/status")
                if status == 401:
                    break
            except (subprocess.CalledProcessError, OSError, ValueError, urllib.error.URLError):
                pass
            time.sleep(0.2)
        else:
            raise RuntimeError("Disposable service did not become ready before the deadline")
        check(base is not None, "Loopback HTTPS works with the instance's actual certificate", results)

        status, _, _ = request(base, context, "/api/v1/auth/login", method="POST", origin=base,
                               data={"username": username, "password": wrong_password})
        check(status == 401, "Wrong password is rejected by real PAM", results)

        status, headers, value = request(base, context, "/api/v1/auth/login", method="POST", origin=base,
                                         data={"username": username, "password": password})
        check(status == 200 and value and value.get("owner") == username, "Valid owner password passes PAM authentication and account checks", results)
        cookie_header = headers.get("Set-Cookie", "")
        check(all(flag in cookie_header.lower() for flag in ["secure", "httponly", "samesite=strict"]),
              "Login issues a Secure, HttpOnly, SameSite=Strict session cookie", results)
        cookie = cookie_header.split(";", 1)[0]
        csrf = value.get("csrf")
        check(isinstance(csrf, str) and len(csrf) >= 32, "Login provides a separate CSRF token", results)

        status, _, document = request(base, context, "/api/v1/config", cookie=cookie)
        check(status == 200 and document.get("server", {}).get("owner") == username,
              "PAM browser session can inspect its installation", results)
        document["label"] = "PAM integration edited"
        change = {"expected_revision": document["revision"], "config": document}
        status, _, _ = request(base, context, "/api/v1/config", method="PUT", data=change, cookie=cookie, origin=base)
        check(status == 403, "Cookie-authenticated mutation without CSRF is rejected", results)
        status, _, _ = request(base, context, "/api/v1/config", method="PUT", data=change, cookie=cookie,
                               csrf=csrf, origin="https://wrong-origin.invalid")
        check(status == 403, "Cookie-authenticated mutation from another origin is rejected", results)
        status, _, changed = request(base, context, "/api/v1/config", method="PUT", data=change, cookie=cookie, csrf=csrf, origin=base)
        check(status == 200 and changed.get("label") == "PAM integration edited",
              "Same-origin mutation with valid CSRF succeeds", results)

        status, logout_headers, _ = request(base, context, "/api/v1/auth/logout", method="POST", data={}, cookie=cookie, csrf=csrf, origin=base)
        check(status == 200 and "max-age=0" in logout_headers.get("Set-Cookie", "").lower(), "Logout expires the browser cookie", results)
        status, _, logged_out = request(base, context, "/api/v1/auth/session", cookie=cookie)
        check(status == 200 and logged_out.get("authenticated") is False, "Logged-out session cannot be reused", results)

        if args.browser:
            browser = subprocess.run(
                ["node", str(ROOT / "ui/tests/pam-service.mjs")],
                input=json.dumps({"base": base, "username": username, "password": password}),
                text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=90,
            )
            try:
                browser_report = json.loads(browser.stdout)
            except ValueError:
                raise RuntimeError("Browser test could not start; check Node, Playwright and Chromium installation") from None
            results.extend(browser_report.get("checks", []))
            if browser.returncode or browser_report.get("failure"):
                raise RuntimeError(browser_report.get("failure", "Browser sign-in check failed")
                                   + "; requests=" + json.dumps(browser_report.get("requests", [])))

        privileged("chage", "--expiredate", "0", username)
        status, _, _ = request(base, context, "/api/v1/auth/login", method="POST", origin=base,
                               data={"username": username, "password": password})
        check(status == 401, "Correct password with an expired account is rejected", results)

        log_stream.flush()
        captured = log_path.read_text(errors="replace")
        check(password not in captured and wrong_password not in captured and csrf not in captured and cookie not in captured,
              "Service output contains no test password, session cookie or CSRF token", results)
    except Exception as error:
        # Do not include request data, environment, service logs or authentication values.
        if isinstance(error, subprocess.CalledProcessError):
            failure = "Host command failed with exit code " + str(error.returncode)
        else:
            failure = type(error).__name__ + ": " + str(error)
        for secret in [password, wrong_password]:
            failure = failure.replace(secret, "[redacted]")
    finally:
        if uid is not None:
            result = privileged("pkill", "-TERM", "-u", str(uid), check=False)
            if result.returncode not in (0, 1):
                cleanup_errors.append("Could not terminate disposable user's processes")
        if process is not None:
            try:
                process.wait(timeout=12)
            except subprocess.TimeoutExpired:
                if uid is not None:
                    privileged("pkill", "-KILL", "-u", str(uid), check=False)
                privileged("kill", "-TERM", "--", str(process.pid), check=False)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    cleanup_errors.append("Disposable process supervisor did not exit")
        if log_stream is not None:
            log_stream.close()
        if user_created:
            result = privileged("userdel", "--remove", username, check=False)
            if result.returncode:
                cleanup_errors.append("Could not remove disposable OS account/home")
            try:
                pwd.getpwnam(username)
            except KeyError:
                pass
            else:
                cleanup_errors.append("Disposable OS account remains")
            try:
                group = grp.getgrnam(username)
            except KeyError:
                pass
            else:
                if group.gr_gid == gid and not group.gr_mem:
                    if privileged("groupdel", username, check=False).returncode:
                        cleanup_errors.append("Could not remove disposable OS group")
                else:
                    cleanup_errors.append("Disposable OS group changed unexpectedly and was preserved")
        try:
            shutil.rmtree(temporary)
        except OSError:
            cleanup_errors.append("Disposable test directory remains at " + str(temporary))

    report = {
        "checked_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform": "Linux, actual local PAM, loopback HTTPS",
        "pam_profile": pam_profile,
        "checks": results,
        "passed": len(results),
        "elapsed_seconds": round(time.monotonic() - started, 2),
        "failure": failure,
        "cleanup_errors": cleanup_errors,
        "scope": "Disposable user and unconfigured process only; no existing users or agent services changed",
    }
    encoded = json.dumps(report, indent=2) + "\n"
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        descriptor = os.open(args.report, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "w") as stream:
            stream.write(encoded)
    print(encoded, end="")
    return 1 if failure or cleanup_errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
