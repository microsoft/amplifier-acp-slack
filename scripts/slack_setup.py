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
import stat
import subprocess
import shlex
import zipfile
import sys
import tarfile
import time
import urllib.request

VERSION = '4.12.0'
WINDOWS_CLI_DISTRIBUTION = 'constellation-headless-1'
WINDOWS_CLI_SOURCE = '91df4803911140c5a6da56e88ecd04e25c9c7515'
WINDOWS_CLI_PATCH = 'a5122ebf2ea5a7c012f7205ed477eef4795f3a53260f3c310a12980df80a9476'
WINDOWS_CLI_BINARY = 'slack-headless-1.exe'
WINDOWS_CLI_RECEIPT = 'cli-headless-1.json'
ARCHIVES = {
    ('Linux', 'x86_64'): ('linux_amd64', '8b93b0ff3a30879b5ef09fe01fb3bc457afb368bb26827b526f101eccce96c76'),
    ('Linux', 'aarch64'): ('linux_arm64', 'd98ad8ba0848958a47befb73761a51ebd97a363b431758e177a828c7774bdb7d'),
    ('Darwin', 'x86_64'): ('macOS_amd64', '838aa81d10f82e5e9212800d084924ac66e5807aecaac16ae747c90d985352a2'),
    ('Darwin', 'arm64'): ('macOS_arm64', '23a5001854d2bb283ded8ad941cff13d4b2573ba922233087609b70ce53e4b6d'),
}


class SetupError(Exception):
    pass


class ConfirmationRefusal(SetupError):
    """An exact local validation or completed ticket-exchange rejection."""
    MESSAGES = {
        'invalid_confirmation_format': 'Enter the confirmation code shown by Slack.',
        'authorization_expired': 'That authorization attempt expired. Start authorization again.',
        'invalid_challenge': 'Slack refused that confirmation code and expired its ticket. Start authorization again.',
    }

    def __init__(self, code):
        super().__init__(self.MESSAGES[code])
        self.code = code


def explicit_invalid_challenge(result, args, auth_dir):
    # Pinned Slack CLI 4.12.0: ExchangeAuthTicket returns its API error before
    # saveNewAuth. Match that method and exact known heading, not a substring
    # echoed by a generic failure. Any credential output contradicts refusal.
    if (result.returncode <= 0 or len(args) != 6 or args[:2] != ('auth', 'login')
            or args[2] != '--ticket' or args[4] != '--challenge' or auth_dir is None
            or os.path.lexists(auth_dir / 'credentials.json')):
        return False
    output = result.stdout + result.stderr
    lines = {line.strip() for line in output.splitlines()}
    return ('The following error was returned by the apps.hosted.exchangeAuthTicket Slack API method' in lines
            and 'The challenge code is invalid (invalid_challenge)' in lines
            and set(re.findall(r'\(([a-z][a-z0-9_]+)\)', output)) == {'invalid_challenge'})


def read(path, default=None):
    return json.loads(path.read_text()) if path.exists() else default


def write(path, value):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    temporary = path.with_suffix('.tmp')
    with open(temporary, 'w', opener=lambda p, f: os.open(p, f, 0o600)) as stream:
        json.dump(value, stream)
    temporary.replace(path)


def cli_regular_bytes(path, maximum):
    info = path.lstat()
    if (not stat.S_ISREG(info.st_mode) or info.st_size > maximum
            or getattr(info, 'st_file_attributes', 0) & 0x400):
        raise SetupError('The private Slack setup tool is invalid. Reopen the complete app download.')
    with path.open('rb') as stream:
        opened = os.fstat(stream.fileno())
        if (opened.st_dev, opened.st_ino, opened.st_size) != (info.st_dev, info.st_ino, info.st_size):
            raise SetupError('The private Slack setup tool changed before verification.')
        data = stream.read(maximum + 1)
    if len(data) != info.st_size:
        raise SetupError('The private Slack setup tool changed during verification.')
    return data


