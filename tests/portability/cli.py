# /// script
# requires-python = ">=3.11"
# dependencies = ["boto3==1.44.3"]
# ///
"""Tiny Seaweed workflow and closed ordinary DB migration; never real AWS."""
import argparse
import contextlib
import hashlib
import json
import os
import pathlib
import select
import shutil
import socket
import socketserver
import subprocess
import tempfile
import threading
import tomllib
import uuid

import boto3
from botocore.config import Config

ENDPOINT = 'http://127.0.0.1:8333'
BUCKET = 'private-archive-test'
CONTAINER = 'private-archive-p2-arm64-runner'
S3_CONTAINER = 'private-archive-s3-tests-s3-1'
ADMIN = boto3.client('s3', endpoint_url='http://127.0.0.1:18333', region_name='us-east-1',
    aws_access_key_id='private-archive-test', aws_secret_access_key='private-archive-test-secret',
    config=Config(retries={'total_max_attempts': 1}, connect_timeout=2, read_timeout=20, s3={'addressing_style': 'path'}))


class Forward(socketserver.BaseRequestHandler):
    def handle(self):
        with socket.create_connection(('127.0.0.1', 18333), timeout=30) as remote:
            peers = {self.request: remote, remote: self.request}
            while True:
                ready, _, _ = select.select(list(peers), [], [], 30)
                if not ready:
                    return
                for source in ready:
                    block = source.recv(65536)
                    if not block:
                        return
                    peers[source].sendall(block)


@contextlib.contextmanager
def forwarding():
    class Server(socketserver.ThreadingTCPServer):
        allow_reuse_address = True
        daemon_threads = True
    with Server(('127.0.0.1', 8333), Forward) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield
        finally:
            server.shutdown()
            thread.join()


