#!/usr/bin/env python3
"""Private, per-installation Slack CLI bridge. Never print CLI output or tokens.

The service invokes this helper with JSON on stdin. Only the internal `token`
operation returns an access token, directly to the service's private pipe.
"""
import fcntl
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import tarfile
import time
import urllib.request

VERSION = '4.12.0'
ARCHIVES = {
    ('Linux', 'x86_64'): ('linux_amd64', '8b93b0ff3a30879b5ef09fe01fb3bc457afb368bb26827b526f101eccce96c76'),
    ('Linux', 'aarch64'): ('linux_arm64', 'd98ad8ba0848958a47befb73761a51ebd97a363b431758e177a828c7774bdb7d'),
    ('Darwin', 'x86_64'): ('macOS_amd64', '838aa81d10f82e5e9212800d084924ac66e5807aecaac16ae747c90d985352a2'),
    ('Darwin', 'arm64'): ('macOS_arm64', '23a5001854d2bb283ded8ad941cff13d4b2573ba922233087609b70ce53e4b6d'),
}


class SetupError(Exception):
    pass


def read(path, default=None):
    return json.loads(path.read_text()) if path.exists() else default


def write(path, value):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    temporary = path.with_suffix('.tmp')
    with open(temporary, 'w', opener=lambda p, f: os.open(p, f, 0o600)) as stream:
        json.dump(value, stream)
    temporary.replace(path)


def ensure_cli(folder):
    binary = folder / 'bin/slack'
    receipt = read(folder / 'cli.json', {})
    if binary.exists() and receipt.get('version') == VERSION:
        if hashlib.sha256(binary.read_bytes()).hexdigest() == receipt.get('sha256'):
            return binary
    target = ARCHIVES.get((platform.system(), platform.machine()))
    if not target:
        raise SetupError('Automatic Slack setup supports Linux and macOS. Manual credentials remain available in Advanced setup.')
    suffix, checksum = target
    url = f'https://github.com/slackapi/slack-cli/releases/download/v{VERSION}/slack_cli_{VERSION}_{suffix}.tar.gz'
    with urllib.request.urlopen(url, timeout=45) as response:
        archive = response.read(100 * 1024 * 1024 + 1)
    if len(archive) > 100 * 1024 * 1024 or hashlib.sha256(archive).hexdigest() != checksum:
        raise SetupError('The Slack setup tool could not be verified. Retry the download.')
    with tarfile.open(fileobj=io.BytesIO(archive), mode='r:gz') as bundle:
        member = bundle.getmember('bin/slack')
        if not member.isfile() or member.size > 150 * 1024 * 1024:
            raise SetupError('The Slack setup download is invalid.')
        binary.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        temporary = binary.with_suffix('.new')
        temporary.write_bytes(bundle.extractfile(member).read())
        temporary.chmod(0o700)
        temporary.replace(binary)
    write(folder / 'cli.json', {'version': VERSION, 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest()})
    return binary


def cli(folder, *args, cwd=None, auth_dir=None):
    binary = ensure_cli(folder)
    config = auth_dir or folder / 'auth'
    config.mkdir(mode=0o700, exist_ok=True)
    env = dict(os.environ)
    for name in list(env):
        if name.startswith('SLACK_'):
            del env[name]
    env.update({'SLACK_SKIP_UPDATE': 'true', 'NO_COLOR': '1'})
    # Output stays in this private process; errors below use an allowlist.
    result = subprocess.run([str(binary), '--config-dir', str(config), '--no-color',
                             '--skip-update', *args], cwd=cwd or folder, env=env,
                            input='', text=True, capture_output=True, timeout=120)
    output = result.stdout + result.stderr
    if result.returncode:
        for code, explanation in [
            ('app_approval_request_pending', 'The app is awaiting Slack administrator approval. After approval, choose Connect again.'),
            ('app_approval_request_eligible', 'Slack requires administrator approval for this app. Ask your workspace administrator to approve it, then retry.'),
            ('app_approval_request_denied', 'The workspace administrator declined this app. Its connection has not started.'),
            ('service_limits_exceeded', 'This Slack workspace has reached its app limit. Make room for another app, then retry.'),
            ('invalid_auth', 'Slack authorization is no longer valid. Reconnect your Slack account.'),
            ('token_revoked', 'Slack authorization was revoked. Reconnect your Slack account.'),
            ('token_expired', 'Slack authorization expired. Reconnect your Slack account.'),
            ('invalid_challenge', 'That confirmation code was not accepted. Check it or start authorization again.'),
            ('missing_scope', 'This Slack authorization cannot complete app setup. Reconnect your Slack account using the guided flow.'),
        ]:
            if code in output:
                raise SetupError(explanation)
        raise SetupError('Slack setup could not complete. Retry, or reconnect your Slack account. Your existing connection and any app already created are preserved.')
    return output


def authorization(folder, auth_dir=None):
    credentials = read((auth_dir or folder / 'auth') / 'credentials.json', {})
    entries = [v for v in credentials.values() if isinstance(v, dict) and v.get('token')]
    if len(entries) != 1:
        raise SetupError('Connect a Slack account for this installation first.')
    return entries[0]


def summary(folder):
    try:
        auth = authorization(folder)
        ready = {'authorized': True, 'team_id': auth['team_id'], 'workspace': auth.get('team_domain', auth['team_id'])}
    except SetupError:
        ready = {'authorized': False}
    pending = read(folder / 'pending.json', {})
    ready['authorization_pending'] = pending.get('expires_at', 0) > time.time()
    return ready


