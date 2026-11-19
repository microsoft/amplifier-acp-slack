#!/usr/bin/env python3
"""Packaged-Python/native-shell hook proof, not a Slack CLI/auth/deploy test.

The caller uses CREATE_NO_WINDOW on Windows, just like the production CLI
parent. The shell receives no hiding flag: upstream 91df uses plain exec.Command.
Only fixture-generated hooks and fake credentials are used. No network call.
"""
import argparse
import ctypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import struct
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

LIMIT = 1024 * 1024
FAKE = {'app_token': 'fixture-only-app-token', 'bot_token': 'fixture-only-bot-token'}


def sha(data):
    return hashlib.sha256(data).hexdigest()


def ordinary(path):
    path = Path(path)
    for part in (path, *path.parents):
        info = part.lstat()
        if stat.S_ISLNK(info.st_mode) or getattr(info, 'st_file_attributes', 0) & 0x400:
            raise AssertionError('Linked fixture path')
    if not path.is_file():
        raise AssertionError('Missing ordinary fixture file')
    return path


def read_small(path):
    path = ordinary(path)
    if path.stat().st_size > LIMIT:
        raise AssertionError('Fixture record exceeds bound')
    return path.read_bytes()


def shell_argv(command, windows=None):
    # Exact pinned upstream hooks.go Fields followed by shell.go Join. All new
    # commands are fixed ASCII, so no dynamic path passes through this grammar.
    script = ' '.join(command.split())
    windows = os.name == 'nt' if windows is None else windows
    if windows:
        return [str(Path(os.environ['SystemRoot']) / 'System32/WindowsPowerShell/v1.0/powershell.exe'), '-Command', script]
    return ['/bin/sh', '-c', script]


def shell_environment():
    allowed = {'SYSTEMROOT', 'WINDIR', 'USERPROFILE', 'HOMEDRIVE', 'HOMEPATH',
               'TEMP', 'TMP', 'TMPDIR', 'HOME', 'USER', 'USERNAME', 'LANG', 'LC_ALL'}
    env = {k: v for k, v in os.environ.items() if k.upper() in allowed}
    env['PATH'] = (os.pathsep.join([str(Path(os.environ['SystemRoot']) / 'System32'),
                                  str(Path(os.environ['SystemRoot']) / 'System32/WindowsPowerShell/v1.0')])
                   if os.name == 'nt' else '/usr/bin:/bin')
    env['SHELL'] = '/bin/sh'
    env['PYTHONDONTWRITEBYTECODE'] = '1'
    return env


def pe_subsystem(path, windows=None):
    if not (os.name == 'nt' if windows is None else windows):
        return None
    with ordinary(path).open('rb') as stream:
        header = stream.read(64)
        if len(header) != 64 or header[:2] != b'MZ':
            raise AssertionError('Expected Windows executable')
        offset = struct.unpack_from('<I', header, 60)[0]
        if offset > LIMIT:
            raise AssertionError('PE header bound')
        stream.seek(offset)
        header = stream.read(94)
    if len(header) != 94 or header[:4] != b'PE\0\0' or struct.unpack_from('<H', header, 24)[0] not in (0x10b, 0x20b):
        raise AssertionError('Invalid PE header')
    return struct.unpack_from('<H', header, 24 + 68)[0]


def file_sha(path):
    path = ordinary(path)
    if path.stat().st_size > 1024 * LIMIT:
        raise AssertionError('Fixture executable exceeds bound')
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        remaining = 1024 * LIMIT + 1
        while remaining:
            block = stream.read(min(remaining, LIMIT))
            if not block:
                return digest.hexdigest()
            digest.update(block)
            remaining -= len(block)
    raise AssertionError('Fixture executable exceeds bound')