def cli_regular_directory(path):
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or getattr(info, 'st_file_attributes', 0) & 0x400:
        raise SetupError('The private Slack setup tool directory is invalid.')


def bundled_windows_cli():
    # This derived CLI is part of the verified private runtime inventory. It
    # preserves the upstream commands and disables only an init-time Windows
    # console-colour query when this helper explicitly sets NO_COLOR.
    root = Path(sys.executable).resolve().parent / 'slack-cli'
    metadata_path = root / 'provenance.json'
    binary = root / 'slack.exe'
    try:
        cli_regular_directory(root)
        metadata = json.loads(cli_regular_bytes(metadata_path, 64 * 1024))
        expected = {'distribution': WINDOWS_CLI_DISTRIBUTION,
                    'upstream_version': VERSION, 'upstream_source': WINDOWS_CLI_SOURCE,
                    'patch_sha256': WINDOWS_CLI_PATCH, 'goos': 'windows', 'goarch': 'amd64'}
        if (not isinstance(metadata, dict) or type(metadata.get('schema')) is not int or metadata['schema'] != 1
                or any(metadata.get(k) != v for k, v in expected.items())
                or metadata.get('cgo_enabled') is not False
                or type(metadata.get('binary_size')) is not int
                or not 0 < metadata['binary_size'] <= 150 * 1024 * 1024
                or not isinstance(metadata.get('binary_sha256'), str)
                or re.fullmatch(r'[0-9a-f]{64}', metadata['binary_sha256']) is None):
            raise SetupError('The bundled Slack setup tool has an unexpected build identity.')
        data = cli_regular_bytes(binary, 150 * 1024 * 1024)
        if len(data) != metadata['binary_size'] or hashlib.sha256(data).hexdigest() != metadata['binary_sha256']:
            raise SetupError('The bundled Slack setup tool failed verification.')
    except (OSError, ValueError, TypeError) as error:
        raise SetupError('The bundled Slack setup tool is missing or invalid. Reopen the complete app download.') from error
    receipt = {'version': VERSION, 'distribution': WINDOWS_CLI_DISTRIBUTION,
               'upstream_source': WINDOWS_CLI_SOURCE, 'patch_sha256': WINDOWS_CLI_PATCH,
               'sha256': metadata['binary_sha256'], 'size': len(data)}
    return data, receipt


def ensure_cli(folder):
    windows = sys.platform == 'win32'
    binary = folder / 'bin' / (WINDOWS_CLI_BINARY if windows else 'slack')
    receipt_path = folder / (WINDOWS_CLI_RECEIPT if windows else 'cli.json')
    if windows:
        data, expected = bundled_windows_cli()
        temporary = binary.with_suffix('.new')
        receipt_temporary = receipt_path.with_suffix('.tmp')
        if os.path.lexists(temporary) or os.path.lexists(receipt_temporary):
            raise SetupError('A previous Slack setup-tool preparation is incomplete. Its files have been preserved.')
        receipt = {}
        if os.path.lexists(receipt_path):
            try:
                receipt = json.loads(cli_regular_bytes(receipt_path, 64 * 1024))
            except (OSError, ValueError, TypeError) as error:
                raise SetupError('The private Slack setup-tool receipt is invalid; it has been preserved.') from error
        if os.path.lexists(binary.parent):
            cli_regular_directory(binary.parent)
        if os.path.lexists(binary):
            cached = cli_regular_bytes(binary, 150 * 1024 * 1024)
            if receipt == expected and hashlib.sha256(cached).hexdigest() == expected['sha256']:
                return binary
        # Keep the official cache and its receipt intact for older helpers
        # after rollback. This fixed derived slot is never an implicit fallback
        # for that cache, and has no network or unpatched-tool fallback.
        binary.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        cli_regular_directory(binary.parent)
        with open(temporary, 'xb', opener=lambda p, f: os.open(p, f, 0o700)) as stream:
            stream.write(data)
        temporary.chmod(0o700)
        with open(receipt_temporary, 'x', encoding='utf-8',
                  opener=lambda p, f: os.open(p, f, 0o600)) as stream:
            json.dump(expected, stream)
        temporary.replace(binary)
        receipt_temporary.replace(receipt_path)
        return binary
    receipt = read(receipt_path, {})
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
            candidates = [m for m in bundle.getmembers() if m.name in {'bin/slack', './bin/slack'}]
            if len(candidates) != 1:
                raise SetupError('The Slack setup download is invalid.')
            member = candidates[0]
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
    # The pinned CLI is a contained, noninteractive setup child, not a user
    # terminal. It continues inheriting the helper's Job Object on Windows.
    window = {'creationflags': subprocess.CREATE_NO_WINDOW} if sys.platform == 'win32' else {}
    result = subprocess.run([str(binary), '--config-dir', str(config), '--no-color',
                             '--skip-update', *args], cwd=cwd or folder, env=env,
                            input='', text=True, capture_output=True, timeout=120, **window)
    output = result.stdout + result.stderr
    if result.returncode:
        if explicit_invalid_challenge(result, args, auth_dir):
            raise ConfirmationRefusal('invalid_challenge')
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