def prepare_project(folder, manifest, app_id, auth):
    project = folder / 'project'
    (project / '.slack').mkdir(mode=0o700, parents=True, exist_ok=True)
    write(project / 'manifest.json', manifest)
    write(project / '.slack/hooks.json', {'hooks': {'get-hooks': 'python3 hooks.py hooks'}})
    write(project / '.slack/config.json', {'manifest': {'source': 'local'}})
    (project / 'hooks.py').write_text(HOOK)
    apps_path = project / '.slack/apps.json'
    apps = read(apps_path, {'apps': {}})
    existing = apps.get('apps', {}).get(auth['team_id'], {}).get('app_id')
    if app_id and existing and app_id != existing:
        raise SetupError('This installation is already associated with a different Slack app.')
    if app_id and not existing:
        apps.setdefault('apps', {})[auth['team_id']] = {'app_id': app_id, 'team_id': auth['team_id'], 'team_domain': auth.get('team_domain', '')}
        write(apps_path, apps)
    return project


HOOK = '''import json, os, pathlib, sys
p = pathlib.Path(__file__).resolve().parent
if sys.argv[1] == "hooks":
    print(json.dumps({"runtime":"rust", "hooks":{"get-manifest":"python3 hooks.py manifest", "deploy":"python3 hooks.py install"}}))
elif sys.argv[1] == "manifest":
    print((p/"manifest.json").read_text())
elif sys.argv[1] == "install":
    value = {"app_token":os.environ["SLACK_APP_TOKEN"], "bot_token":os.environ["SLACK_BOT_TOKEN"]}
    dest=p.parent/"issued.json"
    with open(dest,"w",opener=lambda n,f:os.open(n,f,0o600)) as stream: json.dump(value,stream)
    print("Connection credentials received privately.")
'''


def execute(home, action, body):
    folder = home / 'secrets/slack-setup'
    folder.mkdir(mode=0o700, parents=True, exist_ok=True)
    if action == 'status':
        return summary(folder)
    with open(folder / 'owner.lock', 'a') as guard:
        try:
            fcntl.flock(guard, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SetupError('A Slack setup operation is already running. Try again shortly.')
        if action == 'authorize':
            output = cli(folder, 'auth', 'login', '--no-prompt')
            match = re.search(r'/slackauthticket ([A-Za-z0-9_+/=.-]+)', output)
            if not match:
                raise SetupError('Slack did not return an authorization command. Retry.')
            pending = {'ticket': match[1], 'expires_at': time.time() + 600}
            write(folder / 'pending.json', pending)
            return {'command': '/slackauthticket ' + pending['ticket'], 'expires_at': pending['expires_at']}
        if action == 'confirm':
            pending = read(folder / 'pending.json', {})
            challenge = body.get('challenge', '').strip()
            if pending.get('expires_at', 0) <= time.time():
                raise SetupError('That authorization attempt expired. Start authorization again.')
            if not re.fullmatch(r'[A-Za-z0-9_-]{4,256}', challenge):
                raise SetupError('Enter the confirmation code Slack showed after you approved access.')
            staging = folder / 'pending-auth'
            staging.mkdir(mode=0o700, exist_ok=True)
            (staging / 'credentials.json').unlink(missing_ok=True)
            cli(folder, 'auth', 'login', '--ticket', pending['ticket'], '--challenge', challenge, auth_dir=staging)
            auth = authorization(folder, staging)
            if body.get('team_id') and auth['team_id'] != body['team_id']:
                (staging / 'credentials.json').unlink(missing_ok=True)
                raise SetupError('Authorize the workspace this connection already belongs to. Its previous access is preserved.')
            write(folder / 'auth/credentials.json', {auth['team_id']: auth})
            (staging / 'credentials.json').unlink(missing_ok=True)
            (folder / 'pending.json').unlink(missing_ok=True)
            return summary(folder)
        if action == 'token':
            cli(folder, 'auth', 'list')  # CLI owns rotation; never duplicate its refresh token.
            auth = authorization(folder)
            if auth['team_id'] != body.get('team_id'):
                raise SetupError('Slack authorization belongs to another workspace.')
            return {'token': auth['token']}
        if action == 'connect':
            cli(folder, 'auth', 'list')
            auth = authorization(folder)
            if body.get('team_id') and body['team_id'] != auth['team_id']:
                raise SetupError('This connection belongs to another workspace. Authorize its original workspace.')
            project = prepare_project(folder, body['manifest'], body.get('app_id'), auth)
            (folder / 'issued.json').unlink(missing_ok=True)
            cli(folder, 'deploy', '--team', auth['team_id'], '--app', 'deployed', '--manifest-source', 'local', cwd=project)
            tokens = read(folder / 'issued.json', {})
            if not tokens.get('app_token', '').startswith('xapp-') or not tokens.get('bot_token', '').startswith('xoxb-'):
                raise SetupError('Slack did not supply both connection credentials. Retry or use Advanced setup.')
            app_id = read(project / '.slack/apps.json', {})['apps'][auth['team_id']]['app_id']
            # Only the Rust caller reads issued.json; this response is safe for the UI.
            return {**summary(folder), 'app_id': app_id, 'user_id': auth['user_id']}
        raise SetupError('Unknown Slack setup action.')


def main():
    os.umask(0o077)
    try:
        request = json.load(sys.stdin)
        value = execute(Path(sys.argv[1]), request['action'], request.get('body', {}))
        print(json.dumps({'ok': True, 'result': value}))
    except SetupError as error:
        print(json.dumps({'ok': False, 'error': str(error)}))
    except Exception:
        print(json.dumps({'ok': False, 'error': 'The local Slack setup tool could not complete. Check network access and Python 3, then retry. Saved connection state is preserved.'}))


if __name__ == '__main__':
    main()