def official_cli_record(folder, windows=None):
    """Read the existing, already version-checked CLI; never execute it again."""
    windows = os.name == 'nt' if windows is None else windows
    raw = read_small(folder / 'cli.json')
    receipt = json.loads(raw)
    binary = folder / ('bin/slack.exe' if windows else 'bin/slack')
    digest = file_sha(binary)
    if set(receipt) != {'version', 'sha256'} or receipt['version'] != '4.12.0' or receipt['sha256'] != digest:
        raise AssertionError('Official CLI identity differs from its retained receipt')
    return {'version': receipt['version'], 'sha256': digest, 'receipt_sha256': sha(raw),
            'pe_subsystem': pe_subsystem(binary, windows), 'executed_again': False,
            'limit': 'PE subsystem is executable metadata, not proof of console-window behavior.'}


def child_identity(child):
    result = {'pid': child.pid}
    if os.name != 'nt':
        return result
    from ctypes import wintypes as w
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.GetProcessTimes.argtypes = [w.HANDLE, *([ctypes.POINTER(w.FILETIME)] * 4)]
    kernel.QueryFullProcessImageNameW.argtypes = [w.HANDLE, w.DWORD, w.LPWSTR, ctypes.POINTER(w.DWORD)]
    times = [w.FILETIME() for _ in range(4)]
    if not kernel.GetProcessTimes(child._handle, *(ctypes.byref(t) for t in times)):
        raise OSError('Cannot observe owned child generation')
    path, size = ctypes.create_unicode_buffer(32768), w.DWORD(32768)
    if not kernel.QueryFullProcessImageNameW(child._handle, 0, path, ctypes.byref(size)):
        raise OSError('Cannot observe owned child image')
    result.update(birth=times[0].dwLowDateTime | (times[0].dwHighDateTime << 32), **diagnostic_image(path.value))
    return result


def diagnostic_image(value):
    # Bound both phase-child and new-window paths inside the 1 MiB checkpoint.
    return {'image': value if len(value) <= 1024 else None, 'image_chars': len(value),
            'image_sha256': sha(value.encode('utf-8'))}


