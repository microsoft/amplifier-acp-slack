#!/usr/bin/env python3
"""Pinned CLI local hook chain plus separate fake-only private-Python install body.

The caller uses CREATE_NO_WINDOW on Windows, just like the production CLI
parent. The shell receives no hiding flag: upstream 91df uses plain exec.Command.
The CLI receives a fresh empty config, Slack telemetry-disable/update-skip
environment flags and no tokens. This does not prove all vendor tracing is off.
No authorization, app deployment or actual workspace connection is exercised.
"""
import argparse
import base64
import ctypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unicodedata
import unittest
from unittest.mock import patch

LIMIT = 1024 * 1024
CLI_OUTPUT_LIMIT = 128 * 1024
CLI_CALL_SECONDS = 120  # Existing production helper.cli allowance; fixture only.
OS_ENVIRONMENT_KEYS = ('SYSTEMROOT', 'WINDIR', 'USERPROFILE', 'HOMEDRIVE', 'HOMEPATH',
                       'TEMP', 'TMP', 'TMPDIR', 'HOME', 'USER', 'USERNAME', 'LANG', 'LC_ALL',
                       'APPDATA', 'LOCALAPPDATA', 'COMSPEC', 'PATHEXT', 'COMPUTERNAME')
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
    env = {k: v for k, v in os.environ.items() if k.upper() in OS_ENVIRONMENT_KEYS}
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


