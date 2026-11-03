#!/usr/bin/env python3
"""Install one independent adapter. Does not install or modify its chosen agent."""
import argparse
import json
import os
import platform
from pathlib import Path
import shutil
import subprocess
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def run(*args, **kwargs):
    return subprocess.run([str(a) for a in args], check=True, **kwargs)


def quoted(value):
    return '"' + str(value).replace('\\', '\\\\').replace('"', '\\"').replace('%', '%%').replace('$', '$$') + '"'


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--home', type=Path)
    p.add_argument('--label', default='Slack connection')
    p.add_argument('--agent-home', type=Path, help='Optional convenience: use an existing Agent Instance ACP entrypoint')
    p.add_argument('--bind', default='0.0.0.0')
    p.add_argument('--port', type=int, default=0)
    p.add_argument('--public-url', help='Already configured HTTPS proxy address, such as Tailscale Serve')
    p.add_argument('--no-open', action='store_true')
    p.add_argument('--binary', type=Path, help='Install an already built release binary')
    a = p.parse_args()
    if platform.system() != 'Linux':
        p.error('The first service installer supports Linux. Other platforms can use its web interface.')
    owner = subprocess.check_output(['id', '-un'], text=True).strip()
    if owner == 'root':
        p.error('Run as the account that will own this connection, not as root.')
    install_home = (a.home or Path.home()/'.local/share/acp-slack-adapter'/('connection-'+time.strftime('%Y%m%d-%H%M%S'))).expanduser().absolute()
    if any(ch in str(install_home) for ch in '\n\r\t'):
        p.error('The installation path must not contain line breaks or tabs.')
    if (install_home/'adapter.json').exists():
        p.error('This installation already exists; use the CLI or web interface to change its configuration.')
    if not shutil.which('systemctl'):
        p.error('A systemd user service is required by this installer.')
    if not a.binary:
        if shutil.which('apt-get'):
            missing = [name for name in ('build-essential','pkg-config','libpam0g-dev','libclang-dev')
                       if subprocess.run(['dpkg-query','-W','-f=${Status}',name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL).returncode]
            if missing:
                run('sudo','apt-get','update')
                run('sudo','apt-get','install','-y',*missing)
        cargo = shutil.which('cargo') or str(Path.home()/'.cargo/bin/cargo')
        if not Path(cargo).is_file():
            script = urllib.request.urlopen('https://sh.rustup.rs', timeout=30).read()
            run('sh','-s','--','-y','--profile','minimal', input=script)
            cargo = str(Path.home()/'.cargo/bin/cargo')
        run(cargo,'build','--release','--locked',cwd=ROOT)
    binary = (a.binary or ROOT/'target/release/acp-slack-adapter').absolute()
    pam = Path('/etc/pam.d/acp-slack-adapter')
    expected = '# Slack adapter: authenticate only the installing account using host policy.\n@include common-auth\n@include common-account\n'
    if not pam.exists():
        if not Path('/etc/pam.d/common-auth').exists():
            p.error('Install a host-appropriate PAM policy at /etc/pam.d/acp-slack-adapter, then rerun.')
        temporary = ROOT/'ops/pam/acp-slack-adapter'
        run('sudo','install','-m','0644',temporary,pam)
    args = [binary,'init','--home',install_home,'--label',a.label,'--owner',owner,'--bind',a.bind,'--port',str(a.port)]
    if a.agent_home:
        args += ['--agent-home',a.agent_home.expanduser().absolute()]
    run(*args)
    config_path = install_home/'adapter.json'
    config = json.loads(config_path.read_text())
    if a.public_url:
        config['server']['public_url'] = a.public_url
        config_path.write_text(json.dumps(config,indent=2)+'\n')
        run(install_home/'bin/acp-slack-adapter','config','get','--home',install_home,stdout=subprocess.DEVNULL)
    unit = 'acp-slack-adapter-'+config['id']+'.service'
    units = Path.home()/'.config/systemd/user'
    units.mkdir(parents=True,exist_ok=True)
    service = '\n'.join([
        '[Unit]','Description=Standalone Slack to ACP adapter','After=network-online.target','',
        '[Service]','Type=simple',
        'ExecStart='+quoted(install_home/'bin/acp-slack-adapter')+' serve --home '+quoted(install_home),
        'WorkingDirectory='+str(install_home).replace('%','%%'),'Restart=on-failure','RestartSec=5',
        'UMask=0077','KillMode=control-group','TimeoutStopSec=15','',
        '[Install]','WantedBy=default.target',''])
    (units/unit).write_text(service)
    (install_home/'adapter.service').write_text(service)
    run('systemctl','--user','daemon-reload')
    (install_home/'installation.json').write_text(json.dumps({'schema':1,'repository':'microsoft/amplifier-acp-slack','service_kind':'systemd-user','service':unit,'binary':'bin/acp-slack-adapter'},indent=2)+'\n')
    run('systemctl','--user','enable','--now',unit)
    endpoints = install_home/'endpoints.json'
    for _ in range(100):
        if endpoints.exists():
            break
        time.sleep(.2)
    if not endpoints.exists():
        raise SystemExit('Service did not become ready. Inspect: journalctl --user -u '+unit)
    info = json.loads(endpoints.read_text())
    candidates = [u for u in info['urls'] if '127.0.0.1' not in u and '[::1]' not in u]
    preferred = a.public_url or (candidates[0] if candidates else info['base_url'])
    print('\nYour Slack connection is installed. Open '+preferred)
    print('Sign in as '+owner+'. Connection setup stays paused until you finish.')
    if not a.public_url:
        print('Direct LAN HTTPS uses this installation’s local certificate. A trusted HTTPS proxy can be configured separately.')
    print('All available addresses: '+', '.join(info['urls']))
    print('Home: '+str(install_home))
    if not a.no_open and (os.environ.get('DISPLAY') or os.environ.get('WAYLAND_DISPLAY')) and shutil.which('xdg-open'):
        subprocess.Popen(['xdg-open',preferred],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)


if __name__ == '__main__':
    main()
