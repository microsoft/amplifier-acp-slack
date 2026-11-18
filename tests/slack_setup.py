"""Guided setup lifecycle, with the official CLI's boundary replaced by a fixture."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from types import SimpleNamespace
spec = importlib.util.spec_from_file_location('setup', Path(__file__).resolve().parents[1]/'scripts/slack_setup.py')
s = importlib.util.module_from_spec(spec)
spec.loader.exec_module(s)

# The source installer declares the intended workspace with these `init` arguments.
installer_spec = importlib.util.spec_from_file_location('installer', Path(__file__).resolve().parents[1]/'scripts/install.py')
installer = importlib.util.module_from_spec(installer_spec)
installer_spec.loader.exec_module(installer)

class InstallerWorkspaceTests(unittest.TestCase):
    def test_declared_workspace_becomes_init_arguments(self):
        self.assertEqual(installer.workspace_arguments(None,None),[])
        self.assertEqual(installer.workspace_arguments('T0EXAMPLE1',None),['--workspace-id','T0EXAMPLE1'])
        self.assertEqual(installer.workspace_arguments('T0EXAMPLE1',' Example Workspace '),['--workspace-id','T0EXAMPLE1','--workspace-name=Example Workspace'])
        self.assertEqual(installer.workspace_arguments('E0EXAMPLE1','-leading dash')[-1],'--workspace-name=-leading dash')

    def test_a_name_or_address_is_refused_before_anything_is_installed(self):
        for mistaken in ['Example Workspace','example-workspace','t0example1','https://app.slack.com/client/T0EXAMPLE1','T']:
            with self.assertRaises(ValueError,msg=mistaken):installer.workspace_arguments(mistaken,None)
        with self.assertRaises(ValueError):installer.workspace_arguments(None,'Example Workspace')

class SetupChildTests(unittest.TestCase):
    def test_vendor_cli_is_hidden_on_windows_without_changing_its_request(self):
        with tempfile.TemporaryDirectory() as temporary:
            folder=Path(temporary)
            for platform in ['win32','linux','darwin']:
                with self.subTest(platform=platform), patch.object(s,'ensure_cli',return_value=folder/'slack'), patch.object(s.sys,'platform',platform), patch.object(s.subprocess,'CREATE_NO_WINDOW',0x08000000,create=True), patch.object(s.subprocess,'run',return_value=SimpleNamespace(returncode=0,stdout='done',stderr='')) as child:
                    self.assertEqual(s.cli(folder,'auth','login','--no-prompt'),'done')
                    args,kwargs=child.call_args
                    self.assertEqual(args[0][-3:],['auth','login','--no-prompt'])
                    self.assertEqual(kwargs['timeout'],120)
                    self.assertEqual(kwargs['input'],'')
                    self.assertTrue(kwargs['capture_output'])
                    if platform=='win32':self.assertEqual(kwargs['creationflags'],0x08000000)
                    else:self.assertNotIn('creationflags',kwargs)

class ConfirmationRefusalTests(unittest.TestCase):
    exact = ('The following error was returned by the apps.hosted.exchangeAuthTicket Slack API method\n\n'
             'The challenge code is invalid (invalid_challenge)\n')
    def test_only_exact_completed_exchange_rejection_is_typed(self):
        with tempfile.TemporaryDirectory() as temporary:
            folder=Path(temporary); args=('auth','login','--ticket','fixture-ticket','--challenge','fixture-code')
            result=SimpleNamespace(returncode=1,stdout='',stderr=self.exact)
            self.assertTrue(s.explicit_invalid_challenge(result,args,folder))
            for output in ['invalid_challenge', 'example '+self.exact,
                           self.exact.replace('apps.hosted.exchangeAuthTicket','other.method'),
                           self.exact+'Another failure (request_timeout)\n']:
                with self.subTest(output=output):self.assertFalse(s.explicit_invalid_challenge(SimpleNamespace(returncode=1,stdout='',stderr=output),args,folder))
            for code in [0,-9]:self.assertFalse(s.explicit_invalid_challenge(SimpleNamespace(returncode=code,stdout='',stderr=self.exact),args,folder))
            self.assertFalse(s.explicit_invalid_challenge(result,('deploy',),folder))
            (folder/'credentials.json').write_text('{}')
            self.assertFalse(s.explicit_invalid_challenge(result,args,folder))

    def test_cli_preserves_generic_unknowns_and_never_returns_raw_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            folder=Path(temporary)
            for output,kind in [(self.exact,s.ConfirmationRefusal),('private external failure invalid_challenge',s.SetupError)]:
                with self.subTest(known=kind is s.ConfirmationRefusal), patch.object(s,'ensure_cli',return_value=folder/'slack'), patch.object(s.subprocess,'run',return_value=SimpleNamespace(returncode=1,stdout='',stderr=output)):
                    with self.assertRaises(kind) as error:s.cli(folder,'auth','login','--ticket','fixture-ticket','--challenge','fixture-code',auth_dir=folder/'pending-auth')
                    if kind is s.SetupError:self.assertNotIsInstance(error.exception,s.ConfirmationRefusal)
                    self.assertNotIn('private external failure',str(error.exception))

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
        self.real_cli = s.cli
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

    def test_invalid_format_and_expiry_refuse_without_cli_dispatch(self):
        self.auth();saved=(self.folder/'auth/credentials.json').read_bytes()
        s.execute(self.home,'authorize',{});pending=(self.folder/'pending.json').read_bytes();calls=list(self.calls)
        with self.assertRaises(s.ConfirmationRefusal) as error:s.execute(self.home,'confirm',{'challenge':'bad code with spaces'})
        self.assertEqual(error.exception.code,'invalid_confirmation_format');self.assertEqual(calls,self.calls)
        self.assertEqual(pending,(self.folder/'pending.json').read_bytes());self.assertEqual(saved,(self.folder/'auth/credentials.json').read_bytes())
        s.write(self.folder/'pending.json',{'ticket':'expired','expires_at':0})
        with self.assertRaises(s.ConfirmationRefusal) as error:s.execute(self.home,'confirm',{'challenge':'code'})
        self.assertEqual(error.exception.code,'authorization_expired');self.assertEqual(calls,self.calls)

    def test_known_invalid_challenge_retires_only_exact_pending_and_new_authorize_is_explicit(self):
        self.auth();saved=(self.folder/'auth/credentials.json').read_bytes();s.execute(self.home,'authorize',{})
        def refused(*args,**kwargs):raise s.ConfirmationRefusal('invalid_challenge')
        calls=list(self.calls)
        with patch.object(s,'cli',refused), self.assertRaises(s.ConfirmationRefusal):s.execute(self.home,'confirm',{'challenge':'wrong-code'})
        self.assertFalse((self.folder/'pending.json').exists());self.assertEqual(saved,(self.folder/'auth/credentials.json').read_bytes());self.assertEqual(calls,self.calls)
        self.assertFalse((self.folder/'project').exists());self.assertEqual(self.created,0)
        s.execute(self.home,'authorize',{});self.assertEqual(len(self.calls),len(calls)+1)

    def test_generic_confirmation_timeout_preserves_pending_and_saved_access(self):
        self.auth();saved=(self.folder/'auth/credentials.json').read_bytes();s.execute(self.home,'authorize',{});pending=(self.folder/'pending.json').read_bytes()
        with patch.object(s,'cli',side_effect=s.subprocess.TimeoutExpired('fixture',120)), self.assertRaises(s.subprocess.TimeoutExpired):s.execute(self.home,'confirm',{'challenge':'code'})
        self.assertEqual(pending,(self.folder/'pending.json').read_bytes());self.assertEqual(saved,(self.folder/'auth/credentials.json').read_bytes());self.assertEqual(self.created,0)

    def test_wrong_workspace_reauthorization_preserves_previous_credentials(self):
        self.auth()
        previous=(self.folder/'auth/credentials.json').read_bytes()
        self.team='TTWO'
        s.execute(self.home,'authorize',{})
        with self.assertRaises(s.SetupError):s.execute(self.home,'confirm',{'challenge':'code','team_id':'TONE'})
        self.assertEqual(previous,(self.folder/'auth/credentials.json').read_bytes())

    def test_declared_workspace_refusal_names_both_and_retires_the_used_ticket(self):
        self.team='T0OTHER0001'
        s.execute(self.home,'authorize',{})
        with self.assertRaises(s.SetupError) as refused:
            s.execute(self.home,'confirm',{'challenge':'code','team_id':'T0EXAMPLE1','team_name':'Example Workspace'})
        message=str(refused.exception)
        for expected in ['test (T0OTHER0001)','Example Workspace (T0EXAMPLE1)','Nothing was created','Slackbot in Example Workspace (T0EXAMPLE1)']:
            self.assertIn(expected,message)
        self.assertNotIn('private',message)
        # Slack consumed the ticket with that approval: nothing is kept, and the next attempt gets a new command.
        for name in ['pending.json','pending-auth/credentials.json','auth/credentials.json','project']:
            self.assertFalse((self.folder/name).exists(),name)
        status=s.execute(self.home,'status',{})
        self.assertFalse(status['authorized']);self.assertFalse(status['authorization_pending'])
        s.execute(self.home,'authorize',{})
        self.assertEqual(self.calls.count(('auth','login','--no-prompt')),2)
        self.team='T0EXAMPLE1'
        result=s.execute(self.home,'confirm',{'challenge':'code','team_id':'T0EXAMPLE1','team_name':'Example Workspace'})
        self.assertEqual((result['authorized'],result['team_id'],result['workspace']),(True,'T0EXAMPLE1','test'))

    def test_saved_approval_for_another_workspace_never_creates_the_app(self):
        self.auth()
        with self.assertRaises(s.SetupError) as refused:
            s.execute(self.home,'connect',{'manifest':{},'team_id':'T0EXAMPLE1','team_name':'Example Workspace'})
        self.assertIn('test (TONE)',str(refused.exception));self.assertIn('Example Workspace (T0EXAMPLE1)',str(refused.exception))
        with self.assertRaises(s.SetupError) as unnamed:
            s.execute(self.home,'connect',{'manifest':{},'team_id':'T0EXAMPLE1'})
        self.assertIn('this connection is for T0EXAMPLE1.',str(unnamed.exception))
        self.assertEqual([c for c in self.calls if c[0]=='deploy'],[])
        self.assertFalse((self.folder/'project').exists())

    def test_discard_drops_an_unused_approval_so_another_workspace_can_be_authorized(self):
        self.auth()
        s.write(self.folder/'authorization-health.json',{'state':'needs_attention','message':'Reconnect Slack.'})
        result=s.execute(self.home,'discard',{})
        self.assertEqual((result['authorized'],result['authorization']),(False,'missing'))
        self.assertNotIn('team_id',result)
        self.assertFalse((self.folder/'auth/credentials.json').exists())
        self.assertEqual(s.execute(self.home,'discard',{})['authorized'],False)
        self.team='TTWO'
        self.assertEqual(self.auth()['team_id'],'TTWO')
        self.assertEqual(s.execute(self.home,'connect',{'manifest':{},'team_id':'TTWO'})['app_id'],'ANEW')

    def test_discard_keeps_access_once_the_app_exists(self):
        self.auth()
        s.execute(self.home,'connect',{'manifest':{}})
        saved=(self.folder/'auth/credentials.json').read_bytes()
        with self.assertRaises(s.SetupError):s.execute(self.home,'discard',{})
        self.assertEqual(saved,(self.folder/'auth/credentials.json').read_bytes())
        self.assertTrue(s.execute(self.home,'status',{})['authorized'])

    def test_discard_keeps_access_after_deploy_loses_ack_before_app_record(self):
        self.auth()
        saved=(self.folder/'auth/credentials.json').read_bytes()
        def lost_ack(folder,*args,cwd=None,auth_dir=None):
            if args[0]=='deploy':
                self.calls.append(args)
                self.created+=1  # Simulated remote effect before any local app record.
                raise s.subprocess.TimeoutExpired('fixture deploy',120)
            return self.cli(folder,*args,cwd=cwd,auth_dir=auth_dir)
        with patch.object(s,'cli',lost_ack):
            with self.assertRaises(s.subprocess.TimeoutExpired):
                s.execute(self.home,'connect',{'manifest':{},'team_id':self.team})
        self.assertFalse((self.folder/'project/.slack/apps.json').exists())
        self.assertFalse((self.folder/'issued.json').exists())
        manifest=(self.folder/'project/manifest.json').read_bytes()
        calls=list(self.calls)
        with self.assertRaises(s.SetupError):s.execute(self.home,'discard',{})
        self.assertEqual(saved,(self.folder/'auth/credentials.json').read_bytes())
        self.assertEqual(manifest,(self.folder/'project/manifest.json').read_bytes())
        self.assertTrue(s.execute(self.home,'status',{})['authorized'])
        self.assertEqual(self.calls,calls)  # No reconciliation request or automatic retry.
        self.assertEqual(self.created,1)

    def test_discard_keeps_legacy_prepared_project_without_claiming_no_deploy(self):
        self.auth()
        saved=(self.folder/'auth/credentials.json').read_bytes()
        # An older helper leaves no durable attempt marker. Its existing project
        # cannot establish whether an external deploy did or did not happen.
        project=s.prepare_project(self.folder,{},'',{'team_id':self.team})
        self.assertFalse((project/'.slack/apps.json').exists())
        calls=list(self.calls)
        with self.assertRaises(s.SetupError):s.execute(self.home,'discard',{})
        self.assertEqual(saved,(self.folder/'auth/credentials.json').read_bytes())
        self.assertEqual(self.calls,calls)
        self.assertEqual(self.created,0)

    def test_reopening_authorization_preserves_pending_code(self):
        first=s.execute(self.home,'authorize',{})
        second=s.execute(self.home,'authorize',{})
        self.assertEqual(first,second)
        self.assertEqual(self.calls,[('auth','login','--no-prompt')])

    def test_successful_confirmation_clears_rejected_authorization_state(self):
        self.auth()
        s.write(self.folder/'authorization-health.json',{'state':'needs_attention','message':'Reconnect Slack.'})
        self.assertEqual(s.execute(self.home,'status',{})['authorization'],'needs_attention')
        result=self.auth()
        self.assertEqual(result['authorization'],'managed')
        self.assertNotIn('authorization_issue',result)

    def test_explicit_rejection_is_distinct_from_transient_cli_failure(self):
        self.auth()
        for failure, needs_attention in [('token_revoked private-secret',True),('connection timed out private-secret',False)]:
            (self.folder/'authorization-health.json').unlink(missing_ok=True)
            with patch.object(s,'ensure_cli',return_value=Path('/unused')), patch.object(s.subprocess,'run',return_value=SimpleNamespace(returncode=1,stdout='',stderr=failure)):
                with self.assertRaises(s.SetupError):self.real_cli(self.folder,'auth','list')
            result=s.execute(self.home,'status',{})
            self.assertEqual(result['authorization']=='needs_attention',needs_attention)
            self.assertNotIn('private-secret',json.dumps(result))

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
        if os.name != 'nt': self.assertEqual((self.folder/'issued.json').stat().st_mode & 0o777,0o600)

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

    def test_hook_uses_current_private_python_and_handles_spaces(self):
        project=self.folder/'project with spaces'
        project.mkdir()
        command=s.hook_command(project,'manifest')
        self.assertIn(s.sys.executable,command)
        self.assertIn('-I',command);self.assertIn('-B',command);self.assertIn('-X',command);self.assertIn('utf8',command)
        prepared=s.prepare_project(self.folder,{},'',{'team_id':'TONE'})
        compile((prepared/'hooks.py').read_text(),'hooks.py','exec')
        self.assertNotIn('python3 hooks.py',(prepared/'.slack/hooks.json').read_text())

    def test_official_mac_dot_prefix_and_linux_archive_layouts(self):
        import io,tarfile,hashlib
        for name in ['bin/slack','./bin/slack']:
            archive=io.BytesIO()
            with tarfile.open(fileobj=archive,mode='w:gz') as bundle:
                member=tarfile.TarInfo(name);member.size=6;bundle.addfile(member,io.BytesIO(b'native'))
            data=archive.getvalue();target=('macOS_arm64',hashlib.sha256(data).hexdigest())
            folder=self.folder/name.replace('/','_')
            with patch.object(s.platform,'system',return_value='Darwin'),patch.object(s.platform,'machine',return_value='arm64'),patch.dict(s.ARCHIVES,{('Darwin','arm64'):target}),patch.object(s.urllib.request,'urlopen',return_value=io.BytesIO(data)):
                self.assertEqual(s.ensure_cli(folder).read_bytes(),b'native')

    def test_verified_archive_still_rejects_ambiguous_binary_entries(self):
        import io,tarfile,hashlib
        archive=io.BytesIO()
        with tarfile.open(fileobj=archive,mode='w:gz') as bundle:
            for name in ['bin/slack','./bin/slack']:
                member=tarfile.TarInfo(name);member.size=6;bundle.addfile(member,io.BytesIO(b'native'))
        data=archive.getvalue();target=('macOS_arm64',hashlib.sha256(data).hexdigest())
        with patch.object(s.platform,'system',return_value='Darwin'),patch.object(s.platform,'machine',return_value='arm64'),patch.dict(s.ARCHIVES,{('Darwin','arm64'):target}),patch.object(s.urllib.request,'urlopen',return_value=io.BytesIO(data)):
            with self.assertRaises(s.SetupError):s.ensure_cli(self.folder)

    def test_existing_app_is_adopted_without_another_identity(self):
        self.auth()
        result=s.execute(self.home,'connect',{'manifest':{},'app_id':'AEXISTING'})
        self.assertEqual(result['app_id'],'AEXISTING')
        self.assertEqual(self.created,0)

if __name__=='__main__':unittest.main()