def console_details(row):
    """One bounded metadata snapshot; never changes the window failure rule."""
    from ctypes import wintypes as w
    result = {'window': row['window'], 'pid': row['pid'], 'errors': [],
              'process': None, 'window_pid_matches': False}
    user = ctypes.WinDLL('user32', use_last_error=True)
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    user.GetWindowThreadProcessId.argtypes = [w.HWND, ctypes.POINTER(w.DWORD)]
    user.GetWindowThreadProcessId.restype = w.DWORD
    user.IsWindowVisible.argtypes = [w.HWND]; user.IsWindowVisible.restype = w.BOOL
    user.GetWindowRect.argtypes = [w.HWND, ctypes.POINTER(w.RECT)]
    user.GetWindowRect.restype = w.BOOL
    kernel.OpenProcess.argtypes = [w.DWORD, w.BOOL, w.DWORD]; kernel.OpenProcess.restype = w.HANDLE
    kernel.CloseHandle.argtypes = [w.HANDLE]; kernel.CloseHandle.restype = w.BOOL
    kernel.GetProcessTimes.argtypes = [w.HANDLE, *([ctypes.POINTER(w.FILETIME)] * 4)]
    kernel.GetProcessTimes.restype = w.BOOL
    kernel.QueryFullProcessImageNameW.argtypes = [w.HANDLE, w.DWORD, w.LPWSTR, ctypes.POINTER(w.DWORD)]
    kernel.QueryFullProcessImageNameW.restype = w.BOOL
    kernel.GetExitCodeProcess.argtypes = [w.HANDLE, ctypes.POINTER(w.DWORD)]
    kernel.GetExitCodeProcess.restype = w.BOOL
    def failed(api):
        result['errors'].append({'api': api, 'winerror': ctypes.get_last_error()})
    first, last = w.DWORD(), w.DWORD()
    first_ok = user.GetWindowThreadProcessId(row['window'], ctypes.byref(first))
    result['visible'] = bool(user.IsWindowVisible(row['window']))
    rect = w.RECT()
    if user.GetWindowRect(row['window'], ctypes.byref(rect)):
        result['rect'] = [rect.left, rect.top, rect.right, rect.bottom]
    else:
        failed('GetWindowRect')
    try:
        dwm = ctypes.WinDLL('dwmapi', use_last_error=True)
        query = dwm.DwmGetWindowAttribute
        query.argtypes = [w.HWND, w.DWORD, ctypes.c_void_p, w.DWORD]
        query.restype = ctypes.c_long
        cloak = w.DWORD()
        status = query(row['window'], 14, ctypes.byref(cloak), ctypes.sizeof(cloak))
        result['cloak_hresult'] = int(status)
        if status == 0:
            result['cloaked'] = int(cloak.value)
    except (OSError, AttributeError) as error:
        result['errors'].append({'api': 'DwmGetWindowAttribute', 'error_type': type(error).__name__})
    if first_ok and first.value == row['pid'] and row['pid'] > 0:
        handle = kernel.OpenProcess(0x1000, False, row['pid'])  # QUERY_LIMITED_INFORMATION only.
        if handle:
            try:
                process = {'pid': row['pid']}; result['process'] = process
                times = [w.FILETIME() for _ in range(4)]
                if kernel.GetProcessTimes(handle, *(ctypes.byref(t) for t in times)):
                    process['birth'] = times[0].dwLowDateTime | (times[0].dwHighDateTime << 32)
                else:
                    failed('GetProcessTimes')
                path, size = ctypes.create_unicode_buffer(32768), w.DWORD(32768)
                if kernel.QueryFullProcessImageNameW(handle, 0, path, ctypes.byref(size)):
                    process.update(diagnostic_image(path.value))
                else:
                    failed('QueryFullProcessImageNameW')
                code = w.DWORD()
                if kernel.GetExitCodeProcess(handle, ctypes.byref(code)):
                    process['exit_code'] = code.value
                else:
                    failed('GetExitCodeProcess')
            finally:
                if not kernel.CloseHandle(handle):
                    failed('CloseHandle')
        else:
            failed('OpenProcess')
    last_ok = user.GetWindowThreadProcessId(row['window'], ctypes.byref(last))
    result['window_pid_matches'] = bool(first_ok and last_ok and first.value == last.value == row['pid'])
    result['limit'] = 'Read-only PID-bracketed metadata; not an atomic window/process ownership or ancestry proof.'
    return result


class WindowsConsoleObserver:
    """Bounded new-visible-window observation; no titles or unrelated argv."""
    def __init__(self):
        self.stop = threading.Event()
        self.thread = None
        self.samples = 0
        self.failure = None
        self.before = []
        self.new = {}
        self.details = {}

    def remember(self, row, getter=console_details):
        key = (row['window'], row['pid'])
        first = key not in self.new
        self.new[key] = row  # Keep the original strict new-window predicate.
        if first and len(self.details) < 32:
            try:
                self.details[key] = getter(row)
            except Exception as error:
                self.details[key] = {'window': row['window'], 'pid': row['pid'],
                                     'error_type': type(error).__name__}

    @staticmethod
    def windows():
        from ctypes import wintypes as w
        api = ctypes.WinDLL('user32', use_last_error=True)
        callback_type = ctypes.WINFUNCTYPE(w.BOOL, w.HWND, w.LPARAM)
        api.EnumWindows.argtypes = [callback_type, w.LPARAM]
        api.IsWindowVisible.argtypes = [w.HWND]
        api.GetClassNameW.argtypes = [w.HWND, w.LPWSTR, ctypes.c_int]
        api.GetWindowThreadProcessId.argtypes = [w.HWND, ctypes.POINTER(w.DWORD)]
        rows = []
        @callback_type
        def visit(window, unused):
            if api.IsWindowVisible(window):
                name, pid = ctypes.create_unicode_buffer(256), w.DWORD()
                api.GetClassNameW(window, name, len(name))
                if name.value in ('ConsoleWindowClass', 'CASCADIA_HOSTING_WINDOW_CLASS'):
                    api.GetWindowThreadProcessId(window, ctypes.byref(pid))
                    rows.append({'window': int(window), 'pid': pid.value, 'class': name.value})
            return len(rows) <= 64
        ok = api.EnumWindows(visit, 0)
        if not ok or len(rows) > 64:
            raise OSError('Console observation unavailable or exceeds bound')
        return rows

    def start(self):
        if os.name != 'nt':
            return
        self.before = self.windows()
        known = {(r['window'], r['pid']) for r in self.before}
        def observe():
            try:
                while not self.stop.is_set():
                    self.samples += 1
                    if self.samples > 10000:
                        raise OSError('Console sampling bound')
                    for row in self.windows():
                        key = (row['window'], row['pid'])
                        if key not in known:
                            self.remember(row)
                    if len(self.new) > 64:
                        raise OSError('Console event bound')
                    self.stop.wait(.02)
            except BaseException as error:
                self.failure = type(error).__name__
        self.thread = threading.Thread(target=observe, daemon=True)
        self.thread.start()

    def finish(self):
        self.stop.set()
        if self.thread:
            self.thread.join(timeout=2)
            if self.thread.is_alive():
                self.failure = 'ObserverJoinUnknown'
        return {'applicable': os.name == 'nt', 'samples': self.samples, 'failure_type': self.failure,
                'baseline': self.before, 'new_visible_windows': list(self.new.values()),
                'new_window_details': list(self.details.values()), 'detail_limit': 32,
                'details_omitted': max(0, len(self.new) - len(self.details)),
                'limit': 'Sampled visible console classes only; new windows are not attributed to a process tree and absence is not a universal no-flash proof.'}