def setup_cli_record(folder, helper=None, windows=None):
    """Read the already version-checked CLI without repeating its version command."""
    windows = os.name == 'nt' if windows is None else windows
    if windows and helper is None:
        raise AssertionError('Windows requires the exact materialized setup helper')
    raw = read_small(folder / (helper.WINDOWS_CLI_RECEIPT if windows else 'cli.json'))
    receipt = json.loads(raw)
    binary = folder / 'bin' / (helper.WINDOWS_CLI_BINARY if windows else 'slack')
    digest = file_sha(binary)
    if windows:
        data, expected = helper.bundled_windows_cli()  # Read-only verified bundled input.
        private = Path(sys.executable).resolve().parent
        provenance_raw = read_small(private / 'slack-cli/provenance.json')
        provenance = json.loads(provenance_raw)
        bundle_raw = read_small(private.parent / 'native-bundle.json')
        bundle = json.loads(bundle_raw)
        inventory = bundle.get('python_files', {})
        if (receipt != expected or digest != expected['sha256']
                or binary.stat().st_size != expected['size'] or sha(data) != digest
                or bundle.get('slack_cli') != provenance
                or inventory.get('slack-cli/provenance.json') != sha(provenance_raw)
                or inventory.get('slack-cli/slack.exe') != digest):
            raise AssertionError('Derived CLI identity differs from its bundled provenance')
        origin = {'kind': 'constellation_derived', 'distribution': expected['distribution'],
                  'upstream_source': expected['upstream_source'], 'patch_sha256': expected['patch_sha256'],
                  'size': expected['size'], 'bundled_provenance_sha256': sha(provenance_raw),
                  'native_bundle_sha256': sha(bundle_raw)}
    else:
        if set(receipt) != {'version', 'sha256'} or receipt['version'] != '4.12.0' or receipt['sha256'] != digest:
            raise AssertionError('Official CLI identity differs from its retained receipt')
        origin = {'kind': 'official_upstream'}
    return {'version': receipt['version'], 'sha256': digest, 'receipt_sha256': sha(raw), **origin,
            'pe_subsystem': pe_subsystem(binary, windows), 'version_command_repeated': False,
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


def cli_command_matches(value, command, project=None):
    if project is None:
        return value == command
    prefix = command + ' --source="'
    if not value.startswith(prefix) or not value.endswith('"'):
        return False
    escaped = value[len(prefix):-1]
    escape = '`"' if os.name == 'nt' else '\\"'
    path = escaped.replace(escape, '"')
    if path.replace('"', escape) != escaped:
        return False
    try:
        a, b = Path(path).resolve(strict=True), project.resolve(strict=True)
        def key(p):
            text = str(p)
            if os.name == 'nt':
                if text.startswith('\\\\?\\UNC\\'): text = '\\\\' + text[8:]
                elif text.startswith('\\\\?\\'): text = text[4:]
                return os.path.normcase(text)
            return unicodedata.normalize('NFC', text) if sys.platform == 'darwin' else text
        return key(a) == key(b) and os.path.samefile(a, b)
    except OSError:
        return False


def cli_hook_proof(stdout, stderr, commands, manifest, project):
    if len(stdout) > CLI_OUTPUT_LIMIT or len(stderr) > CLI_OUTPUT_LIMIT:
        raise AssertionError('Actual CLI output bound')
    text = stdout.decode('utf-8')
    errors = text + stderr.decode('utf-8')
    if any(value in errors for value in ('failed to initialize hook configurations:', 'Error running ', 'Error initializing')):
        raise AssertionError('Actual CLI reported a hook initialization error')
    expected = [('hooks', commands['hooks'], None,
                 {'runtime': 'rust', 'hooks': {'get-manifest': commands['manifest'], 'deploy': commands['install']}}),
                ('manifest', commands['manifest'], project, manifest)]
    debug = re.compile(r'^\[\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\] (.*)$')
    completed, payload, outside, active = [], [], [], False
    for line in text.splitlines():
        match = debug.fullmatch(line)
        if not match:
            if line.strip(): outside.append(line)
            continue
        message = match.group(1)
        if message.startswith('starting hook command: '):
            if active or len(completed) >= len(expected):
                raise AssertionError('Unexpected actual CLI hook start')
            _, command, source, _ = expected[len(completed)]
            if not cli_command_matches(message[len('starting hook command: '):], command, source):
                raise AssertionError('Actual CLI hook command differs')
            active, payload = True, []
        elif message.startswith('finished hook command: '):
            if not active:
                raise AssertionError('Actual CLI hook finish without start')
            action, command, source, value = expected[len(completed)]
            if not cli_command_matches(message[len('finished hook command: '):], command, source):
                raise AssertionError('Actual CLI hook finish differs')
            if json.loads('\n'.join(payload)) != value:
                raise AssertionError('Actual CLI hook response differs')
            completed.append(action); active = False
        elif active:
            payload.append(message)
    if active or completed != ['hooks', 'manifest']:
        raise AssertionError('Actual CLI hooks missing; file fallback is not proof')
    returned = json.loads('\n'.join(outside))
    if not isinstance(returned, dict) or returned.get('display_information') != manifest['display_information']:
        raise AssertionError('Actual CLI final manifest differs')
    return {'actual_go_cli_hooks': completed, 'file_fallback_accepted': False}


def cli_config_checkpoint(config):
    """Own fresh config: two fixed file metadata rows and bounded log prefixes."""
    result = {'scope': 'Own fresh configuration only; log snapshots are not EOF proof',
              'config_exists': False, 'logs_exists': False, 'files': [], 'logs': []}
    for key, path in (('config_exists', config), ('logs_exists', config / 'logs')):
        if not os.path.lexists(path):
            return result
        for part in (path, *path.parents):
            info = part.lstat()
            if not stat.S_ISDIR(info.st_mode) or getattr(info, 'st_file_attributes', 0) & 0x400:
                raise AssertionError('Linked or non-directory CLI checkpoint')
        result[key] = True
        if key == 'config_exists':
            for name in ('config.json', 'credentials.json'):
                # Metadata only, including for a link: never open/hash either file
                # or follow it to another configuration or credential store.
                try:
                    info = (config / name).lstat()
                except FileNotFoundError:
                    result['files'].append({'name': name, 'exists': False})
                    continue
                kind = ('reparse' if getattr(info, 'st_file_attributes', 0) & 0x400 else
                        'symlink' if stat.S_ISLNK(info.st_mode) else
                        'regular' if stat.S_ISREG(info.st_mode) else
                        'directory' if stat.S_ISDIR(info.st_mode) else 'other')
                result['files'].append({'name': name, 'exists': True, 'type': kind,
                                       'size': info.st_size,
                                       'identity': {'device': info.st_dev, 'inode': info.st_ino}})
    # At most two date-named files from this one fresh configuration.
    # Refuse unexpected entries instead of scanning another path or log store.
    with os.scandir(config / 'logs') as entries:
        for entry in entries:
            if len(result['logs']) >= 2 or not re.fullmatch(r'slack-debug-[0-9]{8}\.log', entry.name):
                raise AssertionError('CLI checkpoint log inventory differs')
            path = ordinary(Path(entry.path)); before = path.lstat()
            flags = os.O_RDONLY | getattr(os, 'O_BINARY', 0) | getattr(os, 'O_NOFOLLOW', 0)
            fd = os.open(path, flags)
            with os.fdopen(fd, 'rb') as stream:
                opened = os.fstat(stream.fileno())
                if not stat.S_ISREG(opened.st_mode) or (before.st_dev, before.st_ino) != (opened.st_dev, opened.st_ino):
                    raise AssertionError('CLI checkpoint log changed before read')
                data = stream.read(CLI_OUTPUT_LIMIT + 1)
            overflow = len(data) > CLI_OUTPUT_LIMIT or opened.st_size > CLI_OUTPUT_LIMIT
            data = data[:CLI_OUTPUT_LIMIT]
            result['logs'].append({'name': entry.name, 'observed_size': opened.st_size,
                                   'overflow': overflow, 'snapshot_sha256': sha(data),
                                   'base64': base64.b64encode(data).decode('ascii')})
    result['logs'].sort(key=lambda row: row['name'])
    return result


def cli_elapsed(started):
    # A bounded diagnostic scalar, not an additional wait or success predicate.
    # The cap is explicit and cannot conceal a late/unjoined outcome.
    try:
        elapsed = max(0, round((time.monotonic() - started) * 1000))
        return {'elapsed_ms': min(elapsed, 86_400_000), 'elapsed_capped': elapsed > 86_400_000}
    except Exception as error:
        return {'elapsed_ms': None, 'elapsed_error_type': type(error).__name__}


def cli_exception_observation(child, phase, windows=None):
    """One observational snapshot of existing objects; never settle the action."""
    result = {'timing': 'after_original_communicate_exception_before_config_checkpoint',
              'identity': {'pid': phase.get('pid'), 'birth': phase.get('birth'),
                           'binding': 'initial_owned_child_identity'},
              'scope': 'Point-in-time only; not continuous liveness or pipe/output completion',
              'process': {'state': 'unavailable'}, 'threads': {}}
    for name in ('_stdin_thread', 'stdout_thread', 'stderr_thread'):
        row = {'present': None, 'alive': None}; result['threads'][name] = row
        try:
            thread = getattr(child, name, None)
            row['present'] = thread is not None
            if thread is not None:
                row['alive'] = bool(thread.is_alive())
        except BaseException as error:
            row['error_type'] = type(error).__name__
    if not (os.name == 'nt' if windows is None else windows):
        result['process']['reason'] = 'windows_only'
        return result
    try:
        if (type(phase.get('pid')) is not int or phase['pid'] != child.pid
                or type(phase.get('birth')) is not int or phase['birth'] <= 0):
            raise ValueError('Initial child identity unavailable')
        from ctypes import wintypes as w
        kernel = ctypes.WinDLL('kernel32', use_last_error=True)
        kernel.WaitForSingleObject.argtypes = [w.HANDLE, w.DWORD]
        kernel.WaitForSingleObject.restype = w.DWORD
        kernel.GetExitCodeProcess.argtypes = [w.HANDLE, ctypes.POINTER(w.DWORD)]
        kernel.GetExitCodeProcess.restype = w.BOOL
        handle = child._handle  # Borrow only; Popen retains ownership.
        if not handle:
            raise ValueError('Owned child handle unavailable')
        status = kernel.WaitForSingleObject(handle, 0)
        if status == 0x102:  # WAIT_TIMEOUT: this exact handle was not signaled.
            result['process'] = {'state': 'pending'}
        elif status == 0:  # WAIT_OBJECT_0: exit259 is still an actual exit.
            result['process'] = {'state': 'exited', 'exit_code': None}
            code = w.DWORD()
            if kernel.GetExitCodeProcess(handle, ctypes.byref(code)):
                result['process']['exit_code'] = code.value
            else:
                result['process'].update(api='GetExitCodeProcess', winerror=ctypes.get_last_error())
        else:
            result['process'].update(api='WaitForSingleObject', result=int(status),
                                     winerror=ctypes.get_last_error())
    except BaseException as error:
        result['process']['error_type'] = type(error).__name__
    return result


def cli_output_snapshot(outputs, evidence, *, joined, returncode, cli_sha256, config_checkpoint=None):
    records, captured = [], []
    for output in outputs:
        if output is not None and not isinstance(output, bytes):
            raise AssertionError('CLI output must be bytes')
        data = output or b''
        size = len(data)
        overflow = size > CLI_OUTPUT_LIMIT
        data = data[:CLI_OUTPUT_LIMIT]; captured.append(data)
        records.append({'available': output is not None, 'observed_size': size if output is not None else None,
                        'overflow': overflow if output is not None else None,
                        'snapshot_sha256': sha(data) if output is not None else None,
                        'base64': base64.b64encode(data).decode('ascii') if output is not None else None})
    artifact = {'schema': 1, 'scope': 'Fresh empty config, no supplied tokens; actual pinned CLI local command only',
                'joined': joined, 'returncode': returncode if joined else None, 'cli_sha256': cli_sha256,
                'capture_limit_scope': 'Saved prefixes only; communicate memory is not bounded by this limit',
                'stdout': records[0], 'stderr': records[1]}
    if config_checkpoint is not None:
        artifact['own_config_checkpoint'] = config_checkpoint
    encoded = json.dumps(artifact, indent=2).encode('utf-8')
    if len(encoded) > LIMIT:
        raise AssertionError('Combined CLI evidence exceeds bound')
    with evidence.open('xb') as output: output.write(encoded)
    summary = {'raw_evidence_sha256': file_sha(evidence), 'raw_evidence_bytes': evidence.stat().st_size,
               'stdout_size': records[0]['observed_size'], 'stderr_size': records[1]['observed_size'],
               'output_overflow': (True if any(row['overflow'] for row in records) else
                                   False if all(row['available'] for row in records) else None),
               'output_complete': joined}
    return captured, summary


def actual_cli_local_phase(binary, identity, folder, project, commands, manifest, receipt, save, evidence):
    config = folder / 'cli-probe-config'
    if os.path.lexists(config) or any(os.path.lexists(project / name) for name in ('.env', '.venv')):
        raise AssertionError('Actual CLI requires a new empty isolated configuration')
    if file_sha(binary) != identity['sha256']:
        raise AssertionError('Existing pinned CLI changed before launch')
    config.mkdir(mode=0o700)
    env = shell_environment()
    env.update(SLACK_DISABLE_TELEMETRY='true', SLACK_SKIP_UPDATE='true', NO_COLOR='1')
    argv = [str(binary), '--config-dir', str(config), '--no-color', '--skip-update', '--verbose',
            'manifest', 'info', '--source', 'local']
    phase = {'action': 'actual_cli_local_manifest', 'status': 'dispatching',
             'cli_sha256': identity['sha256'], 'slack_disable_telemetry_env': True, 'fresh_empty_config': True,
             'call_limit_seconds': CLI_CALL_SECONDS,
             'environment_key_presence': {key: key in {name.upper() for name in env} for key in OS_ENVIRONMENT_KEYS},
             'stdio': 'pipe_stdin_stdout_stderr', 'capture_limit_scope': 'Saved prefixes only; live communicate memory is not capped'}
    receipt['phases'].append(phase); save()
    started = time.monotonic()
    communicating = False
    try:
        child = subprocess.Popen(argv, cwd=project, env=env, stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 creationflags=subprocess.CREATE_NO_WINDOW if os.name == 'nt' else 0)
        phase.update(child_identity(child)); save()
        # Match production's empty-input/piped-output boundary. Unlike run(),
        # communicate() leaves a timed-out child untouched; do not retry or kill.
        communicating = True
        outputs = child.communicate(input=b'', timeout=CLI_CALL_SECONDS)
    except BaseException as error:
        phase.update(status='unknown_no_retry_no_kill', error_type=type(error).__name__, **cli_elapsed(started))
        if communicating:
            try:
                phase['exception_observation'] = cli_exception_observation(child, phase)
            except BaseException as capture_error:
                phase['exception_observation'] = {'status': 'unavailable', 'error_type': type(capture_error).__name__}
        try:
            checkpoint = cli_config_checkpoint(config)
        except BaseException as capture_error:
            checkpoint = {'status': 'unavailable', 'error_type': type(capture_error).__name__}
        try:
            partial = (getattr(error, 'output', None), getattr(error, 'stderr', None))
            _, summary = cli_output_snapshot(partial, evidence, joined=False, returncode=None,
                                             cli_sha256=identity['sha256'], config_checkpoint=checkpoint)
            phase.update(summary); save()
        except BaseException as capture_error:
            phase['capture_error_type'] = type(capture_error).__name__
        raise
    phase.update(cli_elapsed(started))
    captured, summary = cli_output_snapshot(outputs, evidence, joined=True, returncode=child.returncode, cli_sha256=identity['sha256'])
    phase.update(summary, status='returned', returncode=child.returncode); save()
    if child.returncode or summary['output_overflow']:
        raise AssertionError('Actual CLI failed or output exceeded bound')
    phase.update(cli_hook_proof(*captured, commands, manifest, project))
    if json.loads(read_small(config / 'credentials.json')) != {}:
        raise AssertionError('Actual CLI did not preserve an empty auth configuration')
    phase['empty_auth_preserved'] = True; save()


def run(home, helper_path, helper_sha256, result):
    receipt = {'schema': 1, 'passed': False, 'phases': [], 'scope': 'Actual pinned Go CLI local GetHooks/GetManifest; separate fake-only private-Python install body, not auth/deploy',
               'explicit_slack_requests': 0, 'fake_install_only': True,
               'shell_profile_effects': 'Native shell uses upstream default flags; ambient profile effects are not independently audited.',
               'windows_parent_scope': 'Actual CLI uses production CREATE_NO_WINDOW; Go owns native hook pipes and default shell flags. Fake install body is separate.'}
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
                       setup_cli=setup_cli_record(helper_path.parent / 'slack-setup', helper))
        observer.start()
        commands = {action: helper.hook_command(project, action) for action in ('hooks', 'manifest', 'install')}
        binary = helper_path.parent / 'slack-setup/bin' / (helper.WINDOWS_CLI_BINARY if os.name == 'nt' else 'slack')
        actual_cli_local_phase(binary, receipt['setup_cli'], folder, project, commands, manifest,
                               receipt, save, result.with_name('native-hook-cli-evidence.json'))
        for action in ('install',):
            command = helper.hook_command(project, action)
            argv = [str(expected), '-I', '-B', '-X', 'utf8', str(project / 'hooks.py'), action]
            env = shell_environment()
            env.update(SLACK_APP_TOKEN=FAKE['app_token'], SLACK_BOT_TOKEN=FAKE['bot_token'])
            phase = {'action': action, 'status': 'dispatching', 'command_sha256': sha(command.encode()),
                     'scope': 'Direct private-Python fake install body; not native shell or CLI deploy',
                     'python_pe_subsystem': pe_subsystem(expected)}
            receipt['phases'].append(phase); save()
            # This fake install body has a tiny known response. Files avoid unbounded pipe
            # buffering if a shell fails unexpectedly. They remain with this
            # fixture on uncertainty; no process is signalled or replayed.
            stdout_path = folder / (action + '-stdout')
            stderr_path = folder / (action + '-stderr')
            with stdout_path.open('xb') as out, stderr_path.open('xb') as err:
                child = subprocess.Popen(argv, cwd=project, env=env, stdin=subprocess.DEVNULL,
                                         stdout=out, stderr=err,
                                         creationflags=subprocess.CREATE_NO_WINDOW if os.name == 'nt' else 0)
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
            if json.loads(read_small(folder / 'issued.json')) != FAKE:
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
        if isinstance(error, json.JSONDecodeError):
            receipt['failure_detail'] = {'message': error.msg[:160], 'line': error.lineno,
                                         'column': error.colno, 'position': error.pos}
        elif isinstance(error, AssertionError):
            receipt['failure_detail'] = {'message': str(error)[:160]}
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
    @staticmethod
    def cli_transcript(project):
        commands = {'hooks': 'private-python hooks', 'manifest': 'private-python manifest', 'install': 'private-python install'}
        manifest = {'display_information': {'name': 'Local hook contract fixture'}}
        source = str(project).replace('"', '`"' if os.name == 'nt' else '\\"')
        spans = [(commands['hooks'], {'runtime': 'rust', 'hooks': {'get-manifest': commands['manifest'], 'deploy': commands['install']}}),
                 (commands['manifest'] + ' --source="' + source + '"', manifest)]
        lines = []
        for command, value in spans:
            lines.extend(['starting hook command: ' + command, json.dumps(value), 'finished hook command: ' + command])
        output = ''.join('[2026-11-19 01:02:03] ' + line + '\n' for line in lines) + json.dumps(manifest) + '\n'
        return commands, manifest, output.encode('utf-8')

    def test_actual_cli_ordered_hooks_require_exact_returned_json(self):
        with tempfile.TemporaryDirectory() as temporary:
            project = Path(temporary).resolve()
            commands, manifest, output = self.cli_transcript(project)
            self.assertEqual(cli_hook_proof(output, b'', commands, manifest, project),
                             {'actual_go_cli_hooks': ['hooks', 'manifest'], 'file_fallback_accepted': False})
            self.assertFalse(cli_command_matches(commands['manifest'] + ' --source="' + str(project.parent) + '"', commands['manifest'], project))
            for changed in [output.replace(b'private-python hooks', b'foreign hooks'),
                            output.replace(b'"runtime": "rust"', b'"runtime": "foreign"'),
                            output + output, output[:80], b'x' * (CLI_OUTPUT_LIMIT + 1)]:
                with self.subTest(changed=sha(changed)),self.assertRaises((AssertionError, ValueError)):
                    cli_hook_proof(changed, b'', commands, manifest, project)

    def test_actual_cli_exit_zero_file_fallback_and_deferred_finish_are_not_proof(self):
        with tempfile.TemporaryDirectory() as temporary:
            project = Path(temporary).resolve()
            commands, manifest, output = self.cli_transcript(project)
            fallback = json.dumps(manifest).encode()
            deferred = ('[2026-11-19 01:02:03] starting hook command: ' + commands['hooks'] + '\n' +
                        '[2026-11-19 01:02:03] finished hook command: ' + commands['hooks'] + '\n').encode() + fallback
            for value in (fallback, deferred, output.replace(b'starting hook command:', b'ignored hook command:', 1)):
                with self.assertRaises((AssertionError, ValueError)):
                    cli_hook_proof(value, b'', commands, manifest, project)
            with self.assertRaisesRegex(AssertionError, 'initialization error'):
                cli_hook_proof(output, b'failed to initialize hook configurations: failure', commands, manifest, project)

    def test_actual_cli_raw_snapshot_is_bounded_and_never_overwrites_original(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve(); outputs = [b'x' * (CLI_OUTPUT_LIMIT + 1), b'']; evidence = root/'evidence.json'
            captured, summary = cli_output_snapshot(outputs, evidence, joined=False, returncode=None, cli_sha256='a'*64)
            self.assertEqual(len(captured[0]), CLI_OUTPUT_LIMIT); self.assertTrue(summary['output_overflow'])
            self.assertFalse(summary['output_complete']); original = evidence.read_bytes()
            saved = json.loads(original); self.assertEqual(base64.b64decode(saved['stdout']['base64']), captured[0])
            self.assertIsNone(saved['returncode']); self.assertLess(len(original), LIMIT)
            with self.assertRaises(FileExistsError):
                cli_output_snapshot(outputs, evidence, joined=True, returncode=0, cli_sha256='a'*64)
            self.assertEqual(evidence.read_bytes(), original)

    def test_actual_cli_fresh_config_clean_environment_and_one_dispatch(self):
        from unittest.mock import Mock
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary).resolve(); project=root/'project';project.mkdir();binary=root/'slack';binary.write_bytes(b'inert')
            commands, manifest, output = self.cli_transcript(project)
            child=Mock(returncode=0, pid=42)
            child.communicate.return_value=(output,b'')
            def launch(argv, **kwargs):
                self.assertEqual(argv[-4:], ['manifest','info','--source','local'])
                self.assertEqual(kwargs['cwd'], project)
                self.assertEqual(kwargs['env']['SLACK_DISABLE_TELEMETRY'], 'true')
                self.assertEqual(kwargs['env']['SLACK_SKIP_UPDATE'], 'true')
                for forbidden in ('SLACK_TOKEN','SLACK_CONFIG_DIR','OPENAI_API_KEY','DENO_AUTH_TOKENS'):
                    self.assertNotIn(forbidden, kwargs['env'])
                for name in ('stdin','stdout','stderr'):
                    self.assertEqual(kwargs[name], subprocess.PIPE)
                (Path(argv[2])/'credentials.json').write_text('{}')
                return child
            receipt={'phases':[]};save=Mock()
            with patch.dict(os.environ, {'SLACK_TOKEN':'never-use','SLACK_CONFIG_DIR':'never-use','OPENAI_API_KEY':'never-use','DENO_AUTH_TOKENS':'never-use'}), \
                    patch.object(subprocess,'Popen',side_effect=launch) as popen, \
                    patch(__name__+'.child_identity',return_value={'pid':42}), \
                    patch.object(time,'monotonic',side_effect=[100,100.125]):
                actual_cli_local_phase(binary, {'sha256':file_sha(binary)}, root, project, commands, manifest, receipt, save, root/'evidence.json')
            self.assertEqual(popen.call_count,1);child.communicate.assert_called_once_with(input=b'',timeout=120)
            child.kill.assert_not_called();child.terminate.assert_not_called()
            phase=receipt['phases'][0]
            self.assertTrue(phase['empty_auth_preserved'])
            self.assertEqual(phase['call_limit_seconds'],120)
            self.assertEqual(phase['elapsed_ms'],125);self.assertFalse(phase['elapsed_capped'])
            self.assertEqual(set(phase['environment_key_presence']),set(OS_ENVIRONMENT_KEYS))
            self.assertTrue(all(type(value) is bool for value in phase['environment_key_presence'].values()))
            self.assertNotIn('never-use',json.dumps(receipt))

    def test_actual_cli_unknown_preserves_original_timeout_without_kill_or_retry(self):
        from unittest.mock import Mock
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary).resolve();project=root/'project';project.mkdir();binary=root/'slack';binary.write_bytes(b'inert')
            commands,manifest,_=self.cli_transcript(project)
            original=subprocess.TimeoutExpired('fixed-local-cli',120,output=b'partial',stderr=None)
            child=Mock(returncode=None,pid=42);child.communicate.side_effect=original
            receipt={'phases':[]}
            with patch.object(subprocess,'Popen',return_value=child) as popen, \
                    patch(__name__+'.child_identity',return_value={'pid':42}), \
                    patch.object(time,'monotonic',side_effect=[100,220.025]):
                with self.assertRaises(subprocess.TimeoutExpired) as caught:
                    actual_cli_local_phase(binary, {'sha256':file_sha(binary)}, root, project, commands, manifest, receipt, lambda:None, root/'evidence.json')
            self.assertIs(caught.exception,original);self.assertEqual(popen.call_count,1)
            child.communicate.assert_called_once_with(input=b'',timeout=120)
            child.wait.assert_not_called()
            child.kill.assert_not_called();child.terminate.assert_not_called()
            self.assertEqual(receipt['phases'][0]['status'],'unknown_no_retry_no_kill')
            self.assertEqual(receipt['phases'][0]['elapsed_ms'],120025)
            self.assertEqual(receipt['phases'][0]['call_limit_seconds'],120)
            saved=json.loads((root/'evidence.json').read_bytes())
            self.assertFalse(saved['joined'])
            self.assertEqual(base64.b64decode(saved['stdout']['base64']),b'partial')
            self.assertFalse(saved['stderr']['available'])
            for field in ('observed_size','overflow','snapshot_sha256','base64'):
                self.assertIsNone(saved['stderr'][field])
            self.assertIsNone(receipt['phases'][0]['stderr_size'])
            self.assertEqual(saved['own_config_checkpoint']['logs'],[])

    def test_own_config_checkpoint_bounds_two_logs_and_never_reads_auth(self):
        with tempfile.TemporaryDirectory() as temporary:
            config=Path(temporary).resolve()/'cli-probe-config';config.mkdir()
            (config/'credentials.json').write_text('private-content-not-to-read')
            (config/'config.json').write_text('configuration-content-not-to-read')
            logs=config/'logs';logs.mkdir()
            for day in ('20261118','20261119'):
                (logs/('slack-debug-'+day+'.log')).write_bytes(b'x'*(CLI_OUTPUT_LIMIT+1))
            original_open=Path.open
            original_os_open=os.open
            def guarded_open(path,*args,**kwargs):
                self.assertNotIn(path.name,('config.json','credentials.json'))
                return original_open(path,*args,**kwargs)
            def guarded_os_open(path,*args,**kwargs):
                self.assertNotIn(Path(path).name,('config.json','credentials.json'))
                return original_os_open(path,*args,**kwargs)
            with patch.object(Path,'open',guarded_open),patch.object(os,'open',guarded_os_open):
                checkpoint=cli_config_checkpoint(config)
            self.assertEqual([row['name'] for row in checkpoint['files']],['config.json','credentials.json'])
            for row in checkpoint['files']:
                self.assertEqual(set(row),{'name','exists','type','size','identity'})
                self.assertTrue(row['exists']);self.assertEqual(row['type'],'regular')
                info=(config/row['name']).lstat()
                self.assertEqual(row['size'],info.st_size)
                self.assertEqual(row['identity'],{'device':info.st_dev,'inode':info.st_ino})
            self.assertEqual(len(checkpoint['logs']),2)
            for row in checkpoint['logs']:
                self.assertTrue(row['overflow'])
                self.assertEqual(len(base64.b64decode(row['base64'])),CLI_OUTPUT_LIMIT)
            evidence=config.parent/'evidence.json'
            cli_output_snapshot([b'x'*(CLI_OUTPUT_LIMIT+1)]*2,evidence,joined=False,
                                returncode=None,cli_sha256='a'*64,config_checkpoint=checkpoint)
            self.assertLess(evidence.stat().st_size,LIMIT)
            self.assertNotIn('private-content-not-to-read',evidence.read_text())
            self.assertNotIn('configuration-content-not-to-read',evidence.read_text())
            (logs/'slack-debug-20261120.log').write_bytes(b'')
            with self.assertRaisesRegex(AssertionError,'inventory differs'):
                cli_config_checkpoint(config)

    def test_own_config_checkpoint_records_fixed_metadata_before_logs_exist(self):
        with tempfile.TemporaryDirectory() as temporary:
            config=Path(temporary).resolve()/'cli-probe-config';config.mkdir()
            checkpoint=cli_config_checkpoint(config)
            self.assertFalse(checkpoint['logs_exists'])
            self.assertEqual(checkpoint['files'],[{'name':'config.json','exists':False},
                                                 {'name':'credentials.json','exists':False}])
            (config/'config.json').mkdir()
            if os.name != 'nt':
                (config/'credentials.json').symlink_to(config.parent/'missing-outside')
            with patch.object(Path,'open',side_effect=AssertionError('No file reads')), \
                    patch.object(os,'open',side_effect=AssertionError('No file reads')):
                checkpoint=cli_config_checkpoint(config)
            self.assertEqual(checkpoint['files'][0]['type'],'directory')
            if os.name != 'nt':self.assertEqual(checkpoint['files'][1]['type'],'symlink')
            self.assertEqual(checkpoint['logs'],[])

    def test_elapsed_diagnostic_is_bounded_and_failure_is_not_a_new_error(self):
        with patch.object(time,'monotonic',return_value=100_000):
            self.assertEqual(cli_elapsed(0),{'elapsed_ms':86_400_000,'elapsed_capped':True})
        with patch.object(time,'monotonic',side_effect=OSError('unavailable')):
            self.assertEqual(cli_elapsed(0),{'elapsed_ms':None,'elapsed_error_type':'OSError'})

    def test_exception_observer_pending_uses_one_owned_handle_probe_and_thread_flags(self):
        from types import SimpleNamespace
        from unittest.mock import Mock
        stdin=Mock();stdin.is_alive.return_value=False
        stdout=Mock();stdout.is_alive.return_value=True
        child=SimpleNamespace(pid=42,_handle=73,_stdin_thread=stdin,stdout_thread=stdout)
        kernel=SimpleNamespace(WaitForSingleObject=Mock(return_value=0x102),GetExitCodeProcess=Mock())
        with patch.object(ctypes,'WinDLL',return_value=kernel,create=True):
            result=cli_exception_observation(child,{'pid':42,'birth':123},True)
        kernel.WaitForSingleObject.assert_called_once_with(73,0)
        kernel.GetExitCodeProcess.assert_not_called()
        stdin.is_alive.assert_called_once_with();stdout.is_alive.assert_called_once_with()
        self.assertEqual(result['process'],{'state':'pending'})
        self.assertEqual(result['identity'],{'pid':42,'birth':123,'binding':'initial_owned_child_identity'})
        self.assertEqual(result['threads'],{'_stdin_thread':{'present':True,'alive':False},
                         'stdout_thread':{'present':True,'alive':True},'stderr_thread':{'present':False,'alive':None}})

    def test_exception_observer_signaled_exit259_is_not_pending(self):
        from types import SimpleNamespace
        from unittest.mock import Mock
        events=[]
        def signaled(handle,timeout):events.append(('signal',handle,timeout));return 0
        def exited(handle,code):events.append(('code',handle));code._obj.value=259;return 1
        kernel=SimpleNamespace(WaitForSingleObject=Mock(side_effect=signaled),GetExitCodeProcess=Mock(side_effect=exited))
        with patch.object(ctypes,'WinDLL',return_value=kernel,create=True):
            result=cli_exception_observation(SimpleNamespace(pid=42,_handle=73),{'pid':42,'birth':123},True)
        self.assertEqual(events,[('signal',73,0),('code',73)])
        self.assertEqual(result['process'],{'state':'exited','exit_code':259})

    def test_exception_observer_failed_wait_and_thread_query_stay_unavailable(self):
        from types import SimpleNamespace
        from unittest.mock import Mock
        thread=Mock();thread.is_alive.side_effect=OSError('must-not-be-serialized')
        kernel=SimpleNamespace(WaitForSingleObject=Mock(return_value=0xffffffff),GetExitCodeProcess=Mock())
        with patch.object(ctypes,'WinDLL',return_value=kernel,create=True), \
                patch.object(ctypes,'get_last_error',return_value=5,create=True):
            result=cli_exception_observation(SimpleNamespace(pid=42,_handle=73,stdout_thread=thread),{'pid':42,'birth':123},True)
        kernel.WaitForSingleObject.assert_called_once_with(73,0);kernel.GetExitCodeProcess.assert_not_called()
        self.assertEqual(result['process'],{'state':'unavailable','api':'WaitForSingleObject','result':0xffffffff,'winerror':5})
        self.assertEqual(result['threads']['stdout_thread'],{'present':True,'alive':None,'error_type':'OSError'})
        self.assertNotIn('must-not-be-serialized',json.dumps(result))

    def test_exception_observer_signaled_without_exit_code_preserves_signal_proof(self):
        from types import SimpleNamespace
        from unittest.mock import Mock
        kernel=SimpleNamespace(WaitForSingleObject=Mock(return_value=0),GetExitCodeProcess=Mock(return_value=0))
        with patch.object(ctypes,'WinDLL',return_value=kernel,create=True), \
                patch.object(ctypes,'get_last_error',return_value=5,create=True):
            result=cli_exception_observation(SimpleNamespace(pid=42,_handle=73),{'pid':42,'birth':123},True)
        kernel.WaitForSingleObject.assert_called_once_with(73,0)
        self.assertEqual(kernel.GetExitCodeProcess.call_count,1)
        self.assertEqual(result['process'],{'state':'exited','exit_code':None,'api':'GetExitCodeProcess','winerror':5})

    def test_exception_observer_requires_initial_identity_and_never_opens_a_handle(self):
        from types import SimpleNamespace
        with patch.object(ctypes,'WinDLL',side_effect=AssertionError('Must not query'),create=True) as library:
            for phase in ({'pid':43,'birth':123},{'pid':42}):
                result=cli_exception_observation(SimpleNamespace(pid=42,_handle=73),phase,True)
                self.assertEqual(result['process'],{'state':'unavailable','error_type':'ValueError'})
            library.assert_not_called()

    def test_exception_observation_failure_does_not_mask_original_or_settle_output(self):
        from unittest.mock import Mock
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary).resolve();project=root/'project';project.mkdir();binary=root/'slack';binary.write_bytes(b'inert')
            commands,manifest,_=self.cli_transcript(project)
            original=subprocess.TimeoutExpired('fixed-local-cli',120)
            child=Mock(returncode=None,pid=42);child.communicate.side_effect=original
            receipt={'phases':[]}
            with patch.object(subprocess,'Popen',return_value=child) as popen, \
                    patch(__name__+'.child_identity',return_value={'pid':42,'birth':123}), \
                    patch(__name__+'.cli_exception_observation',side_effect=RuntimeError('must-not-be-serialized')) as observation:
                with self.assertRaises(subprocess.TimeoutExpired) as caught:
                    actual_cli_local_phase(binary,{'sha256':file_sha(binary)},root,project,commands,manifest,receipt,lambda:None,root/'evidence.json')
            self.assertIs(caught.exception,original);self.assertEqual(popen.call_count,1)
            observation.assert_called_once();child.communicate.assert_called_once_with(input=b'',timeout=120)
            child.poll.assert_not_called();child.wait.assert_not_called();child.kill.assert_not_called();child.terminate.assert_not_called()
            phase=receipt['phases'][0]
            self.assertEqual(phase['status'],'unknown_no_retry_no_kill')
            self.assertEqual(phase['exception_observation'],{'status':'unavailable','error_type':'RuntimeError'})
            self.assertIsNone(phase['stdout_size']);self.assertIsNone(phase['stderr_size'])
            self.assertNotIn('must-not-be-serialized',json.dumps(receipt))

    def test_launch_refusal_does_not_observe_uncreated_child(self):
        from unittest.mock import Mock
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary).resolve();project=root/'project';project.mkdir();binary=root/'slack';binary.write_bytes(b'inert')
            commands,manifest,_=self.cli_transcript(project);original=OSError('launch refused')
            with patch.object(subprocess,'Popen',side_effect=original) as popen, \
                    patch(__name__+'.cli_exception_observation') as observation:
                with self.assertRaises(OSError) as caught:
                    actual_cli_local_phase(binary,{'sha256':file_sha(binary)},root,project,commands,manifest,{'phases':[]},lambda:None,root/'evidence.json')
            self.assertIs(caught.exception,original);self.assertEqual(popen.call_count,1);observation.assert_not_called()

    def test_own_config_checkpoint_refuses_unexpected_or_linked_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary).resolve();config=root/'cli-probe-config';config.mkdir();logs=config/'logs';logs.mkdir()
            unexpected=logs/'credentials.json';unexpected.write_text('must-not-read')
            with self.assertRaisesRegex(AssertionError,'inventory differs'):
                cli_config_checkpoint(config)
            unexpected.unlink()
            if os.name != 'nt':
                outside=root/'outside';outside.write_bytes(b'must-not-read')
                (logs/'slack-debug-20261119.log').symlink_to(outside)
                with self.assertRaisesRegex(AssertionError,'Linked'):
                    cli_config_checkpoint(config)

    def test_timeout_checkpoint_failure_preserves_exception_and_unknown(self):
        from unittest.mock import Mock
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary).resolve();project=root/'project';project.mkdir();binary=root/'slack';binary.write_bytes(b'inert')
            commands,manifest,_=self.cli_transcript(project)
            original=subprocess.TimeoutExpired('fixed-local-cli',120)
            child=Mock(returncode=None,pid=42);child.communicate.side_effect=original
            receipt={'phases':[]}
            with patch.object(subprocess,'Popen',return_value=child) as popen, \
                    patch(__name__+'.child_identity',return_value={'pid':42}), \
                    patch(__name__+'.cli_config_checkpoint',side_effect=OSError('unavailable')):
                with self.assertRaises(subprocess.TimeoutExpired) as caught:
                    actual_cli_local_phase(binary,{'sha256':file_sha(binary)},root,project,commands,manifest,receipt,lambda:None,root/'evidence.json')
            self.assertIs(caught.exception,original);self.assertEqual(popen.call_count,1)
            child.communicate.assert_called_once_with(input=b'',timeout=120)
            child.wait.assert_not_called();child.kill.assert_not_called();child.terminate.assert_not_called()
            saved=json.loads((root/'evidence.json').read_bytes())
            self.assertEqual(saved['own_config_checkpoint'],{'status':'unavailable','error_type':'OSError'})
            self.assertIsNone(saved['stdout']['observed_size']);self.assertIsNone(saved['stderr']['observed_size'])

    def test_joined_overflow_never_becomes_hook_success(self):
        from unittest.mock import Mock
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary).resolve();project=root/'project';project.mkdir();binary=root/'slack';binary.write_bytes(b'inert')
            commands,manifest,_=self.cli_transcript(project)
            child=Mock(returncode=0,pid=42);child.communicate.return_value=(b'x'*(CLI_OUTPUT_LIMIT+1),b'')
            receipt={'phases':[]}
            with patch.object(subprocess,'Popen',return_value=child), \
                    patch(__name__+'.child_identity',return_value={'pid':42}), \
                    patch(__name__+'.cli_hook_proof') as proof:
                with self.assertRaisesRegex(AssertionError,'output exceeded'):
                    actual_cli_local_phase(binary,{'sha256':file_sha(binary)},root,project,commands,manifest,receipt,lambda:None,root/'evidence.json')
                proof.assert_not_called()
            self.assertTrue(receipt['phases'][0]['output_overflow'])

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

    def test_standard_windows_context_is_retained_without_broad_environment_copy(self):
        expected={'APPDATA':'fixture-appdata','LOCALAPPDATA':'fixture-localappdata',
                  'COMSPEC':'fixture-comspec','PATHEXT':'.COM;.EXE','COMPUTERNAME':'fixture-machine'}
        injected={**expected,'SystemRoot':'C:/Windows','UNKNOWN_CONTEXT':'do-not-copy',
                  'SLACK_CONFIG_DIR':'do-not-copy','SLACK_TOKEN':'do-not-copy','PATH':'/developer'}
        with patch.dict(os.environ,injected,clear=True):
            env=shell_environment()
        for key,value in expected.items():self.assertEqual(env[key],value)
        self.assertFalse({'UNKNOWN_CONTEXT','SLACK_CONFIG_DIR','SLACK_TOKEN'} & set(env))
        self.assertNotIn('/developer',env['PATH'])
        with patch.dict(os.environ,{'SystemRoot':'C:/Windows'},clear=True):
            env=shell_environment()
        self.assertTrue(all(key not in env for key in expected))

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

    def test_stock_cli_record_remains_official_non_windows_without_execution(self):
        with tempfile.TemporaryDirectory() as temporary:
            folder = Path(temporary).resolve()
            (folder / 'bin').mkdir()
            data=b'fixed stock tool';binary=folder/'bin/slack';binary.write_bytes(data)
            receipt = {'version': '4.12.0', 'sha256': sha(data)}
            (folder / 'cli.json').write_text(json.dumps(receipt))
            with patch.object(subprocess, 'Popen', side_effect=AssertionError('No native launch')) as launch:
                actual = setup_cli_record(folder, windows=False)
                self.assertEqual(actual['kind'],'official_upstream');self.assertIsNone(actual['pe_subsystem'])
                self.assertEqual(actual['sha256'], receipt['sha256'])
                self.assertFalse(actual['version_command_repeated'])
                binary.write_bytes(data + b'changed')
                with self.assertRaisesRegex(AssertionError, 'identity differs'):
                    setup_cli_record(folder, windows=False)
                launch.assert_not_called()

    @staticmethod
    def derived_cli_fixture(root):
        from types import SimpleNamespace
        from unittest.mock import Mock
        folder=root/'cache';(folder/'bin').mkdir(parents=True)
        private=root/'runtime/python';(private/'slack-cli').mkdir(parents=True)
        (private/'python.exe').write_bytes(b'inert private interpreter')
        data=bytearray(64+94);data[:2]=b'MZ';struct.pack_into('<I',data,60,64)
        data[64:68]=b'PE\0\0';struct.pack_into('<H',data,64+24,0x20b);struct.pack_into('<H',data,64+24+68,3)
        data=bytes(data)
        expected={'version':'4.12.0','distribution':'constellation-headless-1','upstream_source':'1'*40,
                  'patch_sha256':'2'*64,'sha256':sha(data),'size':len(data)}
        helper=SimpleNamespace(WINDOWS_CLI_BINARY='slack-headless-1.exe',WINDOWS_CLI_RECEIPT='cli-headless-1.json',
                               bundled_windows_cli=Mock(return_value=(data,expected)))
        binary=folder/'bin'/helper.WINDOWS_CLI_BINARY;binary.write_bytes(data)
        (folder/helper.WINDOWS_CLI_RECEIPT).write_text(json.dumps(expected))
        (folder/'bin/slack.exe').write_bytes(b'preserved old official tool')
        (folder/'cli.json').write_bytes(b'preserved old receipt; never read by new fixture')
        provenance={'distribution':expected['distribution'],'upstream_version':expected['version'],
                    'upstream_source':expected['upstream_source'],'patch_sha256':expected['patch_sha256'],
                    'goos':'windows','goarch':'amd64','cgo_enabled':False,'binary_size':len(data),'binary_sha256':sha(data)}
        provenance_raw=json.dumps(provenance).encode();(private/'slack-cli/provenance.json').write_bytes(provenance_raw)
        (private/'slack-cli/slack.exe').write_bytes(data)
        bundle={'slack_cli':provenance,'python_files':{'slack-cli/provenance.json':sha(provenance_raw),'slack-cli/slack.exe':sha(data)}}
        (private.parent/'native-bundle.json').write_text(json.dumps(bundle))
        return folder,private,helper,expected,binary,bundle

    def test_derived_cli_record_binds_bundle_receipt_and_preserves_official_cache(self):
        with tempfile.TemporaryDirectory() as temporary:
            folder,private,helper,expected,binary,bundle=self.derived_cli_fixture(Path(temporary).resolve())
            before={path:path.read_bytes() for path in (folder/'bin/slack.exe',folder/'cli.json')}
            with patch.object(sys,'executable',str(private/'python.exe')), \
                    patch.object(subprocess,'Popen',side_effect=AssertionError('No native launch')) as launch:
                actual=setup_cli_record(folder,helper,True)
            self.assertEqual(actual['kind'],'constellation_derived');self.assertEqual(actual['pe_subsystem'],3)
            for key in ('version','distribution','upstream_source','patch_sha256','sha256','size'):
                self.assertEqual(actual[key],expected[key])
            self.assertEqual(actual['bundled_provenance_sha256'],bundle['python_files']['slack-cli/provenance.json'])
            helper.bundled_windows_cli.assert_called_once_with();launch.assert_not_called()
            self.assertFalse(actual['version_command_repeated'])
            for path,data in before.items():self.assertEqual(path.read_bytes(),data)

    def test_derived_cli_record_refuses_old_or_changed_receipt_and_inventory(self):
        with tempfile.TemporaryDirectory() as temporary:
            folder,private,helper,expected,binary,bundle=self.derived_cli_fixture(Path(temporary).resolve())
            receipt_path=folder/helper.WINDOWS_CLI_RECEIPT
            with patch.object(sys,'executable',str(private/'python.exe')):
                for changed in ({'version':'4.12.0','sha256':expected['sha256']},
                                {**expected,'upstream_source':'3'*40},{**expected,'patch_sha256':'4'*64}):
                    receipt_path.write_text(json.dumps(changed))
                    with self.assertRaisesRegex(AssertionError,'identity differs'):setup_cli_record(folder,helper,True)
                receipt_path.write_text(json.dumps(expected))
                for changed in ({**bundle,'slack_cli':{}},{**bundle,'python_files':{}},
                                {**bundle,'python_files':{**bundle['python_files'],'slack-cli/slack.exe':'0'*64}}):
                    (private.parent/'native-bundle.json').write_text(json.dumps(changed))
                    with self.assertRaisesRegex(AssertionError,'identity differs'):setup_cli_record(folder,helper,True)
                (private.parent/'native-bundle.json').write_text(json.dumps(bundle))
                binary.write_bytes(b'changed')
                with self.assertRaisesRegex(AssertionError,'identity differs'):setup_cli_record(folder,helper,True)
                receipt_path.unlink()
                with self.assertRaises(FileNotFoundError):setup_cli_record(folder,helper,True)
            self.assertEqual((folder/'bin/slack.exe').read_bytes(),b'preserved old official tool')

    def test_native_version_probe_hides_windows_cli_and_labels_derived_source(self):
        import ast
        import contextlib
        from types import SimpleNamespace
        tree=ast.parse(Path(__file__).with_name('native_launch.py').read_text(encoding='utf-8'))
        code=next(node.value.value for node in ast.walk(tree) if isinstance(node,ast.Assign)
                  and any(isinstance(target,ast.Name) and target.id=='code' for target in node.targets)
                  and isinstance(node.value,ast.Constant) and isinstance(node.value.value,str)
                  and 'm.ensure_cli' in node.value.value)
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary).resolve();(root/'secrets').mkdir()
            (root/'secrets/slack-setup-helper.py').write_text("WINDOWS_CLI_DISTRIBUTION='constellation-headless-1'\ndef ensure_cli(folder): return folder/'bin/slack-headless-1.exe'\n")
            for platform in ('win32','linux'):
                with patch.object(sys,'argv',['fixture',str(root)]),patch.object(sys,'platform',platform), \
                        patch.dict(os.environ,{'NO_COLOR':'','SLACK_SKIP_UPDATE':'false'}), \
                        patch.object(subprocess,'CREATE_NO_WINDOW',0x08000000,create=True), \
                        patch.object(subprocess,'run',return_value=SimpleNamespace(returncode=0,stdout='4.12.0')) as child, \
                        contextlib.redirect_stdout(__import__('io').StringIO()) as output:
                    exec(compile(code,'<inert version fixture>','exec'),{})
                self.assertEqual(child.call_count,1)
                self.assertEqual(child.call_args.kwargs['creationflags'],0x08000000 if platform=='win32' else 0)
                self.assertEqual(child.call_args.kwargs['timeout'],30)
                self.assertEqual(child.call_args.kwargs['env']['NO_COLOR'],'1')
                self.assertEqual(child.call_args.kwargs['env']['SLACK_SKIP_UPDATE'],'true')
                self.assertIn('constellation-headless-1' if platform=='win32' else 'official upstream',output.getvalue())


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
