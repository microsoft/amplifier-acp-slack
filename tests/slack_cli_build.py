#!/usr/bin/env python3
"""Inert source/build-contract tests. No Go, product, Slack, or network calls."""
import importlib.util
import io
import json
import os
from pathlib import Path
import struct
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('slack_cli_builder', ROOT / 'scripts/build-slack-cli.py')
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)


def pe_bytes(machine=0x8664, subsystem=3):
    data = bytearray(256)
    data[:2] = b'MZ'; struct.pack_into('<I', data, 0x3c, 64)
    data[64:68] = b'PE\0\0'; struct.pack_into('<H', data, 68, machine)
    struct.pack_into('<H', data, 88, 0x20b); struct.pack_into('<H', data, 156, subsystem)
    return bytes(data)


def output_fixture(folder):
    lock, lock_hash = builder.load_lock()
    folder.mkdir()
    (folder / 'slack.exe').write_bytes(pe_bytes())
    (folder / 'LICENSE').write_bytes((builder.VENDOR / 'LICENSE').read_bytes())
    metadata = {key: lock[key] for key in (
        'distribution', 'upstream_version', 'upstream_source', 'source_archive_sha256',
        'patch_sha256', 'go_version', 'goos', 'goarch', 'cgo_enabled', 'build_flags')}
    metadata.update({'schema': 1, 'source_lock_sha256': lock_hash,
                     'patched_file_sha256': lock['patched_file_sha256'],
                     'license_sha256': lock['source_files']['LICENSE'], 'official_binary': False,
                     'goamd64': 'v1', 'pe_subsystem': 3,
                     'binary_sha256': builder.sha256(pe_bytes()), 'binary_size': 256})
    (folder / 'provenance.json').write_text(json.dumps(metadata))
    return metadata


