"""Guided setup lifecycle, with the official CLI's boundary replaced by a fixture."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
spec = importlib.util.spec_from_file_location('setup', Path(__file__).resolve().parents[1]/'scripts/slack_setup.py')
s = importlib.util.module_from_spec(spec)
spec.loader.exec_module(s)

class SetupTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name)
        self.folder = self.home/'secrets/slack-setup'
        self.folder.mkdir(parents=True)
        self.team = 'TONE'
        self.created = 0
        self.calls = []
        self.patcher = patch.object(s, 'cli', self.cli)
        self.patcher.start()
        self.addCleanup(self.patcher.stop)

    def cli(self, folder, *args, cwd=None, auth_dir=None):
        self.calls.append(args)
        if args[:2] == ('auth','login'):
            if '--no-prompt' in args:
                return '/slackauthticket abcdef=='
            s.write(auth_dir/'credentials.json', {self.team:{'team_id':self.team,'team_domain':'test','user_id':'UOWNER','token':'private-access','refresh_token':'private-refresh'}})
        elif args[0] == 'deploy':
            apps=s.read(cwd/'.slack/apps.json',{'apps':{}})
            if self.team not in apps['apps']:
                self.created+=1
                apps['apps'][self.team]={'app_id':'ANEW','team_id':self.team}
                s.write(cwd/'.slack/apps.json',apps)
            s.write(folder/'issued.json',{'app_token':'xapp-private','bot_token':'xoxb-private'})
        return ''

    def auth(self):
        s.execute(self.home,'authorize',{})
        return s.execute(self.home,'confirm',{'challenge':'secret-code'})

    def test_authorization_does_not_expose_tokens_or_persist_challenge(self):
        self.assertIn('/slackauthticket',s.execute(self.home,'authorize',{})['command'])
        self.assertNotIn('ticket',json.dumps(s.execute(self.home,'status',{})))
        result=self.auth()
        self.assertTrue(result['authorized'])
        self.assertNotIn('private',json.dumps(result))
        self.assertFalse((self.folder/'pending.json').exists())
        for p in self.folder.rglob('*.json'):
            self.assertNotIn('secret-code',p.read_text())

    def test_wrong_workspace_reauthorization_preserves_previous_credentials(self):
        self.auth()
        previous=(self.folder/'auth/credentials.json').read_bytes()
        self.team='TTWO'
        s.execute(self.home,'authorize',{})
        with self.assertRaises(s.SetupError):s.execute(self.home,'confirm',{'challenge':'code','team_id':'TONE'})
        self.assertEqual(previous,(self.folder/'auth/credentials.json').read_bytes())

    def test_expired_and_invalid_challenges_never_reach_cli(self):
        with self.assertRaises(s.SetupError):s.execute(self.home,'confirm',{'challenge':'code'})
        s.execute(self.home,'authorize',{})
        count=len(self.calls)
        with self.assertRaises(s.SetupError):s.execute(self.home,'confirm',{'challenge':'bad code'})
        self.assertEqual(len(self.calls),count)

    def test_connect_retry_keeps_one_app_and_private_credentials(self):
        self.auth()
        for _ in range(2):
            result=s.execute(self.home,'connect',{'manifest':{},'team_id':'TONE'})
            self.assertEqual(result['app_id'],'ANEW')
            self.assertNotIn('token',json.dumps(result))
        self.assertEqual(self.created,1)
        self.assertEqual((self.folder/'issued.json').stat().st_mode & 0o777,0o600)

    def test_wrong_workspace_and_app_never_install(self):
        self.auth()
        with self.assertRaises(s.SetupError):s.execute(self.home,'connect',{'manifest':{},'team_id':'TTWO'})
        s.execute(self.home,'connect',{'manifest':{}})
        count=len([c for c in self.calls if c[0]=='deploy'])
        with self.assertRaises(s.SetupError):s.execute(self.home,'connect',{'manifest':{},'app_id':'ADIFFERENT'})
        self.assertEqual(len([c for c in self.calls if c[0]=='deploy']),count)

    def test_internal_token_requires_workspace_binding(self):
        self.auth()
        with self.assertRaises(s.SetupError):s.execute(self.home,'token',{'team_id':'TTWO'})
        self.assertEqual(s.execute(self.home,'token',{'team_id':'TONE'})['token'],'private-access')

    def test_existing_app_is_adopted_without_another_identity(self):
        self.auth()
        result=s.execute(self.home,'connect',{'manifest':{},'app_id':'AEXISTING'})
        self.assertEqual(result['app_id'],'AEXISTING')
        self.assertEqual(self.created,0)

if __name__=='__main__':unittest.main()