def run(home, helper_path, helper_sha256, result):
    receipt = {'schema': 1, 'passed': False, 'phases': [], 'scope': 'actual packaged Python and native shell; pinned CLI shell grammar only, no CLI deploy/auth chain',
               'explicit_slack_requests': 0, 'fake_install_only': True,
               'shell_profile_effects': 'Native shell uses upstream default flags; ambient profile effects are not independently audited.',
               'windows_parent_substitution': 'Packaged Python substitutes for the Go CLI parent; caller uses CREATE_NO_WINDOW, native PowerShell uses default flags.'}
    def save():
        result.write_text(json.dumps(receipt, indent=2) + '\n', encoding='utf-8')
    with result.open('x', encoding='utf-8') as stream:
        json.dump(receipt, stream)
    observer = WindowsConsoleObserver()
    try:
        expected = home / 'runtime/python' / ('python.exe' if os.name == 'nt' else 'bin/python3')
        if not os.path.samefile(ordinary(expected), ordinary(Path(sys.executable))):
            raise AssertionError('Use this fixture installation private Python')
        helper_bytes = read_small(helper_path)
        if sha(helper_bytes) != helper_sha256:
            raise AssertionError('Materialized helper differs')
        config_bytes = read_small(home / 'adapter.json')
        config = json.loads(config_bytes)
        if config['enabled'] or config['setup_complete'] or config['slack']['app_id']:
            raise AssertionError('Only empty finite fixture installation allowed')
        folder = home / 'secrets/slack-setup'
        if os.path.lexists(folder):
            raise AssertionError('Do not reuse an existing hook project')
        protected = {p: sha(read_small(p)) for p in (home / 'secrets').rglob('*') if p.is_file()}
        spec = importlib.util.spec_from_file_location('exact_packaged_setup', helper_path)
        helper = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(helper)
        folder.mkdir(mode=0o700)
        manifest = {'display_information': {'name': 'Local hook contract fixture'}}
        project = helper.prepare_project(folder, manifest, '', {'team_id': 'TFIXTURE', 'team_domain': 'local-fixture'})
        if project != folder / 'project':
            raise AssertionError('Unexpected fixture project')
        receipt.update(private_python_sha256=file_sha(expected), private_python_pe_subsystem=pe_subsystem(expected),
                       helper_sha256=helper_sha256, project_has_repeated_spaces='  ' in str(project),
                       official_cli=official_cli_record(helper_path.parent / 'slack-setup'))
        observer.start()
        for action in ('hooks', 'manifest', 'install'):
            command = helper.hook_command(project, action)
            argv = shell_argv(command)
            env = shell_environment()
            if action == 'install':
                env.update(SLACK_APP_TOKEN=FAKE['app_token'], SLACK_BOT_TOKEN=FAKE['bot_token'])
            phase = {'action': action, 'status': 'dispatching', 'command_sha256': sha(command.encode()),
                     'shell_pe_subsystem': pe_subsystem(Path(argv[0])), 'shell_creationflags': 0}
            receipt['phases'].append(phase); save()
            # Native hooks have tiny known responses. Files avoid unbounded pipe
            # buffering if a shell fails unexpectedly. They remain with this
            # fixture on uncertainty; no process is signalled or replayed.
            stdout_path = folder / (action + '-stdout')
            stderr_path = folder / (action + '-stderr')
            with stdout_path.open('xb') as out, stderr_path.open('xb') as err:
                child = subprocess.Popen(argv, cwd=project, env=env, stdin=subprocess.DEVNULL,
                                         stdout=out, stderr=err)
                phase.update(child_identity(child)); save()
                try:
                    child.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    phase['status'] = 'unknown_timeout_no_retry_no_kill'
                    raise
            stdout, stderr = read_small(stdout_path), read_small(stderr_path)
            phase.update(returncode=child.returncode, stdout_size=len(stdout), stderr_size=len(stderr),
                         stdout_sha256=sha(stdout), stderr_sha256=sha(stderr), status='returned')
            save()
            if child.returncode != 0 or len(stdout) > LIMIT or len(stderr) > LIMIT:
                raise AssertionError('Native hook failed or output exceeded bound')
            if action == 'hooks':
                value = json.loads(stdout)
                if value != {'runtime': 'rust', 'hooks': {'get-manifest': helper.hook_command(project, 'manifest'), 'deploy': helper.hook_command(project, 'install')}}:
                    raise AssertionError('Hook discovery differs')
            elif action == 'manifest' and json.loads(stdout) != manifest:
                raise AssertionError('Hook manifest differs')
            elif action == 'install' and json.loads(read_small(folder / 'issued.json')) != FAKE:
                raise AssertionError('Inert install hook did not preserve exact fake values')
        if read_small(home / 'adapter.json') != config_bytes or any(sha(read_small(p)) != value for p, value in protected.items()):
            raise AssertionError('Fixture settings or prior secrets changed')
        receipt.update(settings_preserved=True, prior_secret_hashes_preserved=True, fake_issued_observed=True)
        receipt['console'] = observer.finish()
        if receipt['console']['failure_type'] or receipt['console']['new_visible_windows']:
            raise AssertionError('Native console observation failed or observed a new visible console; attribution remains separate')
        receipt['passed'] = True
    except BaseException as error:
        receipt['failure_type'] = type(error).__name__
        raise
    finally:
        if 'console' not in receipt:
            receipt['console'] = observer.finish()
        if sys.exc_info()[0] is None:
            save()
        else:
            try:
                save()
            except BaseException:
                pass  # A diagnostic write must not replace the original error.
    return receipt