class BuildContract(unittest.TestCase):
    def test_pinned_lock_and_patch_identity(self):
        lock, _ = builder.load_lock()
        self.assertEqual(lock['upstream_source'], builder.SOURCE)
        self.assertEqual(lock['go_version'], 'go1.27.3')
        body = (builder.VENDOR / lock['patch_file']).read_text()
        self.assertIn('runtime.GOOS == "windows" && os.Getenv("NO_COLOR") != ""', body)
        self.assertIn('+\t\treturn true', body)

    def test_ambient_toolchain_flags_and_version_override_are_removed(self):
        env = builder.build_environment({'PATH': '/fixture', 'HOME': '/home/fixture',
                                        'GOFLAGS': '-race', 'GOEXPERIMENT': 'bad',
                                        'GOROOT': '/other', 'GOTOOLCHAIN': 'auto',
                                        'GOSUMDB': 'off', 'SLACK_TEST_VERSION': 'v9.9.9'})
        self.assertNotIn('GOEXPERIMENT', env); self.assertNotIn('GOROOT', env)
        self.assertNotIn('SLACK_TEST_VERSION', env)
        self.assertEqual(env['PATH'], '/fixture')
        self.assertEqual((env['GOTOOLCHAIN'], env['GOENV'], env['GOWORK']), ('local', 'off', 'off'))
        self.assertEqual((env['GOOS'], env['GOARCH'], env['CGO_ENABLED']), ('windows', 'amd64', '0'))
        self.assertEqual(env['GOSUMDB'], 'sum.golang.org')

    def test_exact_patch_and_second_application_refusal(self):
        source = b'before\nold\nafter\n'
        delta = (b'--- a/internal/style/colors.go\n+++ b/internal/style/colors.go\n'
                 b'@@ -1,3 +1,4 @@\n before\n-old\n+new\n+extra\n after\n')
        expected = b'before\nnew\nextra\nafter\n'
        lock = {'source_files': {'internal/style/colors.go': builder.sha256(source)},
                'patched_file': 'internal/style/colors.go', 'patch_sha256': builder.sha256(delta),
                'patched_file_sha256': builder.sha256(expected)}
        self.assertEqual(builder.apply_exact_patch(source, delta, lock), expected)
        with self.assertRaisesRegex(ValueError, 'exact upstream'):
            builder.apply_exact_patch(expected, delta, lock)
        changed = delta.replace(b'-old', b'-bad')
        lock['patch_sha256'] = builder.sha256(changed)
        with self.assertRaisesRegex(ValueError, 'context mismatch'):
            builder.apply_exact_patch(source, changed, lock)

    def test_altered_patch_digest_refused(self):
        source = b'exact source\n'
        lock = {'source_files': {'internal/style/colors.go': builder.sha256(source)},
                'patched_file': 'internal/style/colors.go', 'patch_sha256': '0' * 64}
        with self.assertRaisesRegex(ValueError, 'Patch bytes mismatch'):
            builder.apply_exact_patch(source, b'changed patch bytes', lock)

    def test_archive_mismatch_refuses_before_extraction(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); archive = root / 'wrong.tar.gz'; archive.write_bytes(b'wrong')
            lock, _ = builder.load_lock()
            with self.assertRaisesRegex(ValueError, 'Unverified'):
                builder.unpack_source(archive, root / 'source', lock)
            self.assertFalse((root / 'source').exists())

    def test_archive_links_and_escape_refused_before_writes(self):
        for mode in ('link', 'escape', 'duplicate'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp); archive = root / 'source.tar.gz'
                with tarfile.open(archive, 'w:gz') as out:
                    member = tarfile.TarInfo('locked/../escape' if mode == 'escape' else 'locked/file')
                    if mode == 'link':
                        member.type = tarfile.SYMTYPE; member.linkname = '/outside'
                    else:
                        member.size = 1
                    out.addfile(member, io.BytesIO(b'x') if member.isfile() else None)
                    if mode == 'duplicate':
                        out.addfile(member, io.BytesIO(b'x'))
                lock = {'source_archive_size': archive.stat().st_size,
                        'source_archive_sha256': builder.sha256(archive.read_bytes()),
                        'source_archive_root': 'locked'}
                with self.assertRaises(ValueError):
                    builder.unpack_source(archive, root / 'source', lock)
                self.assertFalse((root / 'source').exists())

    def test_output_rehashes_binary_license_and_exact_inventory(self):
        for changed in ('binary', 'license', 'extra', 'missing'):
            with self.subTest(changed=changed), tempfile.TemporaryDirectory() as tmp:
                folder = Path(tmp) / 'out'; metadata = output_fixture(folder)
                self.assertEqual(builder.validate_built_output(folder), metadata)
                if changed == 'binary': (folder / 'slack.exe').write_bytes(pe_bytes() + b'changed')
                if changed == 'license': (folder / 'LICENSE').write_bytes(b'wrong license')
                if changed == 'extra': (folder / 'extra').write_bytes(b'x')
                if changed == 'missing': (folder / 'LICENSE').unlink()
                with self.assertRaises(ValueError): builder.validate_built_output(folder)

    def test_provenance_refuses_official_label_and_changed_build_identity(self):
        for key, value in [('official_binary', True), ('go_version', 'go1.28.0'),
                           ('patch_sha256', '0' * 64), ('source_archive_sha256', '0' * 64),
                           ('distribution', 'official'), ('upstream_source', '0' * 40),
                           ('binary_size', True), ('cgo_enabled', 0), ('build_flags', [])]:
            with self.subTest(key=key), tempfile.TemporaryDirectory() as tmp:
                folder = Path(tmp) / 'out'; metadata = output_fixture(folder)
                metadata[key] = value; (folder / 'provenance.json').write_text(json.dumps(metadata))
                with self.assertRaises(ValueError): builder.validate_built_output(folder)

    def test_pe_wrong_architecture_and_gui_subsystem_refused(self):
        for image in (pe_bytes(machine=0xaa64), pe_bytes(subsystem=2), b'MZ', b'not a PE'):
            with self.subTest(image=image[:8]), tempfile.TemporaryDirectory() as tmp:
                binary = Path(tmp) / 'slack.exe'; binary.write_bytes(image)
                with self.assertRaises(ValueError): builder.inspect_pe(binary)

    @unittest.skipUnless(hasattr(os, 'symlink'), 'symlink primitive unavailable')
    def test_output_member_symlink_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); folder = root / 'out'; output_fixture(folder)
            binary = folder / 'slack.exe'; binary.rename(root / 'other.exe')
            try: binary.symlink_to(root / 'other.exe')
            except OSError as error: self.skipTest('ordinary symlink unavailable: ' + str(error))
            with self.assertRaisesRegex(ValueError, 'ordinary'): builder.validate_built_output(folder)

    def test_existing_output_or_work_refuses_without_go(self):
        for suffix in ('', '.work'):
            with self.subTest(suffix=suffix), tempfile.TemporaryDirectory() as tmp:
                output = Path(tmp) / 'out'; output.with_name('out' + suffix).mkdir()
                with patch.object(builder.subprocess, 'run') as run:
                    with self.assertRaisesRegex(ValueError, 'exists'):
                        builder.build(output)
                    run.assert_not_called()

    def test_wrong_go_version_refuses_before_build_or_fetch(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(builder.shutil, 'which', return_value='/fixture/go'):
            with patch.object(builder.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, 'go1.26.0\n')) as run:
                with patch.object(builder.urllib.request, 'urlopen') as fetch:
                    with self.assertRaisesRegex(ValueError, 'Go version mismatch'):
                        builder.build(Path(tmp) / 'out')
                    self.assertEqual(run.call_count, 1); fetch.assert_not_called()
                    self.assertFalse((Path(tmp) / 'out').exists())

    @unittest.skipUnless(os.environ.get('SLACK_CLI_SOURCE_ARCHIVE'), 'exact source archive not supplied')
    def test_actual_locked_archive_patch_and_mock_build_provenance(self):
        # Actual source archive/patch, synthetic PE output: never invokes Go or Slack.
        archive = Path(os.environ['SLACK_CLI_SOURCE_ARCHIVE'])
        calls = []
        def fake_go(command, **kwargs):
            calls.append((command, kwargs))
            if command[1:3] == ['env', 'GOVERSION']:
                return subprocess.CompletedProcess(command, 0, 'go1.27.3\n')
            self.assertEqual(command[1], 'build')
            Path(command[command.index('-o') + 1]).write_bytes(pe_bytes())
            return subprocess.CompletedProcess(command, 0)
        with tempfile.TemporaryDirectory() as tmp, patch.object(builder.shutil, 'which', return_value='/fixture/go'):
            with patch.object(builder.subprocess, 'run', side_effect=fake_go), patch.object(builder.urllib.request, 'urlopen') as network:
                output = Path(tmp) / 'out'; result = builder.build(output, supplied=archive)
                self.assertFalse(result['official_binary'])
                self.assertEqual(result['distribution'], 'constellation-headless-1')
                self.assertEqual(len(calls), 2); network.assert_not_called()
                self.assertEqual(calls[1][1]['env']['GOTOOLCHAIN'], 'local')
                self.assertEqual(calls[1][1]['env']['GOARCH'], 'amd64')
                self.assertEqual(calls[1][1]['stdin'], subprocess.DEVNULL)
                self.assertEqual({p.name for p in output.iterdir()}, {'slack.exe', 'LICENSE', 'provenance.json'})


if __name__ == '__main__':
    unittest.main(verbosity=2)
