#!/usr/bin/env python3
"""Bounded fixture fault injection; no native registration or live app starts."""
import base64
import copy
import hashlib
import importlib.util
import json
import os
import ntpath
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


class SetupToolFixtureTests(unittest.TestCase):
    def setUp(self):
        self.temporary=tempfile.TemporaryDirectory();self.addCleanup(self.temporary.cleanup)
        self.home=Path(self.temporary.name)/'fresh';(self.home/'secrets').mkdir(parents=True)
        self.config={'enabled':False,'setup_complete':False,'slack':{'app_id':''}}
        (self.home/'adapter.json').write_text(json.dumps(self.config))
        (self.home/'secrets/local-password').write_bytes(b'unchanged private fixture')
        self.headers={'Cookie':'fixture=session','X-CSRF-Token':'fixture-csrf'}
        self.helper_source=ROOT.parent/'scripts/slack_setup.py'
        spec=importlib.util.spec_from_file_location('fresh_fixture_setup_helper',self.helper_source)
        self.helper=importlib.util.module_from_spec(spec);spec.loader.exec_module(self.helper)
        self.empty={'authorized':False,'authorization':'missing','authorization_pending':False,
                    'setup_complete':False,'connection_running':False}

    def local_action(self,base,path,body=None,headers=None):
        self.assertEqual((base,path,headers),('http://fixture.invalid','/api/v1/setup/slack',self.headers))
        if body is None:return 200,{},dict(self.empty)
        self.assertEqual(body,{'action':'discard'})
        # The HTTP boundary is fake; the helper bytes and discard implementation
        # are the real exact source. No service, authorization or external call.
        (self.home/'secrets/slack-setup-helper.py').write_bytes(self.helper_source.read_bytes())
        result=self.helper.execute(self.home,'discard',{})
        return 200,{},dict(result,setup_complete=False,connection_running=False)

    def test_readonly_get_then_one_explicit_local_discard_materializes_exact_helper(self):
        with patch.object(m,'request',side_effect=self.local_action) as request, \
             patch.object(self.helper,'cli',side_effect=AssertionError('No Slack CLI action')) as cli, \
             patch.object(self.helper,'ensure_cli',side_effect=AssertionError('No CLI download')) as ensure, \
             patch.object(self.helper.urllib.request,'urlopen',side_effect=AssertionError('No network')) as network:
            m.fresh_setup_status(self.home,'http://fixture.invalid',self.headers)
            self.assertFalse((self.home/'secrets/slack-setup-helper.py').exists())
            m.prepare_fresh_setup_tool(self.home,'http://fixture.invalid',self.headers)
            self.assertEqual(request.call_count,2)
            self.assertEqual(request.call_args.args[2],{'action':'discard'})
            cli.assert_not_called();ensure.assert_not_called();network.assert_not_called()
        self.assertEqual((self.home/'secrets/local-password').read_bytes(),b'unchanged private fixture')

    def test_readonly_get_refuses_helper_or_directory_side_effect(self):
        for member in ['slack-setup-helper.py','slack-setup']:
            path=self.home/'secrets'/member
            def changed(*args,**kwargs):
                path.mkdir() if member=='slack-setup' else path.write_bytes(b'unexpected')
                return 200,{},dict(self.empty)
            with patch.object(m,'request',side_effect=changed) as request,self.assertRaisesRegex(AssertionError,'Read-only'):
                m.fresh_setup_status(self.home,'http://fixture.invalid',self.headers)
            request.assert_called_once()
            path.rmdir() if path.is_dir() else path.unlink()

    def test_existing_setup_or_pending_authorization_refuses_before_request(self):
        folder=self.home/'secrets/slack-setup';(folder/'pending-auth').mkdir(parents=True)
        credentials=folder/'pending-auth/credentials.json';credentials.write_bytes(b'keep unknown authorization')
        with patch.object(m,'request') as request:
            for action in [m.fresh_setup_status,m.prepare_fresh_setup_tool]:
                with self.assertRaises(AssertionError):action(self.home,'http://fixture.invalid',self.headers)
            request.assert_not_called()
        self.assertEqual(credentials.read_bytes(),b'keep unknown authorization')

    def test_configured_home_refuses_before_request(self):
        for change in [{'enabled':True},{'setup_complete':True},{'slack':{'app_id':'AEXISTING'}}]:
            (self.home/'adapter.json').write_text(json.dumps({**self.config,**change}))
            with patch.object(m,'request') as request,self.assertRaises(AssertionError):
                m.prepare_fresh_setup_tool(self.home,'http://fixture.invalid',self.headers)
            request.assert_not_called()

    def test_lost_or_refused_action_is_not_replayed_or_replaced_by_source_copy(self):
        original=TimeoutError('unobserved local action')
        with patch.object(m,'request',side_effect=original) as request,self.assertRaises(TimeoutError) as caught:
            m.prepare_fresh_setup_tool(self.home,'http://fixture.invalid',self.headers)
        self.assertIs(caught.exception,original);request.assert_called_once()
        self.assertFalse((self.home/'secrets/slack-setup-helper.py').exists())
        with patch.object(m,'request',return_value=(403,{},{})) as request,self.assertRaises(AssertionError):
            m.prepare_fresh_setup_tool(self.home,'http://fixture.invalid',self.headers)
        request.assert_called_once()

    def test_lost_committed_action_retains_materialized_state_without_replay(self):
        original=TimeoutError('committed local action reply lost')
        def lost(*args,**kwargs):
            self.local_action(*args,**kwargs)
            raise original
        with patch.object(m,'request',side_effect=lost) as request,self.assertRaises(TimeoutError) as caught:
            m.prepare_fresh_setup_tool(self.home,'http://fixture.invalid',self.headers)
        self.assertIs(caught.exception,original);request.assert_called_once()
        self.assertEqual((self.home/'secrets/slack-setup-helper.py').read_bytes(),self.helper_source.read_bytes())
        self.assertTrue((self.home/'secrets/slack-setup/owner.lock').is_file())
        self.assertEqual((self.home/'secrets/local-password').read_bytes(),b'unchanged private fixture')

    def test_changed_helper_or_existing_state_is_refused_after_single_action(self):
        for member in ['slack-setup-helper.py','local-password']:
            def changed(*args,**kwargs):
                result=self.local_action(*args,**kwargs)
                (self.home/'secrets'/member).write_bytes(b'changed')
                return result
            with patch.object(m,'request',side_effect=changed) as request,self.assertRaises(AssertionError):
                m.prepare_fresh_setup_tool(self.home,'http://fixture.invalid',self.headers)
            request.assert_called_once()
            (self.home/'secrets/slack-setup-helper.py').unlink()
            (self.home/'secrets/slack-setup/owner.lock').unlink();(self.home/'secrets/slack-setup').rmdir()


class ManagedFailureRecordTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.home=Path(self.temp.name)/'installation';(self.home/'native-manager/requests').mkdir(parents=True)
        (self.home/'native-manager/controls').mkdir()
        info=self.home.lstat();self.identity=(info.st_dev,info.st_ino)
        self.request=str(uuid.uuid4());self.control=str(uuid.uuid4())
        self.events=[{'action':'native-start','args':['--request-id',self.request]},
                     {'action':'native-stop','args':['--request-id',self.control]}]
        self.canary='private-canary-must-never-appear'

    def write(self,name,value):
        path=self.home/'native-manager'/name;path.write_text(json.dumps(value));return path

    def observe(self):return m.managed_failure_records(self.home,self.identity,self.events)

    def test_fixed_issued_records_retain_hash_identity_phase_without_secret_fields_or_commands(self):
        active=self.write('active.json',{'request_id':self.request})
        binding={'id':str(uuid.uuid4()),'binary_sha256':'a'*64,'profile':{'args':[self.canary]}}
        request=self.write(f'requests/{self.request}.json',{'schema':1,'request_id':self.request,'phase':'accepted','binding':binding,'outcome':{'detail':self.canary},'credential':self.canary})
        witness=self.write(f'requests/{self.request}.launched.json',{'request_id':self.request,'binding':binding,'process':{'platform':'windows','pid':123,'creation_time_100ns':'456','session_id':1,'unrecognized':self.canary}})
        self.write(f'controls/{self.control}.json',{'request_id':self.control,'start_request_id':self.request,'phase':'unknown','process_absent':False})
        self.write('enrollment.json',{'id':binding['id'],'profile':{'args':[self.canary]}})
        before={p:p.read_bytes() for p in self.home.rglob('*') if p.is_file()}
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('No command')) as command,patch.object(m,'request',side_effect=AssertionError('No API')) as api:
            result=self.observe();command.assert_not_called();api.assert_not_called()
        self.assertTrue(result['read_only']);self.assertNotIn(self.canary,json.dumps(result))
        self.assertEqual(len(result['records']),6)
        for path in (active,request,witness):
            row=result['records'][str(path.relative_to(self.home))]
            self.assertEqual(row['state'],'observed');self.assertEqual(row['sha256'],hashlib.sha256(before[path]).hexdigest())
        observed=result['records'][str(witness.relative_to(self.home))]['projection']
        self.assertEqual(observed['process'],{'platform':'windows','pid':123,'creation_time_100ns':'456','session_id':1})
        self.assertEqual(observed['binding_sha256'],result['records'][str(request.relative_to(self.home))]['projection']['binding_sha256'])
        self.assertEqual(before,{p:p.read_bytes() for p in self.home.rglob('*') if p.is_file()})

    def test_missing_witness_and_unissued_pointer_remain_readonly_absence(self):
        foreign=str(uuid.uuid4());self.write('active.json',{'request_id':foreign})
        self.write(f'requests/{foreign}.json',{'credential':self.canary})
        result=self.observe()
        self.assertEqual(result['records'][f'native-manager/requests/{self.request}.launched.json']['state'],'absent_at_read')
        self.assertNotIn(f'native-manager/requests/{foreign}.json',result['records'])
        self.assertNotIn(self.canary,json.dumps(result))

    def test_oversize_malformed_nonobject_and_directory_are_distinct(self):
        path=self.home/'native-manager/active.json'
        for data,state in [(b'x'*65537,'oversize'),(b'{','malformed'),(b'[]','invalid_record')]:
            path.write_bytes(data);row=self.observe()['records']['native-manager/active.json'];self.assertEqual(row['state'],state)
            self.assertNotIn('projection',row)
        path.unlink();path.mkdir()
        self.assertEqual(self.observe()['records']['native-manager/active.json']['state'],'unsafe_or_changed')

    def test_hardlink_record_is_not_read(self):
        outside=Path(self.temp.name)/'outside.json';outside.write_text(json.dumps({'credential':self.canary}))
        path=self.home/'native-manager/active.json';os.link(outside,path)
        self.assertEqual(self.observe()['records']['native-manager/active.json']['state'],'unsafe_or_changed')

    def test_symlink_and_parent_redirect_are_not_followed(self):
        outside=Path(self.temp.name)/'outside.json';outside.write_text(json.dumps({'credential':self.canary}))
        path=self.home/'native-manager/active.json'
        try:path.symlink_to(outside)
        except OSError as error:
            if os.name=='nt' and getattr(error,'winerror',None)==1314:self.skipTest('Windows account lacks symlink creation privilege')
            raise
        self.assertEqual(self.observe()['records']['native-manager/active.json']['state'],'unsafe_or_changed')
        path.unlink();folder=self.home/'native-manager/requests';folder.rmdir();folder.symlink_to(outside.parent,target_is_directory=True)
        self.assertEqual(self.observe()['records'][f'native-manager/requests/{self.request}.json']['state'],'unsafe_or_changed')

    def test_unknown_or_changed_home_never_reads_records(self):
        with patch.object(m.os,'open',side_effect=AssertionError('No record read')) as opened:
            self.assertEqual(m.managed_failure_records(self.home,None,self.events)['unavailable'],'fresh_home_identity_not_recorded')
            self.assertEqual(m.managed_failure_records(self.home,(self.identity[0],self.identity[1]+1),self.events)['unavailable'],'fresh_home_identity_changed')
            opened.assert_not_called()

    def test_read_error_is_not_absence_and_original_exception_survives_diagnostic_failure(self):
        self.write('active.json',{'request_id':self.request})
        with patch.object(m.os,'open',side_effect=PermissionError(13,'denied')):
            row=self.observe()['records']['native-manager/active.json'];self.assertEqual((row['state'],row['errno']),('read_error',13))
        original=TimeoutError('original unknown action');receipt={}
        with patch.object(m,'managed_failure_records',side_effect=KeyboardInterrupt()) as observe, self.assertRaises(TimeoutError) as caught:
            try:raise original
            except BaseException:
                m.retain_managed_failure(receipt,self.home,self.identity,self.events,lambda:(_ for _ in ()).throw(OSError('failed receipt save')))
                raise
        self.assertIs(caught.exception,original);observe.assert_called_once()
        self.assertEqual(receipt['manager_records'],{'unavailable':'diagnostic_error','error_type':'KeyboardInterrupt'})
        self.assertEqual(receipt['diagnostic_save_error'],'OSError')