def named(label, team_id):
    return f'{label} ({team_id})' if label and label != team_id else team_id


def wrong_workspace(auth, body, outcome):
    approved = named(auth.get('team_domain'), auth['team_id'])
    intended = named(body.get('team_name'), body['team_id'])
    return (f'Slack approved access to {approved}, but this connection is for {intended}. {outcome} '
            f'Connect to Slack again, then run the new setup command in Slackbot in {intended}.')


def prepare_project(folder, manifest, app_id, auth):
    project = folder / 'project'
    # Refuse a mismatched runtime/layout before materializing a project. Hooks
    # run from this exact cwd; absolute user paths must never enter shell text.
    commands = {action: hook_command(project, action) for action in ('hooks', 'manifest', 'install')}
    (project / '.slack').mkdir(mode=0o700, parents=True, exist_ok=True)
    write(project / 'manifest.json', manifest)
    write(project / '.slack/hooks.json', {'hooks': {'get-hooks': commands['hooks']}})
    write(project / '.slack/config.json', {'manifest': {'source': 'local'}})
    (project / 'hooks.py').write_text(HOOK.replace('__MANIFEST_COMMAND__', repr(commands['manifest'])).replace('__INSTALL_COMMAND__', repr(commands['install'])))
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
    if action not in ('hooks', 'manifest', 'install'):
        raise SetupError('Invalid private Slack setup action.')
    project = Path(project)
    if (not project.is_absolute() or '..' in project.parts or
            project.parts[-3:] != ('secrets', 'slack-setup', 'project')):
        raise SetupError('The private Slack setup project does not match this installation.')
    home = project.parents[2]

    def ordinary(path, directory=False, optional=False):
        try:
            info = path.lstat()
        except FileNotFoundError:
            if optional:
                return False
            raise SetupError('The private Slack setup path is missing.') from None
        valid_type = stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode)
        if not valid_type or getattr(info, 'st_file_attributes', 0) & 0x400:
            raise SetupError('The private Slack setup path has changed.')
        return True

    for directory in (home, home / 'secrets', project.parent):
        ordinary(directory, directory=True)
    ordinary(project, directory=True, optional=True)
    ordinary(project / 'hooks.py', optional=True)
    windows = sys.platform == 'win32'
    interpreter = home / 'runtime/python' / ('python.exe' if windows else 'bin/python3')
    if ordinary(interpreter, optional=True):
        for directory in (home / 'runtime', home / 'runtime/python', interpreter.parent):
            ordinary(directory, directory=True)
        try:
            actual = Path(sys.executable).resolve(strict=True)
            expected = interpreter.resolve(strict=True)
            same = (os.path.normcase(str(actual)) == os.path.normcase(str(expected))
                    and os.path.samefile(actual, expected))
        except OSError:
            same = False
        if not same:
            raise SetupError('The Slack setup interpreter does not match this installation.')
        # CLI4.12.0 splits on whitespace before handing hooks to PowerShell or
        # a POSIX shell. Fixed relative tokens survive that normalization and
        # do not interpret a username/home as shell syntax. cli() owns cwd.
        prefix = (r'& ..\..\..\runtime\python\python.exe -I -B -X utf8 .\hooks.py '
                  if windows else '../../../runtime/python/bin/python3 -I -B -X utf8 ./hooks.py ')
        return prefix + action
    server = read(home / 'adapter.json', {}).get('server') if sys.platform == 'linux' else None
    # Match Rust ServerConfig's serde default for existing PAM configurations,
    # while refusing a missing configuration or an explicit local-password one.
    if not isinstance(server, dict) or server.get('auth_mode', 'pam') != 'pam':
        raise SetupError('The connector private setup runtime is missing. Reopen the complete app bundle.')
    # The existing Linux PAM installer intentionally uses its system Python.
    # It must not be redirected to an absent bundled runtime. The project script
    # is still fixed relative to the same private cwd.
    return shlex.join([sys.executable, '-I', '-B', '-X', 'utf8', './hooks.py', action])


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
                raise ConfirmationRefusal('authorization_expired')
            if not re.fullmatch(r'[A-Za-z0-9_-]{4,256}', challenge):
                raise ConfirmationRefusal('invalid_confirmation_format')
            staging = folder / 'pending-auth'
            staging.mkdir(mode=0o700, exist_ok=True)
            (staging / 'credentials.json').unlink(missing_ok=True)
            try:
                cli(folder, 'auth', 'login', '--ticket', pending['ticket'], '--challenge', challenge, auth_dir=staging)
            except ConfirmationRefusal as error:
                if error.code != 'invalid_challenge' or os.path.lexists(staging / 'credentials.json'):
                    raise SetupError('Slack authorization was not confirmed. Check saved setup before continuing.')
                # This known Slack rejection expires this ticket. Retire only
                # the exact pending ticket under our existing installation lock;
                # creating a replacement still requires explicit authorize.
                if read(folder / 'pending.json', {}) != pending:
                    raise SetupError('Slack authorization changed. Check saved setup before continuing.')
                (folder / 'pending.json').unlink()
                raise
            auth = authorization(folder, staging)
            if body.get('team_id') and auth['team_id'] != body['team_id']:
                # Slack consumed the ticket for this approval. Drop both, so the next
                # attempt gets a fresh command; previously saved access is untouched.
                (staging / 'credentials.json').unlink(missing_ok=True)
                (folder / 'pending.json').unlink(missing_ok=True)
                raise SetupError(wrong_workspace(auth, body, 'Nothing was created, and saved Slack access is unchanged.'))
            write(folder / 'auth/credentials.json', {auth['team_id']: auth})
            (folder / 'authorization-health.json').unlink(missing_ok=True)
            (staging / 'credentials.json').unlink(missing_ok=True)
            (folder / 'pending.json').unlink(missing_ok=True)
            return summary(folder)
        if action == 'discard':
            # Project preparation precedes external deploy. A missing apps.json
            # cannot prove that deploy had no effect: its reply may have been lost.
            # Keep access even for older attempts that have no explicit marker.
            if (folder / 'project').exists():
                raise SetupError('Slack app setup was already prepared or attempted, so saved Slack access was kept. Check that setup before switching workspaces.')
            for name in ('auth/credentials.json', 'pending-auth/credentials.json', 'authorization-health.json'):
                (folder / name).unlink(missing_ok=True)
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
                raise SetupError(wrong_workspace(auth, body, 'Nothing was created.'))
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
    except ConfirmationRefusal as error:
        print(json.dumps({'ok': False, 'error': str(error), 'confirmation_refusal': {'schema': 1, 'action': 'confirm', 'code': error.code}}))
    except SetupError as error:
        print(json.dumps({'ok': False, 'error': str(error)}))
    except Exception:
        print(json.dumps({'ok': False, 'error': 'The local Slack setup tool could not complete. Check network access and reopen the installed connector, then retry. Saved connection state is preserved.'}))


if __name__ == '__main__':
    main()
