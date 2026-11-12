"""Shared archive safety and exact notice provenance tests, no network."""
import importlib.util
import io
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest import mock

import python_support as package

class SupportTests(unittest.TestCase):
    def archive(self, root, entries):
        path = root / 'python.tar.gz'
        with tarfile.open(path, 'w:gz') as out:
            for name, content, kind in entries:
                entry = tarfile.TarInfo(name)
                entry.mode = 0o755
                if kind:
                    entry.type = kind
                    entry.linkname = content
                    out.addfile(entry)
                else:
                    entry.size = len(content)
                    out.addfile(entry, io.BytesIO(content))
        return path

    def test_distributor_internal_alias_is_copied_as_regular_file(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            archive = self.archive(root, [('python/bin/python3.13', b'python', None),
                                          ('python/bin/python3', 'python3.13', tarfile.SYMTYPE)])
            package.unpack_python(archive, root / 'out')
            path = root / 'out/bin/python3'
            self.assertEqual(path.read_bytes(), b'python')
            self.assertFalse(path.is_symlink())

    def test_traversal_absolute_and_cross_root_links_rejected(self):
        for name, content, kind in [('python/../../escape', b'no', None),
                                    ('python/bin/a', '../../../escape', tarfile.SYMTYPE),
                                    ('python/bin/a', '/etc/passwd', tarfile.SYMTYPE),
                                    ('python/bin/a', 'C:\\secret', tarfile.SYMTYPE),
                                    ('different/file', b'no', None)]:
            with self.subTest(name=name, content=content), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                archive = self.archive(root, [(name, content, kind)])
                with self.assertRaises(ValueError):
                    package.unpack_python(archive, root / 'out')
                self.assertFalse((root / 'escape').exists())

    def test_cyclic_and_duplicate_archives_rejected(self):
        variants = [[('python/a', 'b', tarfile.SYMTYPE), ('python/b', 'a', tarfile.SYMTYPE)],
                    [('python/a', b'one', None), ('python/a', b'two', None)]]
        for entries in variants:
            with tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                with self.assertRaises(ValueError):
                    package.unpack_python(self.archive(root, entries), root / 'out')

    def test_pinned_archive_corruption_rejected_before_extraction(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'python.tar.gz').write_bytes(b'bad')
            with self.assertRaisesRegex(ValueError, 'checksum'):
                package.download({'filename': 'python.tar.gz', 'size': 3,
                                  'sha256': '0' * 64}, root)

    def test_all_six_platform_locks_retain_exact_notices(self):
        lock = json.loads((package.ROOT / 'lock.json').read_text())
        self.assertEqual(len(lock['assets']), 6)
        self.assertEqual(len({a['target'] for a in lock['assets']}), 6)
        for a in lock['assets']:
            self.assertEqual(len(a['sha256']), 64)
            self.assertEqual(len(a['license_source']['sha256']), 64)
            self.assertTrue(a['url'].startswith('https://github.com/astral-sh/python-build-standalone/releases/download/20261110/'))
            self.assertIn('LICENSE.cpython.txt', a['notices'])
            for name, expected in a['notices'].items():
                self.assertEqual(package.digest(package.ROOT / 'notices' / a['target'] / name), expected)

    def test_preparation_preserves_source_and_notices_but_omits_mutable_cache(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            archive = self.archive(root, [('python/bin/python3.13', b'fixture executable', None),
                ('python/lib/module.py', b'unchanged source', None),
                ('python/lib/__pycache__/module.pyc', b'mutable cache', None)])
            with mock.patch.object(package, 'download', return_value=archive):
                result = package.prepare('aarch64-unknown-linux-gnu', root / 'cache', root / 'out')
            self.assertEqual(result['executable'], 'bin/python3.13')
            self.assertEqual((root / 'out/lib/module.py').read_bytes(), b'unchanged source')
            self.assertEqual(list((root / 'out').rglob('*.pyc')), [])
            self.assertTrue((root / 'out/distribution-licenses/LICENSE.cpython.txt').is_file())

    def test_missing_notice_stops_before_download_or_destination_write(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            lock, asset = package.asset_for('aarch64-unknown-linux-gnu')
            with mock.patch.object(package, 'ROOT', root), mock.patch.object(package, 'asset_for', return_value=(lock, asset)), mock.patch.object(package, 'download') as download:
                with self.assertRaisesRegex(ValueError, 'notice'):
                    package.prepare(asset['target'], root / 'cache', root / 'out')
                download.assert_not_called()
                self.assertFalse((root / 'out').exists())

    def test_existing_destination_is_not_repurposed(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); out = root / 'out'; out.mkdir()
            (out / 'unrelated').write_bytes(b'keep')
            with mock.patch.object(package, 'download') as download:
                with self.assertRaisesRegex(ValueError, 'empty'):
                    package.prepare('aarch64-unknown-linux-gnu', root / 'cache', out)
                download.assert_not_called()
            self.assertEqual((out / 'unrelated').read_bytes(), b'keep')

if __name__ == '__main__':
    unittest.main()
