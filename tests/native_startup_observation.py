#!/usr/bin/env python3
"""Bounded fixture fault injection; no native registration or live app starts."""
import base64
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import subprocess
import tempfile
import types
import unittest
from unittest.mock import patch
import urllib.error
import uuid
from xml.sax.saxutils import escape

ROOT = Path(__file__).resolve().parent
SOURCE = ROOT / 'native_launch.py'
spec = importlib.util.spec_from_file_location('slack_native_launch', SOURCE)
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


class FixtureTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.home = self.root / 'private home'
        (self.home / 'bin').mkdir(parents=True)
        (self.home / 'bin/acp-slack-adapter.exe').write_bytes(b'inert')
        (self.home / 'bin/acp-slack-adapter').write_bytes(b'inert')
        self.label = 'acp-slack-adapter-' + str(uuid.uuid4())
        self.sid = 'S-1-5-21-111-222-333-1001'
        self.xml = ('<Task xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">'
                    '<Triggers><LogonTrigger><UserId>' + self.sid + '</UserId></LogonTrigger></Triggers>'
                    '<Principals><Principal><UserId>' + self.sid + '</UserId>'
                    '<LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel>'
                    '</Principal></Principals><Actions><Exec><Command>' +
                    escape(str(self.home / 'bin/acp-slack-adapter.exe')) +
                    '</Command><Arguments>--home &quot;' + escape(str(self.home)) +
                    '&quot; serve</Arguments></Exec></Actions></Task>')
        self.owned = {'present': True, 'sid': self.sid, 'principal_sid': self.sid,
                      'trigger_sids': [self.sid], 'xml': base64.b64encode(self.xml.encode()).decode()}
        self.windows = types.SimpleNamespace(name='nt', environ={'SystemRoot': str(self.root)}, path=os.path)

    def tearDown(self):
        self.temporary.cleanup()

    def encoded(self, value):
        if value=={'present':False}:return b'slack-startup-v1\nabsent\n'
        return ('\n'.join(['slack-startup-v1','present',value['xml'],value['sid'],value['principal_sid'],','.join(value['trigger_sids'])])+'\n').encode()

    def read(self):
        return m.startup_registration(self.home, self.label, {})

    def cleanup(self):
        return m.cleanup_fixture_startup(self.home, self.label, {})

    def test_owned_windows_snapshot(self):
        with patch.object(m, 'os', self.windows), patch.object(m, 'run_startup_query', return_value=self.encoded(self.owned)) as query:
            self.assertEqual(self.read(), self.xml)
            self.assertEqual(query.call_args.args[2]['SLACK_FIXTURE_TASK'], self.label)
            self.assertIn('GetTask($env:SLACK_FIXTURE_TASK)', query.call_args.args[1])

    def test_explicit_native_missing_is_absent(self):
        with patch.object(m, 'os', self.windows), patch.object(m, 'run_startup_query', return_value=b'slack-startup-v1\nabsent\n'):
            self.assertIsNone(self.read())

    def test_failed_native_query_is_not_absence(self):
        failure = subprocess.CalledProcessError(1, ['fixture-query'], stderr=b'Access denied')
        with patch.object(m, 'os', self.windows), patch.object(m, 'run_startup_query', side_effect=failure):
            with self.assertRaises(subprocess.CalledProcessError) as caught:
                self.read()
            self.assertIs(caught.exception, failure)

    def test_foreign_principal_refused(self):
        value = copy.deepcopy(self.owned); value['principal_sid'] = 'S-1-5-21-444-1001'
        with patch.object(m, 'os', self.windows), patch.object(m, 'run_startup_query', return_value=self.encoded(value)):
            with self.assertRaises((AssertionError,ValueError)): self.read()

    def test_global_or_other_logon_trigger_refused(self):
        for owners in ([], ['S-1-5-21-444-1001'], [self.sid, self.sid]):
            with self.subTest(owners=owners):
                value = copy.deepcopy(self.owned); value['trigger_sids'] = owners
                with patch.object(m, 'os', self.windows), patch.object(m, 'run_startup_query', return_value=self.encoded(value)):
                    with self.assertRaises((AssertionError,ValueError)): self.read()

    def test_wrong_command_home_and_elevated_principal_refused(self):
        other = self.root / 'foreign'; other.mkdir(); (other / 'other.exe').write_bytes(b'other')
        cases = [self.xml.replace(escape(str(self.home / 'bin/acp-slack-adapter.exe')), escape(str(other / 'other.exe'))),
                 self.xml.replace('&quot;' + escape(str(self.home)) + '&quot;', '&quot;' + escape(str(other)) + '&quot;'),
                 self.xml.replace('LeastPrivilege', 'HighestAvailable')]
        for xml in cases:
            with self.subTest(xml=xml):
                value = copy.deepcopy(self.owned); value['xml'] = base64.b64encode(xml.encode()).decode()
                with patch.object(m, 'os', self.windows), patch.object(m, 'run_startup_query', return_value=self.encoded(value)):
                    with self.assertRaises((AssertionError,ValueError)): self.read()

    def test_lost_remove_reply_deletes_only_verified_owned_task_and_keeps_error(self):
        original = urllib.error.URLError('simulated lost HTTP reply')
        with patch.object(m, 'os', self.windows), patch.object(m, 'run_startup_query', side_effect=[self.encoded(self.owned), b'slack-startup-v1\nabsent\n']), patch.object(m.subprocess, 'run') as delete:
            with self.assertRaises(urllib.error.URLError) as caught:
                m.verify_startup_removal(lambda: (_ for _ in ()).throw(original), self.read, self.cleanup)
            self.assertIs(caught.exception, original)
            delete.assert_called_once()
            self.assertEqual(delete.call_args.args[0][1:], ['/Delete', '/TN', self.label, '/F'])
            self.assertTrue(delete.call_args.kwargs['check'])

    def test_lost_committed_reply_does_not_delete_any_task(self):
        original = urllib.error.URLError('simulated lost committed HTTP reply')
        with patch.object(m, 'os', self.windows), patch.object(m, 'run_startup_query', return_value=b'slack-startup-v1\nabsent\n'), patch.object(m.subprocess, 'run') as delete:
            with self.assertRaises(urllib.error.URLError) as caught:
                m.verify_startup_removal(lambda: (_ for _ in ()).throw(original), self.read, self.cleanup)
            self.assertIs(caught.exception, original); delete.assert_not_called()

    def test_foreign_task_cleanup_refusal_does_not_hide_original_error(self):
        original = urllib.error.URLError('lost HTTP reply')
        value = copy.deepcopy(self.owned); value['principal_sid'] = 'S-1-5-21-999-1001'
        with patch.object(m, 'os', self.windows), patch.object(m, 'run_startup_query', return_value=self.encoded(value)), patch.object(m.subprocess, 'run') as delete:
            with self.assertRaises(urllib.error.URLError) as caught:
                m.verify_startup_removal(lambda: (_ for _ in ()).throw(original), self.read, self.cleanup)
            self.assertIs(caught.exception, original); delete.assert_not_called()
            self.assertIn('Fixture cleanup also failed', caught.exception.__notes__[0])

    def test_false_successful_remove_receipt_still_fails_after_owned_cleanup(self):
        with patch.object(m, 'os', self.windows), patch.object(m, 'run_startup_query', side_effect=[self.encoded(self.owned), self.encoded(self.owned), b'slack-startup-v1\nabsent\n']), patch.object(m.subprocess, 'run') as delete:
            with self.assertRaisesRegex(AssertionError, 'API reported removal'):
                m.verify_startup_removal(lambda: None, self.read, self.cleanup)
            delete.assert_called_once()

    def test_real_absence_needs_no_fallback(self):
        with patch.object(m, 'os', self.windows), patch.object(m, 'run_startup_query', return_value=b'slack-startup-v1\nabsent\n'), patch.object(m.subprocess, 'run') as delete:
            m.verify_startup_removal(lambda: None, self.read, self.cleanup)
            delete.assert_not_called()

    def mac_file(self, foreign=False):
        user = self.root / 'user'; folder = user / 'Library/LaunchAgents'; folder.mkdir(parents=True)
        path = folder / (self.label + '.plist')
        value = {'Label': self.label, 'ProgramArguments': [str(self.home / 'bin/acp-slack-adapter'), '--home', str(self.home), 'serve'], 'RunAtLoad': True}
        if foreign: value['ProgramArguments'][2] = str(self.root)
        path.write_bytes(plistlib.dumps(value))
        return user, path

    @unittest.skipIf(os.name=='nt','Unix LaunchAgent ownership fixture')
    def test_mac_owned_registration_cleanup_and_absence(self):
        user, path = self.mac_file()
        with patch.object(m.Path, 'home', return_value=user):
            self.assertIsNotNone(self.read()); self.cleanup(); self.assertIsNone(self.read())
        self.assertFalse(path.exists())

    @unittest.skipIf(os.name=='nt','Unix LaunchAgent ownership fixture')
    def test_mac_foreign_registration_is_not_removed(self):
        user, path = self.mac_file(foreign=True)
        before = path.read_bytes()
        with patch.object(m.Path, 'home', return_value=user):
            with self.assertRaises(AssertionError): self.cleanup()
        self.assertEqual(path.read_bytes(), before)

    @unittest.skipIf(os.name=='nt','Unix LaunchAgent ownership fixture')
    def test_mac_dangling_symlink_is_not_absence(self):
        user, path = self.mac_file(); path.unlink(); path.symlink_to(self.root / 'missing')
        with patch.object(m.Path, 'home', return_value=user):
            with self.assertRaises((AssertionError,ValueError)): self.read()
        self.assertTrue(path.is_symlink())


    def test_frame_accepts_only_exact_absence(self):
        self.assertEqual(m.parse_startup_observation('slack-startup-v1\nabsent\n'),{'present':False})
        for text in ['', 'absent', 'noise\nslack-startup-v1\nabsent\n', 'slack-startup-v1\nabsent\nextra']:
            with self.subTest(text=text),self.assertRaises(ValueError):m.parse_startup_observation(text)

    def test_frame_rejects_truncation_extra_fields_and_invalid_identity(self):
        good=self.encoded(self.owned).decode()
        for text in ['\n'.join(good.splitlines()[:-1]),good+'extra\n',good.replace(self.sid,'not-an-owner',1)]:
            with self.subTest(text=text),self.assertRaises(ValueError):m.parse_startup_observation(text)

    def test_frame_rejects_noncanonical_base64_and_invalid_utf8(self):
        good=self.encoded(self.owned).decode()
        for encoded in ['!!!!',self.owned['xml']+'=',base64.b64encode(b'\xff').decode()]:
            with self.subTest(encoded=encoded),self.assertRaises(ValueError):m.parse_startup_observation(good.replace(self.owned['xml'],encoded))

if __name__ == '__main__':unittest.main()