def docker(*args):
    return subprocess.run(['docker', *args], check=True, capture_output=True, text=True, timeout=60).stdout


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=pathlib.Path, required=True)
    parser.add_argument('--arm-binary', type=pathlib.Path)
    parser.add_argument('--report', type=pathlib.Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    passed, details = [], {}
    prefix = 'p2-portability-' + uuid.uuid4().hex + '/'
    data_prefix = prefix + 'data/'
    root = f's3://{BUCKET}/{data_prefix}'
    with tempfile.TemporaryDirectory(prefix='private-archive-portability-') as directory, forwarding():
        work = pathlib.Path(directory).resolve()
        env = {k: v for k, v in os.environ.items() if not k.startswith('AWS_')}
        env['HOME'] = str(work / 'home')
        arm_started = False
        try:
            if args.arm_binary:
                target = work / 'private-archive-arm64'
                shutil.copyfile(args.arm_binary, target)
                target.chmod(0o755)
                header = target.read_bytes()[:20]
                assert header[:5] == b'\x7fELF\x02' and int.from_bytes(header[18:20], 'little') == 183
                docker('run', '--rm', '-d', '--platform', 'linux/arm64', '--name', CONTAINER,
                    '--network', 'container:' + S3_CONTAINER, '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
                    '--memory', '512m', '--cpus', '1', '--mount', f'type=bind,src={work},dst=/work',
                    'private-archive-build-arm64:1.99.1', 'sleep', 'infinity')
                arm_started = True
                details['arm_uname'] = docker('exec', CONTAINER, 'uname', '-sm').strip()
                details['arm_ldd'] = docker('exec', CONTAINER, 'ldd', '/work/private-archive-arm64').strip()
                assert details['arm_uname'] == 'Linux aarch64'
                assert not any(lib in details['arm_ldd'] for lib in ['libsqlite', 'libssl', 'libcrypto'])

            def run(role, argv, expected=0, fixture=True):
                config = work / 'config.toml'
                saved = tomllib.loads(config.read_text()) if config.exists() else {}
                location = saved.get('index', {}).get('location', 'index.db')
                shared = ['--config', 'config.toml']
                # The fixture build requires both explicit paths; use exactly
                # the saved value. cfg itself is tested without any override.
                if fixture:
                    shared += ['--test-endpoint', ENDPOINT, '--index-location', location]
                if role == 'arm':
                    command = ['docker', 'exec', '-w', '/work', '-e', 'HOME=/work/home', CONTAINER,
                        '/work/private-archive-arm64', *shared, *argv]
                else:
                    command = [str(binary), *shared, *argv]
                proc = subprocess.run(command, cwd=work, env=env, capture_output=True, timeout=90)
                assert proc.returncode == expected, (role, argv, proc.returncode, proc.stderr.decode())
                return proc.stdout

            other = 'arm' if arm_started else 'mac'
            def cfg(key, value, role='mac'):
                run(role, ['cfg', 'set', key, value], fixture=False)
                group, field = key.split('.')
                assert tomllib.loads((work / 'config.toml').read_text())[group][field] == value
                effective = json.loads(run(role, ['cfg', 'get', key], fixture=False))
                expected = str(pathlib.Path('/work' if role == 'arm' else work) / value) if key == 'index.location' and not value.startswith('s3://') else value
                assert effective == expected, (effective, expected)

            run('mac', ['cfg', 'init'], fixture=False)
            cfg('index.location', 'index.db')
            cfg('copy.data-class', 'standard')
            cfg('logging.level', 'warn')
            source = work / 'source'
            (source / '.git').mkdir(parents=True)
            payload = b'private-archive portability\x00\xff\n'
            (source / 'data.bin').write_bytes(payload)
            (source / '.DS_Store').write_bytes(b'visible unless explicitly ignored')
            (source / '.git/config').write_bytes(b'also selected')
            run('mac', ['cp', 'source', root, '-r'])
            rows = json.loads(run(other, ['ls', root, '-r', '--json']))
            assert len(rows) == 3 and all(len(row) == 10 for row in rows)
            assert any(row['uri'].endswith('/.git/config') for row in rows)
            passed.append('explicit cfg init/edit and Mac upload include hidden names; second platform reads the same local DB/settings')

            run(other, ['cp', 'source/data.bin', root + 'data.bin'])
            run(other, ['cp', root + 'data.bin', root + 'copy.bin'])
            run(other, ['cp', root + 'copy.bin', 'download.bin'])
            assert (work / 'download.bin').read_bytes() == payload
            run(other, ['reconcile', root, '-r'])
            rows = json.loads(run('mac', ['ls', root, '-r', '--sort', 'size-asc,name-desc', '--json']))
            assert len(rows) == 4
            passed.append('second platform performs S3 copy/download/reconcile; body digest and shared sorted observations match')

            # Every product process has exited: this is an ordinary external
            # binary transfer of a closed, checkpointed DB, without an envelope.
            before = (work / 'index.db').read_bytes()
            assert before.startswith(b'SQLite format 3\0')
            index_key = prefix + 'index.db'
            ADMIN.put_object(Bucket=BUCKET, Key=index_key, Body=before, StorageClass='STANDARD')
            cfg('index.location', f's3://{BUCKET}/{index_key}')
            assert json.loads(run('mac', ['ls', root, '-r', '--json'], 1)) == []
            run(other, ['reconcile', root, '-r'])
            copied = json.loads(run('mac', ['ls', root, '-r', '--json']))
            assert len(copied) == 4
            remote = ADMIN.get_object(Bucket=BUCKET, Key=index_key)['Body'].read()
            assert remote.startswith(b'SQLite format 3\0')
            (work / 'manual.db').write_bytes(remote)
            cfg('index.location', 'manual.db', other)
            assert json.loads(run('mac', ['ls', root, '-r', '--json'])) == copied
            assert json.loads(run(other, ['ls', root, '-r', '--json'])) == copied
            details['roundtrip_sha256'] = hashlib.sha256(remote).hexdigest()
            passed.append('closed local DB externally uploaded, used as S3 index, externally downloaded and reopened by location change with identical observations')

            ADMIN.put_object(Bucket=BUCKET, Key=data_prefix + 'external.bin', Body=b'external')
            ADMIN.delete_object(Bucket=BUCKET, Key=data_prefix + '.DS_Store')
            assert json.loads(run('mac', ['ls', root, '-r', '--json'])) == copied
            run(other, ['reconcile', root, '-r'])
            refreshed = json.loads(run('mac', ['ls', root, '-r', '--json']))
            assert len(refreshed) == 4 and any(r['uri'].endswith('/external.bin') for r in refreshed)
            assert not any(r['uri'].endswith('/.DS_Store') for r in refreshed)
            passed.append('external add/delete remain stale until explicit reconcile; shared copy then reflects both')

            run(other, ['rm', root, '-r', '--auto-approve'])
            assert json.loads(run('mac', ['ls', '--json'], 1)) == []
            (work / 'broken.db').write_bytes(b'broken ordinary index')
            cfg('index.location', 'broken.db')
            run('mac', ['ls', '--json'], 1)
            assert (work / 'broken.db').read_bytes() == b'broken ordinary index'
            cfg('index.location', 'recovered.db')
            run('mac', ['reconcile', root, '-r'])
            assert json.loads(run('mac', ['ls', '--json'], 1)) == []
            passed.append('confirmed delete removes final root; corruption is preserved and explicit new location/reconcile provides a fresh copy')
        finally:
            if arm_started:
                docker('stop', '-t', '2', CONTAINER)
            for page in ADMIN.get_paginator('list_objects_v2').paginate(Bucket=BUCKET, Prefix=prefix):
                for item in page.get('Contents', []):
                    ADMIN.delete_object(Bucket=BUCKET, Key=item['Key'])
            assert ADMIN.list_objects_v2(Bucket=BUCKET, Prefix=prefix).get('KeyCount', 0) == 0
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(dict(count=len(passed), passed=passed, arm_executed=bool(args.arm_binary), details=details,
        real_aws_requests=0, test_prefix_empty=True), indent=2) + '\n')
    print(json.dumps(dict(passed=len(passed), arm_executed=bool(args.arm_binary), report=str(args.report))))


if __name__ == '__main__':
    main()
