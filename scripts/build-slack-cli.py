#!/usr/bin/env python3
"""Build the explicitly identified Windows Slack CLI derivative during CI only.

This does not run the resulting executable, install a toolchain, or run at user
setup. Failed build directories are retained; an output directory is never reused.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import struct
import subprocess
import tarfile
import urllib.request


ROOT = Path(__file__).resolve().parents[1]
VENDOR = ROOT / 'vendor' / 'slack-cli'
SOURCE = '91df4803911140c5a6da56e88ecd04e25c9c7515'
MAX_ARCHIVE = 40 * 1024 * 1024
MAX_EXPANDED = 16 * 1024 * 1024
MAX_BINARY = 150 * 1024 * 1024


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def load_lock(vendor=VENDOR):
    data = (vendor / 'source-lock.json').read_bytes()
    lock = json.loads(data)
    expected = {
        'schema': 1, 'distribution': 'constellation-headless-1',
        'upstream_repository': 'slackapi/slack-cli', 'upstream_source': SOURCE,
        'upstream_version': '4.12.0', 'go_version': 'go1.27.3',
        'goos': 'windows', 'goarch': 'amd64', 'goamd64': 'v1',
        'cgo_enabled': False, 'patch_file': 'windows-no-color.patch',
        'patched_file': 'internal/style/colors.go',
        'source_archive_root': 'slack-cli-' + SOURCE,
        'source_archive_url': 'https://codeload.github.com/slackapi/slack-cli/tar.gz/' + SOURCE,
        'build_flags': ['-mod=readonly', '-trimpath', '-buildvcs=false', '-ldflags',
                        '-s -w -X github.com/slackapi/slack-cli/internal/version.Version=v4.12.0'],
    }
    for key, value in expected.items():
        require(type(lock.get(key)) is type(value) and lock[key] == value,
                'Unexpected locked build field: ' + key)
    require(type(lock.get('source_archive_size')) is int and
            0 < lock['source_archive_size'] <= MAX_ARCHIVE, 'Invalid locked archive size')
    for key in ('source_archive_sha256', 'patch_sha256', 'patched_file_sha256'):
        require(re.fullmatch('[0-9a-f]{64}', lock.get(key, '')) is not None,
                'Invalid locked digest: ' + key)
    expected_files = {'internal/style/colors.go', 'go.mod', 'go.sum', 'LICENSE',
                      '.goreleaser.yml', '.circleci/config.yml', 'Makefile'}
    require(set(lock['source_files']) == expected_files, 'Unexpected source hash inventory')
    for value in lock['source_files'].values():
        require(re.fullmatch('[0-9a-f]{64}', value) is not None, 'Invalid source file digest')
    require(sha256((vendor / lock['patch_file']).read_bytes()) == lock['patch_sha256'],
            'Patch digest mismatch')
    require(sha256((vendor / 'LICENSE').read_bytes()) == lock['source_files']['LICENSE'],
            'Retained upstream license mismatch')
    return lock, sha256(data)


def regular_file(path, maximum):
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode) and not path.is_symlink(), 'Expected ordinary file')
    require(0 < info.st_size <= maximum, 'File size outside bound')
    return info.st_size


def acquire_source(destination, lock, supplied=None):
    # Both supplied and downloaded archives pass the same exact byte checks.
    if supplied is not None:
        size = regular_file(supplied, MAX_ARCHIVE)
        with supplied.open('rb') as src, destination.open('xb') as dst:
            total = 0
            while block := src.read(1024 * 1024):
                total += len(block)
                require(total <= lock['source_archive_size'], 'Source copy exceeds locked size')
                dst.write(block)
        require(destination.stat().st_size == size, 'Source changed while copied')
    else:
        with urllib.request.urlopen(lock['source_archive_url'], timeout=60) as src:
            with destination.open('xb') as dst:
                total = 0
                while block := src.read(1024 * 1024):
                    total += len(block)
                    require(total <= lock['source_archive_size'], 'Source download exceeds locked size')
                    dst.write(block)
    require(regular_file(destination, MAX_ARCHIVE) == lock['source_archive_size'],
            'Source archive size mismatch')
    require(sha256(destination.read_bytes()) == lock['source_archive_sha256'],
            'Source archive digest mismatch')


def unpack_source(archive, destination, lock):
    require(regular_file(archive, MAX_ARCHIVE) == lock['source_archive_size'] and
            sha256(archive.read_bytes()) == lock['source_archive_sha256'],
            'Unverified source archive')
    with tarfile.open(archive, 'r:gz') as bundle:
        members = bundle.getmembers()
        require(0 < len(members) <= 2000, 'Source member count exceeds bound')
        total, seen, selected = 0, set(), []
        for member in members:
            name = member.name.rstrip('/')
            path = PurePosixPath(name)
            require(name and '\\' not in name and '\x00' not in name and
                    not path.is_absolute() and '..' not in path.parts and
                    path.as_posix() == name and
                    path.parts[0] == lock['source_archive_root'] and
                    all(':' not in part for part in path.parts), 'Unsafe source member path')
            require(member.isfile() or member.isdir(), 'Source links or special files refused')
            require(name.casefold() not in seen, 'Duplicate source member path')
            seen.add(name.casefold())
            total += member.size
            require(0 <= member.size <= MAX_EXPANDED and total <= MAX_EXPANDED,
                    'Expanded source exceeds bound')
            if len(path.parts) == 1:
                require(member.isdir(), 'Archive root must be a directory')
            else:
                selected.append((member, Path(*path.parts[1:])))
        # Validate the entire selection before creating the source directory.
        destination.mkdir()
        for member, relative in selected:
            target = destination / relative
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                with bundle.extractfile(member) as src, target.open('xb') as dst:
                    shutil.copyfileobj(src, dst, 1024 * 1024)
                require(target.stat().st_size == member.size, 'Incomplete source member')
                target.chmod(0o755 if member.mode & 0o111 else 0o644)
    for name, digest in lock['source_files'].items():
        require(sha256((destination / name).read_bytes()) == digest,
                'Source file digest mismatch: ' + name)


def apply_exact_patch(source, patch, lock):
    require(sha256(source) == lock['source_files'][lock['patched_file']],
            'Patch input is not exact upstream source')
    require(sha256(patch) == lock['patch_sha256'], 'Patch bytes mismatch')
    lines = patch.splitlines(keepends=True)
    require(lines[:2] == [b'--- a/internal/style/colors.go\n',
                          b'+++ b/internal/style/colors.go\n'], 'Unexpected patch target')
    old = source.splitlines(keepends=True)
    result, cursor, index = [], 0, 2
    while index < len(lines):
        match = re.fullmatch(rb'@@ -(\d+),(\d+) \+(\d+),(\d+) @@\n', lines[index])
        require(match is not None, 'Unexpected patch hunk')
        start, old_count, new_start, new_count = map(int, match.groups())
        require(cursor <= start - 1 <= len(old), 'Overlapping or out-of-range patch')
        result.extend(old[cursor:start - 1]); cursor = start - 1
        require(len(result) == new_start - 1, 'Patch new offset mismatch')
        index += 1; consumed, added = 0, 0
        while index < len(lines) and not lines[index].startswith(b'@@ '):
            row = lines[index]; index += 1
            require(row[:1] in (b' ', b'-', b'+'), 'Unexpected patch content')
            if row[:1] != b'+':
                require(cursor < len(old) and old[cursor] == row[1:], 'Patch context mismatch')
                cursor += 1; consumed += 1
            if row[:1] != b'-':
                result.append(row[1:]); added += 1
        require((consumed, added) == (old_count, new_count), 'Patch counts mismatch')
    result.extend(old[cursor:]); patched = b''.join(result)
    require(sha256(patched) == lock['patched_file_sha256'], 'Patched source digest mismatch')
    return patched


def build_environment(parent):
    env = dict(parent)
    for name in list(env):
        if name.startswith(('SLACK_', 'GO', 'CGO_')):
            del env[name]
    env.update({'CGO_ENABLED': '0', 'GOOS': 'windows', 'GOARCH': 'amd64', 'GOAMD64': 'v1',
                'GOTOOLCHAIN': 'local', 'GOWORK': 'off', 'GOENV': 'off', 'GOFLAGS': '',
                'GOPROXY': 'https://proxy.golang.org', 'GOSUMDB': 'sum.golang.org',
                'GOPRIVATE': '', 'GONOPROXY': '', 'GONOSUMDB': '', 'GOINSECURE': '',
                'GOVCS': '*:off'})
    return env


def inspect_pe(binary):
    regular_file(binary, MAX_BINARY)
    data = binary.read_bytes()
    require(0 < len(data) <= MAX_BINARY and data[:2] == b'MZ', 'Missing or oversized Windows binary')
    require(len(data) >= 64, 'Truncated DOS header')
    offset = struct.unpack_from('<I', data, 0x3c)[0]
    require(offset + 94 <= len(data) and data[offset:offset + 4] == b'PE\0\0', 'Invalid PE header')
    require(struct.unpack_from('<H', data, offset + 4)[0] == 0x8664, 'Expected amd64 compatibility binary')
    require(struct.unpack_from('<H', data, offset + 24)[0] == 0x20b, 'Expected PE32+')
    require(struct.unpack_from('<H', data, offset + 92)[0] == 3, 'Expected unchanged console subsystem')
    return sha256(data), len(data)


def validate_built_output(output):
    """Revalidate all three files before packaging; never trust provenance alone."""
    lock, lock_hash = load_lock()
    require(output.is_dir() and not output.is_symlink(), 'Expected ordinary build output directory')
    require({path.name for path in output.iterdir()} == {'slack.exe', 'LICENSE', 'provenance.json'},
            'Unexpected built output inventory')
    regular_file(output / 'provenance.json', 65536)
    regular_file(output / 'LICENSE', 65536)
    data = json.loads((output / 'provenance.json').read_bytes())
    expected = {key: lock[key] for key in (
        'distribution', 'upstream_version', 'upstream_source', 'source_archive_sha256',
        'patch_sha256', 'go_version', 'goos', 'goarch', 'cgo_enabled', 'build_flags')}
    expected.update({'schema': 1, 'source_lock_sha256': lock_hash,
                     'patched_file_sha256': lock['patched_file_sha256'],
                     'license_sha256': lock['source_files']['LICENSE'],
                     'official_binary': False, 'goamd64': 'v1', 'pe_subsystem': 3})
    require(set(data) == set(expected) | {'binary_sha256', 'binary_size'},
            'Unexpected provenance fields')
    for key, value in expected.items():
        require(type(data[key]) is type(value) and data[key] == value,
                'Build provenance mismatch: ' + key)
    digest, size = inspect_pe(output / 'slack.exe')
    require(data['binary_sha256'] == digest and type(data['binary_size']) is int and
            data['binary_size'] == size, 'Built binary digest or size mismatch')
    require(sha256((output / 'LICENSE').read_bytes()) == expected['license_sha256'],
            'Built upstream license mismatch')
    return data


def build(output, go='go', supplied=None):
    lock, lock_hash = load_lock()
    output = output.parent.resolve(strict=True) / output.name
    require(not os.path.lexists(output), 'Output directory already exists; no reuse or retry')
    require(output.parent.is_dir(), 'Output parent must already exist')
    require(not output.is_relative_to(ROOT), 'Build output must be outside the publication checkout')
    work = output.with_name(output.name + '.work')
    require(not os.path.lexists(work), 'Work directory already exists; no reuse or retry')
    go_path = shutil.which(go)
    require(go_path is not None, 'Pinned Go must already be installed by CI')
    env = build_environment(os.environ)
    flags = {'creationflags': subprocess.CREATE_NO_WINDOW} if os.name == 'nt' else {}
    version = subprocess.run([go_path, 'env', 'GOVERSION'], env=env, check=True,
                             capture_output=True, text=True, timeout=15, **flags).stdout.strip()
    require(version == lock['go_version'], 'Go version mismatch; no toolchain download')
    output.mkdir()
    work.mkdir()  # A failed earlier work directory is never reused.
    env.update({'GOCACHE': str(work / 'go-cache'), 'GOMODCACHE': str(work / 'go-mod')})
    archive, source = work / 'source.tar.gz', work / 'source'
    acquire_source(archive, lock, supplied)
    unpack_source(archive, source, lock)
    target = source / lock['patched_file']
    target.write_bytes(apply_exact_patch(target.read_bytes(), (VENDOR / lock['patch_file']).read_bytes(), lock))
    binary = output / 'slack.exe'
    command = [go_path, 'build', *lock['build_flags'], '-o', str(binary), '.']
    with (work / 'build.stdout').open('xb') as stdout, (work / 'build.stderr').open('xb') as stderr:
        subprocess.run(command, cwd=source, env=env, stdin=subprocess.DEVNULL,
                       stdout=stdout, stderr=stderr, check=True, timeout=1800, **flags)
    for name in ('go.mod', 'go.sum'):
        require(sha256((source / name).read_bytes()) == lock['source_files'][name],
                'Build changed locked module inputs')
    binary_hash, binary_size = inspect_pe(binary)
    with (output / 'LICENSE').open('xb') as stream:
        stream.write((source / 'LICENSE').read_bytes())
    provenance = {key: lock[key] for key in (
        'distribution', 'upstream_version', 'upstream_source', 'source_archive_sha256',
        'patch_sha256', 'go_version', 'goos', 'goarch', 'cgo_enabled', 'build_flags')}
    provenance.update({'schema': 1, 'binary_sha256': binary_hash, 'binary_size': binary_size,
                       'source_lock_sha256': lock_hash, 'patched_file_sha256': lock['patched_file_sha256'],
                       'license_sha256': lock['source_files']['LICENSE'], 'official_binary': False,
                       'goamd64': 'v1', 'pe_subsystem': 3})
    with (output / 'provenance.json').open('x', encoding='utf-8', newline='\n') as stream:
        json.dump(provenance, stream, indent=2, sort_keys=True); stream.write('\n')
    return validate_built_output(output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--go', default='go', help='Already-installed exact Go 1.27.3')
    parser.add_argument('--source-archive', type=Path, help='Optional already-retained exact source archive')
    args = parser.parse_args()
    result = build(args.output, args.go, args.source_archive)
    print(json.dumps(result, sort_keys=True))


if __name__ == '__main__':
    main()
