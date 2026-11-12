"""Build-time support for privately bundled CPython, shared by family packages.

This module performs no system installation, account change or service action.
Vendor it with lock.json and notices/ from an exact umbrella source revision.
"""
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import posixpath
import shutil
import tarfile
import tempfile
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parent

def digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def safe_name(name):
    parts = PurePosixPath(name).parts
    if not parts or name.startswith('/') or '\\' in name or ':' in name or '..' in parts:
        raise ValueError('Unsafe archive path')
    return '/'.join(parts)


def unpack_python(archive, destination):
    """Resolve only internal distributor links; emit ordinary portable files."""
    # Decompress once: resolving internal aliases must not repeatedly seek
    # backwards through gzip (which made release preparation unnecessarily slow).
    with tempfile.TemporaryFile() as spool:
        with gzip.open(archive, 'rb') as compressed:
            total = 0
            while block := compressed.read(1024 * 1024):
                total += len(block)
                if total > 2 * 1024**3:
                    raise ValueError('Python archive expands beyond its bound')
                spool.write(block)
        spool.seek(0)
        with tarfile.open(fileobj=spool, mode='r:') as source:
            entries = {}
            for member in source.getmembers():
                name = safe_name(member.name)
                if not name.startswith('python/') and name != 'python':
                    raise ValueError('Python archive has an unexpected root')
                if name in entries:
                    raise ValueError('Duplicate Python archive entry')
                if not (member.isfile() or member.isdir() or member.issym() or member.islnk()):
                    raise ValueError('Unsupported Python archive entry')
                entries[name] = member
            if sum(m.size for m in entries.values()) > 2 * 1024**3:
                raise ValueError('Python archive expands beyond its bound')
            for name, member in entries.items():
                if member.isdir():
                    continue
                resolved = member
                visited = {name}
                while resolved.issym() or resolved.islnk():
                    link = resolved.linkname
                    if link.startswith('/') or '\\' in link or ':' in link:
                        raise ValueError('Unsafe Python archive link')
                    target = posixpath.normpath(posixpath.join(posixpath.dirname(resolved.name), link)
                                               if resolved.issym() else link)
                    if not target.startswith('python/') or target not in entries or target in visited:
                        raise ValueError('Escaping, missing or cyclic Python archive link')
                    visited.add(target)
                    resolved = entries[target]
                if not resolved.isfile():
                    raise ValueError('Python archive link does not identify a file')
                target = destination / name.removeprefix('python/')
                target.parent.mkdir(parents=True, exist_ok=True)
                with source.extractfile(resolved) as incoming, target.open('xb') as output:
                    while block := incoming.read(1024 * 1024):
                        output.write(block)
                target.chmod(0o755 if resolved.mode & 0o111 else 0o644)


def download(asset, cache):
    cache.mkdir(parents=True, exist_ok=True)
    if Path(asset['filename']).name != asset['filename'] or '\\' in asset['filename']:
        raise ValueError('Invalid private Python archive filename')
    target = cache / asset['filename']
    if target.is_symlink():
        raise ValueError('Private Python cache archive must be an ordinary file')
    if not target.exists():
        request = urllib.request.Request(asset['url'], headers={'User-Agent': 'Constellation-release'})
        temporary = cache / ('.download-' + uuid.uuid4().hex)
        try:
            with urllib.request.urlopen(request, timeout=120) as incoming, temporary.open('xb') as out:
                count = 0
                while block := incoming.read(1024 * 1024):
                    count += len(block)
                    if count > asset['size']:
                        raise ValueError('Python archive exceeds the pinned size')
                    out.write(block)
            if digest(temporary) != asset['sha256']:
                raise ValueError('Python distributor checksum mismatch')
            os.replace(temporary, target)
        finally:
            temporary.unlink(missing_ok=True)
    if target.stat().st_size != asset['size'] or digest(target) != asset['sha256']:
        raise ValueError('Python distributor checksum mismatch')
    return target


def asset_for(target):
    lock = json.loads((ROOT / 'lock.json').read_text())
    asset = next((a for a in lock['assets'] if a['target'] == target), None)
    if asset is None:
        raise ValueError('No pinned private Python for this platform')
    return lock, asset


def prepare(target, cache, destination):
    """Produce ordinary files at a new destination and retain every notice."""
    lock, asset = asset_for(target)
    if destination.exists() and (destination.is_symlink() or any(destination.iterdir())):
        raise ValueError('Use a new or empty private Python destination')
    for name, expected in asset['notices'].items():
        notice = ROOT / 'notices' / target / name
        if not notice.is_file() or notice.is_symlink() or digest(notice) != expected:
            raise ValueError('Distributor notice is missing or changed')
    archive = download(asset, cache)
    unpack_python(archive, destination)
    for cached in destination.rglob('*.pyc'):
        cached.unlink()
    if not (destination / asset['executable']).is_file():
        raise ValueError('Pinned Python executable is missing')
    notices = destination / 'distribution-licenses'
    notices.mkdir(exist_ok=True)
    for name in asset['notices']:
        shutil.copyfile(ROOT / 'notices' / target / name, notices / name)
    return {'directory': lock['directory'], 'version': lock['python_version'],
            'target': target, 'executable': asset['executable'],
            'archive_sha256': asset['sha256'], 'crt_features': asset['crt_features'],
            'minimum_macos': asset['minimum_macos']}
