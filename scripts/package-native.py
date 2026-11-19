#!/usr/bin/env python3
"""Build self-contained native Slack Connector downloads from native CI binaries."""
import argparse
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import plistlib
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import zipfile
ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('python_support',ROOT/'vendor/python-support/python_support.py')
python_support=importlib.util.module_from_spec(spec);spec.loader.exec_module(python_support)
TARGETS={
'x86_64-unknown-linux-gnu':('linux','x86_64'),'aarch64-unknown-linux-gnu':('linux','aarch64'),
'x86_64-apple-darwin':('macos','x86_64'),'aarch64-apple-darwin':('macos','aarch64'),
'x86_64-pc-windows-msvc':('windows','x86_64'),'aarch64-pc-windows-msvc':('windows','aarch64')}
def digest(path):return hashlib.sha256(path.read_bytes()).hexdigest()
def verify_vendor():
    vendor=json.loads((ROOT/'vendor/python-support/vendor.json').read_text())
    for name,expected in vendor['files'].items():
        path=ROOT/'vendor/python-support'/name
        if not path.is_file() or path.is_symlink() or digest(path)!=expected:raise ValueError('Pinned private Python support changed')
def stage_windows_cli(built,private):
    if built is None:raise ValueError('Windows packaging requires the exact headless Slack CLI build')
    spec=importlib.util.spec_from_file_location('slack_cli_build',ROOT/'scripts/build-slack-cli.py')
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
    provenance=module.validate_built_output(built)
    destination=private/'slack-cli';destination.mkdir()
    for name in ('slack.exe','LICENSE','provenance.json'):
        shutil.copyfile(built/name,destination/name)
    if module.validate_built_output(destination)!=provenance:raise ValueError('Bundled Slack CLI changed during staging')
    return provenance
def package(binary,target,version,output,cache,slack_cli_build=None):
    verify_vendor()
    if target not in TARGETS or not re.fullmatch(r'\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.-]+)?',version):raise ValueError('Invalid target/version')
    if not binary.is_file() or binary.is_symlink():raise ValueError('Use a native CI executable')
    system,architecture=TARGETS[target]; output.mkdir(parents=True,exist_ok=True)
    guard=ROOT/'vendor/native-binary';meta=json.loads((guard/'vendor.json').read_text())
    if digest(guard/'native_binary.py')!=meta['sha256']:raise ValueError('Native binary validator changed')
    spec=importlib.util.spec_from_file_location('native_binary',guard/'native_binary.py');module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
    binary_report=module.inspect(binary.read_bytes(),system+'-'+architecture)
    if system=='macos':
        libraries=subprocess.check_output(['/usr/bin/otool','-L',str(binary)],text=True).splitlines()[1:]
        if any(not row.strip().split(' (',1)[0].startswith(('/usr/lib/','/System/Library/')) for row in libraries):raise ValueError('Unexpected external Mac dynamic library')
    with tempfile.TemporaryDirectory(prefix='slack-native-') as temp:
        stage=Path(temp)/'bundle';stage.mkdir()
        if system=='macos':
            content=stage/'Slack Connector.app/Contents';binroot=content/'MacOS';resources=content/'Resources'
            content.mkdir(parents=True)
            (content/'Info.plist').write_bytes(plistlib.dumps({'CFBundleName':'Slack Connector','CFBundleDisplayName':'Slack Connector','CFBundleIdentifier':'com.microsoft.constellation.slack','CFBundleExecutable':'acp-slack-adapter','CFBundleVersion':version,'CFBundleShortVersionString':version,'CFBundlePackageType':'APPL','LSMinimumSystemVersion':'11.0','LSUIElement':True}))
        else:binroot=stage;resources=stage
        binroot.mkdir(parents=True,exist_ok=True);resources.mkdir(parents=True,exist_ok=True)
        executable='acp-slack-adapter.exe' if system=='windows' else 'acp-slack-adapter'
        shutil.copyfile(binary,binroot/executable);(binroot/executable).chmod(0o755)
        private=resources/'runtime/python'
        python=python_support.prepare(target,cache,private)
        slack_cli=stage_windows_cli(slack_cli_build,private) if system=='windows' else None
        inventory={p.relative_to(private).as_posix():digest(p) for p in sorted(private.rglob('*')) if p.is_file()}
        manifest={'schema':1,'component':'acp-slack-adapter','version':version,'target':target,'python':python,'python_files':inventory}
        if slack_cli is not None:manifest['slack_cli']=slack_cli
        (resources/'native-bundle.json').write_text(json.dumps(manifest,indent=2)+'\n')
        tool_note=('Windows includes a pinned Slack CLI source build with a small fix for hidden setup. Its source and patch identities, plus the license, are included in the private runtime. Windows 11 ARM uses the x64 setup tool through Windows compatibility.\n' if system=='windows' else 'Slack authorization downloads the checksum-pinned official Slack CLI when needed.\n')
        (stage/'START-HERE.txt').write_text('Open Slack Connector.app on Mac, acp-slack-adapter.exe on Windows, or acp-slack-adapter on Linux.\nThe app opens its local first-account page. Choose your own connector password, then use the existing Slack and agent setup.\nNo Rust, Python, WSL, developer tools or system-wide Slack CLI installation is needed. Keep the download together.\n'+tool_note+'This preview is not publisher-signed or notarized. Verify its published checksum; do not disable system security protections.\nYour agent runtime and credentials remain your choice; creating this connector does not create a session or start work.\n')
        shutil.copyfile(ROOT/'LICENSE',stage/'LICENSE')
        filename=f'acp-slack-adapter-{version}-{target}'+('.tar.gz' if system=='linux' else '.zip');destination=output/filename
        if system=='linux':
            with tarfile.open(destination,'w:gz') as out:
                for p in sorted(stage.rglob('*')):
                    if p.is_file():out.add(p,arcname=p.relative_to(stage).as_posix(),recursive=False)
        else:
            with zipfile.ZipFile(destination,'w',compression=zipfile.ZIP_DEFLATED) as out:
                for p in sorted(stage.rglob('*')):
                    if not p.is_file():continue
                    item=zipfile.ZipInfo(p.relative_to(stage).as_posix());item.create_system=3;item.external_attr=(stat.S_IFREG|(0o755 if p.stat().st_mode&0o111 else 0o644))<<16;item.compress_type=zipfile.ZIP_DEFLATED
                    out.writestr(item,p.read_bytes())
        record={'schema':1,'component':'acp-slack-adapter','version':version,'target':target,'os':system,'architecture':architecture,'filename':filename,'entry':(binroot/executable).relative_to(stage).as_posix(),'sha256':digest(destination),'binary_sha256':digest(binary),'binary_metadata':binary_report,'python':python,'signed_or_notarized':False,'requires_developer_toolchain':False}
        if slack_cli is not None:record['slack_cli']=slack_cli
        (output/(filename+'.json')).write_text(json.dumps(record,indent=2)+'\n');(output/(filename+'.sha256')).write_text(record['sha256']+'  '+filename+'\n')
        return record
if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--binary',required=True,type=Path);p.add_argument('--target',required=True,choices=TARGETS);p.add_argument('--version',required=True);p.add_argument('--output',type=Path,default=Path('dist'));p.add_argument('--python-cache',type=Path,default=Path('.python-cache'));p.add_argument('--slack-cli-build',type=Path);a=p.parse_args();print(json.dumps(package(a.binary,a.target,a.version,a.output,a.python_cache,a.slack_cli_build),indent=2))
