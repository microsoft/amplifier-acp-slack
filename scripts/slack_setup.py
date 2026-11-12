#!/usr/bin/env python3
"""Private, per-installation Slack CLI bridge. Never print CLI output or tokens.

The service invokes this helper with JSON on stdin. Only the internal `token`
operation returns an access token, directly to the service's private pipe.
"""
import os
if os.name == "nt":
    import msvcrt
else:
    import fcntl
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import shlex
import zipfile
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
    # Slack publishes x64 only. Windows 11 ARM runs this official CLI through OS x64 compatibility.
    ('Windows', 'AMD64'): ('windows_64-bit', 'b1ca5e13e7c90cc7a9dfcb2af055f91d9a1e22db20790caa50ab530c07d4e914'),
    ('Windows', 'ARM64'): ('windows_64-bit', 'b1ca5e13e7c90cc7a9dfcb2af055f91d9a1e22db20790caa50ab530c07d4e914'),
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
    binary = folder / ('bin/slack.exe' if os.name == 'nt' else 'bin/slack')
    receipt = read(folder / 'cli.json', {})
    if binary.exists() and receipt.get('version') == VERSION:
        if hashlib.sha256(binary.read_bytes()).hexdigest() == receipt.get('sha256'):
            return binary
    target = ARCHIVES.get((platform.system(), platform.machine()))
    if not target:
        raise SetupError('Automatic Slack setup is not available for this processor. Manual credentials remain available in Advanced setup.')
    suffix, checksum = target
    extension = 'zip' if suffix.startswith('windows_') else 'tar.gz'
    url = f'https://github.com/slackapi/slack-cli/releases/download/v{VERSION}/slack_cli_{VERSION}_{suffix}.{extension}'
    with urllib.request.urlopen(url, timeout=45) as response:
        archive = response.read(100 * 1024 * 1024 + 1)
    if len(archive) > 100 * 1024 * 1024 or hashlib.sha256(archive).hexdigest() != checksum:
        raise SetupError('The Slack setup tool could not be verified. Retry the download.')
    if extension == 'zip':
        with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
            candidates = [m for m in bundle.infolist() if m.filename.replace('\\', '/').split('/')[-1] == 'slack.exe' and not m.is_dir()]
            if len(candidates) != 1 or candidates[0].file_size > 150 * 1024 * 1024:
                raise SetupError('The Slack setup download is invalid.')
            data = bundle.read(candidates[0])
    else:
        with tarfile.open(fileobj=io.BytesIO(archive), mode='r:gz') as bundle:
            member = bundle.getmember('bin/slack')
            if not member.isfile() or member.size > 150 * 1024 * 1024:
                raise SetupError('The Slack setup download is invalid.')
            data = bundle.extractfile(member).read()
    binary.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    temporary = binary.with_suffix('.new')
    temporary.write_bytes(data)
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
                # An explicit rejection is different from a network failure.
                # Keep this small status record safe for the management UI.
                if auth_dir is None and code in {'invalid_auth', 'token_revoked', 'token_expired', 'missing_scope'}:
                    write(folder / 'authorization-health.json', {'state': 'needs_attention', 'message': explanation})
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
    health = read(folder / 'authorization-health.json', {})
    ready['authorization'] = 'managed' if ready['authorized'] else 'missing'
    if ready['authorized'] and health.get('state') == 'needs_attention':
        ready['authorization'] = 'needs_attention'
        ready['authorization_issue'] = health['message']
    pending = read(folder / 'pending.json', {})
    ready['authorization_pending'] = pending.get('expires_at', 0) > time.time()
    return ready


def prepare_project(folder, manifest, app_id, auth):
    project = folder / 'project'
    (project / '.slack').mkdir(mode=0o700, parents=True, exist_ok=True)
    write(project / 'manifest.json', manifest)
    write(project / '.slack/hooks.json', {'hooks': {'get-hooks': hook_command(project, 'hooks')}})
    write(project / '.slack/config.json', {'manifest': {'source': 'local'}})
    (project / 'hooks.py').write_text(HOOK.replace('__MANIFEST_COMMAND__', repr(hook_command(project, 'manifest'))).replace('__INSTALL_COMMAND__', repr(hook_command(project, 'install'))))
    apps_path = project / '.slack/apps.json'
    apps = read(apps_path, {'apps': {}})
    existing = apps.get('apps', {}).get(auth['team_id'], {}).get('app_id')
    if app_id and existing and app_id != existing:
        raise SetupError('This installation is already associated with a different Slack app.')
    if app_id and not existing:
        apps.setdefault('apps', {})[auth['team_id']] = {'app_id': app_id, 'team_id': auth['team_id'], 'team_domain': auth.get('team_domain', '')}
        write(apps_path, apps)
    return project


def hook_command(project, action):
    args = [sys.executable, '-I', '-B', '-X', 'utf8', str(project / 'hooks.py'), action]
    return subprocess.list2cmdline(args) if os.name == 'nt' else shlex.join(args)


HOOK = '''import json, os, pathlib, sys
p = pathlib.Path(__file__).resolve().parent
if sys.argv[1] == "hooks":
    print(json.dumps({"runtime":"rust", "hooks":{"get-manifest":__MANIFEST_COMMAND__, "deploy":__INSTALL_COMMAND__}}))
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
            if os.name == 'nt':
                guard.seek(0, os.SEEK_END)
                if guard.tell() == 0: guard.write(' '); guard.flush()
                guard.seek(0)
                msvcrt.locking(guard.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                fcntl.flock(guard, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except (BlockingIOError, PermissionError):
            raise SetupError('A Slack setup operation is already running. Try again shortly.')
        if action == 'authorize':
            pending = read(folder / 'pending.json', {})
            if pending.get('expires_at', 0) > time.time() and pending.get('ticket'):
                return {'command': '/slackauthticket ' + pending['ticket'], 'expires_at': pending['expires_at']}
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
            (folder / 'authorization-health.json').unlink(missing_ok=True)
            (staging / 'credentials.json').unlink(missing_ok=True)
            (folder / 'pending.json').unlink(missing_ok=True)
            return summary(folder)
        if action == 'token':
            cli(folder, 'auth', 'list')  # CLI owns rotation; never duplicate its refresh token.
            auth = authorization(folder)
            if auth['team_id'] != body.get('team_id'):
                raise SetupError('Slack authorization belongs to another workspace.')
            (folder / 'authorization-health.json').unlink(missing_ok=True)
            return {'token': auth['token']}
        if action == 'connect':
            cli(folder, 'auth', 'list')
            auth = authorization(folder)
            if body.get('team_id') and body['team_id'] != auth['team_id']:
                raise SetupError('This connection belongs to another workspace. Authorize its original workspace.')
            (folder / 'authorization-health.json').unlink(missing_ok=True)
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
        print(json.dumps({'ok': False, 'error': 'The local Slack setup tool could not complete. Check network access and reopen the installed connector, then retry. Saved connection state is preserved.'}))


if __name__ == '__main__':
    main()