class FixtureTests(unittest.TestCase):
    def test_exact_path_identity_refuses_copy_hardlink_and_missing_path(self):
        with tempfile.TemporaryDirectory() as folder:
            original=Path(folder)/'original';original.write_bytes(b'fixture')
            copied=Path(folder)/'copy';copied.write_bytes(original.read_bytes())
            self.assertTrue(m.exact_path_identity(original,original.parent/'.'/original.name))
            self.assertFalse(m.exact_path_identity(original,copied))
            linked=Path(folder)/'hardlink';os.link(original,linked)
            self.assertTrue(os.path.samefile(original,linked))
            self.assertFalse(m.exact_path_identity(original,linked),'A different hardlink path must not gain authority')
            with self.assertRaises(FileNotFoundError):m.exact_path_identity(original,Path(folder)/'missing')

    def test_exact_path_identity_windows_spelling_still_requires_resolved_path_and_file(self):
        def resolve(value):
            return types.SimpleNamespace(resolve=lambda *,strict: value if strict else self.fail('resolution must be strict'))
        pairs=[(r'C:\Users\RUNNER~1\home',r'C:\Users\runneradmin\home',False),
               (r'\\?\C:\Users\runneradmin\home',r'C:\Users\RUNNERADMIN\home',True),
               (r'\\?\UNC\server\share\home',r'\\server\share\home',True),
               (r'\\.\C:\Users\runneradmin\home',r'C:\Users\runneradmin\home',False),
               (r'C:\Users\runneradmin\other',r'C:\Users\runneradmin\home',False)]
        with patch.object(m,'Path',side_effect=resolve),patch.object(m.os,'name','nt'),patch.object(m.os.path,'normcase',side_effect=ntpath.normcase),patch.object(m.os.path,'samefile',return_value=True) as same:
            for first,second,expected in pairs:
                with self.subTest(first=first,second=second):self.assertEqual(m.exact_path_identity(first,second),expected)
            same.return_value=False
            self.assertFalse(m.exact_path_identity(r'\\?\C:\home',r'C:\home'),'Equal path spelling is not file identity')
        # Real Windows Path.resolve must resolve short names first. This pure
        # boundary does not pretend unmatched RUNNER~1 spelling is canonical.

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


    def test_managed_failed_setup_retains_partial_home_without_cleanup_or_retry(self):
        root=self.root/'managed-retained';home=root/'installation';home.mkdir(parents=True);(home/'partial').write_bytes(b'keep')
        process=types.SimpleNamespace(pid=991,returncode=1,communicate=lambda timeout:(b'',b'refused'))
        result=self.root/'test-native-launch-result.json'
        with patch.object(m.tempfile,'mkdtemp',return_value=str(root)),patch.object(m.subprocess,'Popen',return_value=process) as launch,patch.object(m.shutil,'rmtree') as remove:
            with self.assertRaises(AssertionError):m.managed_lifecycle(self.home/'bin/acp-slack-adapter',{},result)
            launch.assert_called_once();remove.assert_not_called()
        self.assertEqual((home/'partial').read_bytes(),b'keep')
        saved=json.loads(result.with_name('test-native-manager-result.json').read_text());self.assertFalse(saved['passed']);self.assertTrue(saved['retained_home'])

    def test_managed_setup_timeout_reaps_only_caller_and_retains_unknown(self):
        root=self.root/'managed-unknown';home=root/'installation';home.mkdir(parents=True);(home/'partial').write_bytes(b'keep')
        class Unknown:
            pid=992;returncode=None;calls=0;killed=False
            def communicate(self,timeout):
                self.calls+=1
                if self.calls==1:raise subprocess.TimeoutExpired(['fixture-setup'],timeout)
                self.returncode=-9;return b'',b''
            def kill(self):self.killed=True
        process=Unknown();result=self.root/'unknown-native-launch-result.json'
        with patch.object(m.tempfile,'mkdtemp',return_value=str(root)),patch.object(m.subprocess,'Popen',return_value=process) as launch,patch.object(m.shutil,'rmtree') as remove:
            with self.assertRaises(subprocess.TimeoutExpired):m.managed_lifecycle(self.home/'bin/acp-slack-adapter',{},result)
            launch.assert_called_once();remove.assert_not_called()
        self.assertTrue(process.killed);self.assertEqual(process.calls,2);self.assertEqual((home/'partial').read_bytes(),b'keep')
        saved=json.loads(result.with_name('unknown-native-manager-result.json').read_text());self.assertEqual(saved['events'][0]['outcome'],'unknown');self.assertFalse(saved['action_retry'])

    def test_managed_inactive_start_retains_exact_request_and_absent_witness_without_another_action(self):
        root=self.root/'managed-inactive';home=root/'installation';(home/'bin').mkdir(parents=True)
        installed=home/'bin/acp-slack-adapter';installed.write_bytes(b'fixed fixture')
        (home/'adapter.json').write_text(json.dumps({'enabled':False,'setup_complete':False,'acp':{'command':None}}))
        (home/'native-manager/requests').mkdir(parents=True)
        ident=str(uuid.uuid4());setup={'created':True,'serving':False,'executable':str(installed),'id':ident}
        def process(argv,**kwargs):
            if argv[3]=='native-setup':value=setup
            else:
                self.assertEqual(argv[3],'native-start');request=argv[-1]
                (home/'native-manager/active.json').write_text(json.dumps({'request_id':request}))
                (home/'native-manager/requests'/f'{request}.json').write_text(json.dumps({'request_id':request,'phase':'accepted','binding':{'id':ident}}))
                value={'schema':'com.microsoft.amplifier.slack-native-status/1','id':ident,'home':str(home),'executable':str(installed),'autostart':False,'startup_change_supported':False,'state':'inactive','profile_matches':True,'request_current':True,'launch_observed':False}
            return types.SimpleNamespace(pid=993,returncode=0,communicate=lambda timeout:(json.dumps(value).encode(),b''))
        result=self.root/'inactive-native-launch-result.json'
        with patch.object(m.tempfile,'mkdtemp',return_value=str(root)),patch.object(m.subprocess,'Popen',side_effect=process) as launch,patch.object(m.shutil,'rmtree') as remove:
            with self.assertRaises(AssertionError):m.managed_lifecycle(installed,{},result)
            self.assertEqual(launch.call_count,2);remove.assert_not_called()
        saved=json.loads(result.with_name('inactive-native-manager-result.json').read_text());request=saved['first_request_id']
        records=saved['manager_records']['records'];self.assertFalse(saved['passed']);self.assertTrue(saved['retained_home'])
        self.assertEqual(records[f'native-manager/requests/{request}.json']['projection']['phase'],'accepted')
        self.assertEqual(records[f'native-manager/requests/{request}.launched.json']['state'],'absent_at_read')
        self.assertEqual(saved['events'][-1]['result']['state'],'inactive')

if __name__ == '__main__':unittest.main()
