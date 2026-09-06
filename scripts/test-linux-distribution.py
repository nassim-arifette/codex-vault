#!/usr/bin/env python3
"""Test the shipped Linux archive in an isolated profile; optionally test a real rollout copy."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import struct
import subprocess
import tarfile
import tempfile


def digest(path):
    result = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            result.update(block)
    return result.hexdigest()


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def static_elf(path):
    data = path.read_bytes()
    require(data[:6] == b'\x7fELF\x02\x01', 'Expected a little-endian 64-bit ELF')
    require(struct.unpack_from('<H', data, 18)[0] == 62, 'Expected x86_64')
    offset = struct.unpack_from('<Q', data, 32)[0]
    size, count = struct.unpack_from('<HH', data, 54)
    for i in range(count):
        entry = offset + i * size
        kind = struct.unpack_from('<I', data, entry)[0]
        require(kind != 3, 'Unexpected dynamic ELF interpreter')
        if kind == 2:
            start = struct.unpack_from('<Q', data, entry + 8)[0]
            length = struct.unpack_from('<Q', data, entry + 32)[0]
            for pos in range(start, start + length, 16):
                tag = struct.unpack_from('<q', data, pos)[0]
                if tag == 0:
                    break
                require(tag != 1, 'Unexpected external shared library')


def seed(path, project):
    records = [dict(type='session_meta', payload=dict(id='linux-test', cwd=str(project), cli_version='0.152.1'))]
    for turn in ['old', 'new']:
        if turn == 'new':
            records.append(dict(type='compacted', payload=dict(window_number=3, replacement_history=[{'role': 'user'}])))
        records.extend([
            dict(type='event_msg', payload=dict(type='turn_started', turn_id=turn)),
            dict(type='turn_context', payload=dict(turn_id=turn, model='gpt')),
            dict(type='event_msg', payload=dict(type='user_message', message='authentication café 🦀 ' + ('x' * 2_000_000 if turn == 'old' else 'recent message'))),
            dict(type='event_msg', payload=dict(type='turn_complete', turn_id=turn)),
        ])
    path.write_text(''.join(json.dumps(row, ensure_ascii=False) + '\n' for row in records), encoding='utf-8')


def test(archive, real_rollout, expect_9p_refusal=False):
    require(platform.system() == 'Linux', 'Run this check on Linux or WSL2')
    archive = archive.resolve(strict=True)
    checksum_file = archive.with_name('SHA256SUMS-linux.txt')
    if not checksum_file.exists():
        checksum_file = archive.with_name('SHA256SUMS.txt')
    hashes = [line.split()[0] for line in checksum_file.read_text().splitlines()
              if len(line.split()) == 2 and line.split()[1].lstrip('*') == archive.name]
    require(hashes == [digest(archive)], 'Archive checksum mismatch or missing entry')
    source_hash = digest(real_rollout) if real_rollout else None
    with tempfile.TemporaryDirectory(prefix='codex-vault-linux-test-') as temporary:
        root = Path(temporary)
        unpacked = root / 'unpacked'
        unpacked.mkdir()
        with tarfile.open(archive) as bundle:
            for member in bundle.getmembers():
                require(member.isfile() or member.isdir(), 'Unsupported archive entry')
                require((unpacked / member.name).resolve().is_relative_to(unpacked), 'Unsafe archive path')
            bundle.extractall(unpacked, filter='data')
        installed = root / 'installed cli'
        subprocess.run(['sh', str(unpacked / 'install.sh'), str(installed)], check=True, capture_output=True)
        binary = installed / 'codex-vault'
        require(digest(binary) == digest(unpacked / 'codex-vault'), 'Installed executable mismatch')
        static_elf(binary)
        # A failed update must not replace the already installed executable.
        installed_hash = digest(binary)
        with (unpacked / 'codex-vault').open('ab') as tampered:
            tampered.write(b'checksum-test')
        rejected = subprocess.run(['sh', str(unpacked / 'install.sh'), str(installed)], capture_output=True)
        require(rejected.returncode != 0 and digest(binary) == installed_hash,
                'Installer accepted a corrupt executable or changed the existing installation')
        env = dict(os.environ, CODEX_HOME=str(root / 'codex'), CODEX_VAULT_HOME=str(root / 'vault'))

        def invoke(*args, json_output=True):
            flags = ['--json', '--no-progress'] if json_output else []
            output = subprocess.run([str(binary), *flags, *map(str, args)], env=env,
                                    capture_output=True, text=True, encoding='utf-8')
            require(output.returncode == 0,
                    f'CLI command {args[0]} failed ({output.returncode}): {output.stderr or output.stdout}')
            return json.loads(output.stdout) if json_output else output.stdout

        version = invoke('--version', json_output=False).strip()
        require('--all' in invoke('scan', '--help', json_output=False), 'Missing scan options')
        session = root / 'codex' / 'sessions' / 'rollout-linux-test.jsonl'
        session.parent.mkdir(parents=True)
        if real_rollout:
            shutil.copyfile(real_rollout, session)
        else:
            seed(session, root / 'sample-project')
        if not expect_9p_refusal:
            session.chmod(0o640)
        before_hash = digest(session)
        before_bytes = session.stat().st_size
        require(len(invoke('scan')['sessions']) == 1, 'Isolated discovery failed')
        require('Showing 1 of 1' in invoke('--human', 'scan', json_output=False), 'Readable scan failed')
        invoke('archive', session)
        invoke('doctor', session, '--deep')
        invoke('compact', session, '--dry-run')
        if expect_9p_refusal:
            vault = root / 'vault'
            def snapshots():
                return {str(p.relative_to(vault)): digest(p) for p in vault.rglob('*') if p.is_file()}
            saved = snapshots()
            for command in [('compact', str(session)), ('restore', str(session), '--original')]:
                result = subprocess.run([str(binary), '--json', *command], env=env,
                                        capture_output=True, text=True, encoding='utf-8')
                require(result.returncode == 2, 'Expected an unsupported-filesystem refusal')
                error = json.loads(result.stderr)
                require(error['code'] == 'invalid_input' and '9p/DrvFS' in error['message'],
                        'Unexpected refusal reason')
                require(digest(session) == before_hash and snapshots() == saved,
                        'Refused operation changed the transcript or recovery files')
            return dict(version=version, platform='Linux x86_64', case='9p-refusal',
                        executable_sha256=digest(binary), compact_refused_before_mutation=True,
                        restore_refused_before_mutation=True, transcript_and_vault_unchanged=True, passed=True)
        invoke('compact', session)
        compacted_bytes = session.stat().st_size
        require(compacted_bytes < before_bytes, 'Compaction did not shrink the fixture')
        require(session.stat().st_mode & 0o777 == 0o640, 'Compaction changed transcript permissions')
        invoke('doctor', session, '--deep')
        if not real_rollout:
            invoke('index')
            matches = invoke('search', 'authentication')['matches']
            require(bool(matches), 'Bundled SQLite search failed')
            passage = invoke('read', matches[0]['id'])
            require('authentication' in passage['text'], 'Verified passage read failed')
        invoke('restore', session, '--original')
        require(digest(session) == before_hash, 'Restoration was not byte-exact')
        require(session.stat().st_mode & 0o777 == 0o640, 'Restore changed transcript permissions')
        invoke('doctor', session, '--deep')
        if real_rollout:
            require(digest(real_rollout) == source_hash == before_hash, 'Original source changed')
        else:
            invoke('index', '--rebuild')
            require(bool(invoke('search', 'authentication')['matches']), 'Search after restoration failed')
        vault = root / 'vault'
        for entry in [vault, *vault.rglob('*')]:
            require(entry.stat().st_mode & 0o077 == 0, 'Vault content is accessible to group/other')
        return dict(version=version, platform='Linux x86_64', kernel=platform.release(),
                    case='real-rollout-copy' if real_rollout else 'synthetic', static_elf=True,
                    executable_sha256=digest(binary), installed_from_archive=True,
                    original_bytes=before_bytes, compacted_bytes=compacted_bytes,
                    sha256_restore_exact=True, unix_permissions_verified=True,
                    source_unchanged=True if real_rollout else None,
                    sqlite_search_verified=None if real_rollout else True, passed=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('archive', type=Path)
    parser.add_argument('--real-rollout', type=Path, help='Read-only source; operations run on a temporary copy')
    parser.add_argument('--report', type=Path, help='Write an anonymous JSON result')
    parser.add_argument('--expect-9p-refusal', action='store_true', help='Test safe refusal; requires a temporary directory on a 9p/DrvFS mount')
    args = parser.parse_args()
    result = test(args.archive, args.real_rollout, args.expect_9p_refusal)
    text = json.dumps(result, indent=2) + '\n'
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(text, encoding='utf-8')
    print(text, end='')
