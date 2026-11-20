"""Packaging checks for the derived Windows tool; inert PE data only."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


package = load('native_package', ROOT / 'scripts/package-native.py')
fixture = load('build_fixtures', ROOT / 'tests/slack_cli_build.py')
helper = load('setup_helper', ROOT / 'scripts/slack_setup.py')


class NativeCliPackage(unittest.TestCase):
    def test_verified_tool_and_provenance_enter_existing_private_inventory(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            built, private = root / 'build', root / 'runtime/python'
            private.mkdir(parents=True)
            expected = fixture.output_fixture(built)
            self.assertEqual(package.stage_windows_cli(built, private), expected)
            for name in ('slack.exe', 'LICENSE', 'provenance.json'):
                self.assertEqual((private / 'slack-cli' / name).read_bytes(), (built / name).read_bytes())
            inventory = {p.relative_to(private).as_posix(): package.digest(p)
                         for p in private.rglob('*') if p.is_file()}
            self.assertEqual(set(inventory), {'slack-cli/slack.exe', 'slack-cli/LICENSE', 'slack-cli/provenance.json'})
            self.assertEqual(expected['upstream_source'], helper.WINDOWS_CLI_SOURCE)
            self.assertEqual(expected['patch_sha256'], helper.WINDOWS_CLI_PATCH)
            self.assertEqual(expected['distribution'], helper.WINDOWS_CLI_DISTRIBUTION)

    def test_missing_or_corrupt_build_refuses_before_runtime_staging(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            built, private = root / 'build', root / 'runtime/python'
            private.mkdir(parents=True)
            with self.assertRaises(ValueError): package.stage_windows_cli(None, private)
            fixture.output_fixture(built)
            (built / 'slack.exe').write_bytes(b'changed')
            with self.assertRaises(ValueError): package.stage_windows_cli(built, private)
            self.assertFalse((private / 'slack-cli').exists())

    def test_existing_destination_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            built, private = root / 'build', root / 'runtime/python'
            private.mkdir(parents=True); fixture.output_fixture(built)
            destination = private / 'slack-cli'; destination.mkdir()
            (destination / 'slack.exe').write_bytes(b'preserve')
            with self.assertRaises(FileExistsError): package.stage_windows_cli(built, private)
            self.assertEqual((destination / 'slack.exe').read_bytes(), b'preserve')


if __name__ == '__main__':
    unittest.main()