class PureContractTests(unittest.TestCase):
    def test_console_image_text_bound_retains_exact_digest_and_length(self):
        self.assertEqual(diagnostic_image('x' * 1024)['image'], 'x' * 1024)
        value = '\u5b50' * 1025
        result = diagnostic_image(value)
        self.assertIsNone(result['image'])
        self.assertEqual(result['image_chars'], 1025)
        self.assertEqual(result['image_sha256'], sha(value.encode('utf-8')))

    def test_escaped_32_window_and_three_phase_image_metadata_fit_checkpoint(self):
        image = diagnostic_image('\U0001f680' * 1024)
        windows = [{'window': i, 'pid': 42, 'process': {'pid': 42, 'birth': 1, **image}}
                   for i in range(32)]
        phases = [{'action': action, 'child': {'pid': 42, 'birth': 1, **image}}
                  for action in ('hooks', 'manifest', 'install')]
        encoded = json.dumps({'new_window_details': windows, 'phases': phases}, indent=2).encode('utf-8')
        self.assertLess(len(encoded), LIMIT // 2)  # Reserve half for all other fixed metadata.

    def test_console_details_use_limited_handle_and_keep_visibility_separate(self):
        from types import SimpleNamespace
        from unittest.mock import Mock
        def pid(window, pointer):
            pointer._obj.value = 42
            return 1
        def rectangle(window, pointer):
            pointer._obj.left, pointer._obj.top = -10, 20
            pointer._obj.right, pointer._obj.bottom = 30, 40
            return 1
        def times(handle, created, exited, kernel, user):
            created._obj.dwLowDateTime, created._obj.dwHighDateTime = 123, 1
            return 1
        def image(handle, flags, buffer, length):
            buffer.value = 'C:/Windows/System32/example.exe'
            return 1
        def exit_code(handle, pointer):
            pointer._obj.value = 259
            return 1
        def cloak(window, attribute, pointer, size):
            pointer._obj.value = 2
            return 0
        user = SimpleNamespace(GetWindowThreadProcessId=Mock(side_effect=pid),
            IsWindowVisible=Mock(return_value=True), GetWindowRect=Mock(side_effect=rectangle))
        kernel = SimpleNamespace(OpenProcess=Mock(return_value=99), CloseHandle=Mock(return_value=1),
            GetProcessTimes=Mock(side_effect=times), QueryFullProcessImageNameW=Mock(side_effect=image),
            GetExitCodeProcess=Mock(side_effect=exit_code))
        dwm = SimpleNamespace(DwmGetWindowAttribute=Mock(side_effect=cloak))
        libraries = {'user32': user, 'kernel32': kernel, 'dwmapi': dwm}
        with patch.object(ctypes, 'WinDLL', side_effect=lambda name, **kwargs: libraries[name], create=True):
            result = console_details({'window': 1, 'pid': 42})
        kernel.OpenProcess.assert_called_once_with(0x1000, False, 42)
        kernel.CloseHandle.assert_called_once_with(99)
        self.assertEqual(result['process'], {'pid': 42, 'birth': (1 << 32) | 123,
            **diagnostic_image('C:/Windows/System32/example.exe'), 'exit_code': 259})
        self.assertEqual(result['rect'], [-10, 20, 30, 40])
        self.assertEqual(result['cloaked'], 2)
        self.assertTrue(result['visible']); self.assertTrue(result['window_pid_matches'])
        self.assertEqual(result['errors'], [])
        self.assertEqual(user.GetWindowThreadProcessId.call_count, 2)
        observer = WindowsConsoleObserver()
        row = {'window': 1, 'pid': 42, 'class': 'ConsoleWindowClass'}
        observer.remember(row, lambda unused: result)
        self.assertEqual(observer.finish()['new_visible_windows'], [row])

    def test_console_detail_bound_does_not_drop_failure_rows_or_repeat_queries(self):
        from unittest.mock import Mock
        observer = WindowsConsoleObserver()
        getter = Mock(side_effect=lambda row: {'window': row['window'], 'pid': row['pid']})
        for i in range(40):
            row = {'window': i + 1, 'pid': 42, 'class': 'ConsoleWindowClass'}
            observer.remember(row, getter)
            observer.remember(row, getter)
        result = observer.finish()
        self.assertEqual(getter.call_count, 32)
        self.assertEqual(len(result['new_visible_windows']), 40)
        self.assertEqual(len(result['new_window_details']), 32)
        self.assertEqual(result['details_omitted'], 8)

    def test_console_detail_unavailable_retains_strict_row_without_error_text(self):
        observer = WindowsConsoleObserver()
        row = {'window': 1, 'pid': 42, 'class': 'ConsoleWindowClass'}
        def failed(unused):
            raise OSError('Do not retain arbitrary error content')
        observer.remember(row, failed)
        result = observer.finish()
        self.assertEqual(result['new_visible_windows'], [row])
        self.assertEqual(result['new_window_details'], [{'window': 1, 'pid': 42, 'error_type': 'OSError'}])

    def test_fixed_posix_command_preserves_fields(self):
        value = '../../../runtime/python/bin/python3 -I -B -X utf8 ./hooks.py hooks'
        self.assertEqual(shell_argv(value, False), ['/bin/sh', '-c', value])

    def test_windows_uses_real_shell_without_hiding_flags(self):
        with patch.dict(os.environ, {'SystemRoot': 'C:/Windows'}):
            argv = shell_argv('& ../../../runtime/python/python.exe -I -B -X utf8 ./hooks.py hooks', True)
        self.assertEqual(argv[1:], ['-Command', '& ../../../runtime/python/python.exe -I -B -X utf8 ./hooks.py hooks'])
        self.assertNotIn('-WindowStyle', argv)

    def test_pinned_upstream_grammar_exposes_old_repeated_space_loss(self):
        old = "'/private/home  twice/python' -I '/private/home  twice/hooks.py' hooks"
        self.assertNotEqual(shell_argv(old, False)[2], old)

    def test_environment_does_not_inherit_credentials_or_developer_path(self):
        with patch.dict(os.environ, {'OPENAI_API_KEY': 'never-copy', 'SLACK_TOKEN': 'never-copy', 'PATH': '/developer', 'SystemRoot': 'C:/Windows'}, clear=True):
            env = shell_environment()
        self.assertNotIn('OPENAI_API_KEY', env)
        self.assertNotIn('SLACK_TOKEN', env)
        self.assertNotIn('/developer', env['PATH'])

    def test_ordinary_refuses_link_without_reading_target(self):
        if os.name == 'nt':
            self.skipTest('Unprivileged Windows symlink creation is not assumed')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            target = root / 'keep'
            target.write_bytes(b'unchanged')
            link = root / 'link'
            link.symlink_to(target)
            with self.assertRaises(AssertionError):
                read_small(link)
            self.assertEqual(target.read_bytes(), b'unchanged')

    def test_cli_pe_record_binds_existing_receipt_without_execution(self):
        with tempfile.TemporaryDirectory() as temporary:
            folder = Path(temporary).resolve()
            (folder / 'bin').mkdir()
            data = bytearray(64 + 94)
            data[:2] = b'MZ'; struct.pack_into('<I', data, 60, 64)
            data[64:68] = b'PE\0\0'
            struct.pack_into('<H', data, 64 + 24, 0x20b)
            struct.pack_into('<H', data, 64 + 24 + 68, 3)
            binary = folder / 'bin/slack.exe'; binary.write_bytes(data)
            receipt = {'version': '4.12.0', 'sha256': sha(data)}
            (folder / 'cli.json').write_text(json.dumps(receipt))
            with patch.object(subprocess, 'Popen', side_effect=AssertionError('No native launch')) as launch:
                actual = official_cli_record(folder, True)
                self.assertEqual(actual['pe_subsystem'], 3)
                self.assertEqual(actual['sha256'], receipt['sha256'])
                self.assertFalse(actual['executed_again'])
                binary.write_bytes(data + b'changed')
                with self.assertRaisesRegex(AssertionError, 'identity differs'):
                    official_cli_record(folder, True)
                launch.assert_not_called()


if __name__ == '__main__':
    if sys.argv[1:] == ['--self-test']:
        unittest.main(argv=[sys.argv[0]])
    else:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument('--home', type=Path, required=True)
        parser.add_argument('--helper', type=Path, required=True)
        parser.add_argument('--helper-sha256', required=True)
        parser.add_argument('--result', type=Path, required=True)
        args = parser.parse_args()
        run(args.home, args.helper, args.helper_sha256, args.result)
