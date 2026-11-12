"""Check the complete six-platform native bundle set before publication."""
import hashlib,json,sys
from pathlib import Path
root=Path(sys.argv[1])/'dist';version=sys.argv[2]
expected={'x86_64-unknown-linux-gnu','aarch64-unknown-linux-gnu','x86_64-apple-darwin','aarch64-apple-darwin','x86_64-pc-windows-msvc','aarch64-pc-windows-msvc'}
seen=set()
for path in root.glob('*.json'):
    value=json.loads(path.read_text());target=value['target']
    assert value['component']=='acp-slack-adapter' and value['version']==version and target in expected and target not in seen
    file=root/value['filename'];assert file.parent==root and file.is_file()
    assert hashlib.sha256(file.read_bytes()).hexdigest()==value['sha256']
    assert (root/(file.name+'.sha256')).read_text().strip()==value['sha256']+'  '+file.name
    seen.add(target)
assert seen==expected and len(list(root.iterdir()))==18
print('All six exact-version native release bundles and checksums verified')
