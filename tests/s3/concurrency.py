"""Independent CLI processes share only the configured DB/cache; loopback only."""
import argparse
import fcntl
import hashlib
import json
import pathlib
import sqlite3
import subprocess
import tempfile
import threading
import urllib.parse

import faults
from guards import Handler
from index_files import chunks
from restore import listing


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=pathlib.Path, required=True)
    parser.add_argument('--report', type=pathlib.Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    passed = []
    phase = dict(db=None, etag='"initial"', objects={'a/new': 'standard'}, gets=0,
                 hold=None, barrier=None, puts=[])
    lock = threading.Lock()
    entered, release = threading.Event(), threading.Event()

    def route(r, state):
        url = urllib.parse.urlsplit(r['path'])
        key = urllib.parse.unquote(url.path).removeprefix('/bucket/')
        if url.path == '/index-bucket/index.db':
            if r['method'] == 'GET':
                with lock:
                    body, etag = phase['db'], phase['etag']
                    phase['gets'] += 1
                    number, hold, barrier = phase['gets'], phase['hold'], phase['barrier']
                # Response owns the version captured when the request arrived.
                if hold == number:
                    entered.set()
                    assert release.wait(20), 'test failed to release held GET'
                if barrier:
                    barrier.wait(20)
                return faults.response(304) if r['condition'] == etag else faults.response(body=body, headers={'ETag': etag})
            if r['method'] == 'PUT':
                body = chunks(r['body']) if 'aws-chunked' in (r['encoding'] or '') else r['body']
                assert body.startswith(b'SQLite format 3\0')
                with lock:
                    accepted = r['condition'] == phase['etag']
                    phase['puts'].append((r['condition'], accepted))
                    if accepted:
                        phase['db'], phase['etag'] = body, '"' + hashlib.sha256(body).hexdigest() + '"'
                    etag = phase['etag']
                return faults.response(headers={'ETag': etag}) if accepted else faults.error(412)
        if r['method'] == 'GET' and 'list-type=2' in url.query:
            prefix = urllib.parse.parse_qs(url.query).get('prefix', [''])[0]
            with lock:
                objects = {k: v for k, v in phase['objects'].items() if k.startswith(prefix)}
            return faults.response(body=listing(objects))
        if r['method'] == 'HEAD':
            return faults.head(1, ETag='"data"') if key in phase['objects'] else faults.response(404)
        if r['method'] == 'PUT':
            with lock:
                phase['objects'][key] = 'standard'
            return faults.response(headers={'ETag': '"data"'})
        raise AssertionError(r)

    with tempfile.TemporaryDirectory(prefix='private-archive-concurrency-') as directory:
        work = pathlib.Path(directory)
        (work / 'config').write_text('')
        (work / 'source').write_bytes(b'x')
        with faults.fixture(route, Handler) as (state, endpoint):
            faults.run(binary, endpoint, work, ['ls', '--json'], 1)
            initial = (work / 'index.db').read_bytes()
            phase['db'] = initial

            def command(argv):
                cmd = faults.command(binary, endpoint, work, argv)
                cmd[cmd.index('--index-location') + 1] = 's3://index-bucket/index.db'
                return cmd

            def start(argv):
                return subprocess.Popen(command(argv), cwd=work, env=faults.environment(work), stdout=subprocess.PIPE, stderr=subprocess.PIPE)

            def finish(proc, expected=0):
                stdout, stderr = proc.communicate(timeout=30)
                assert proc.returncode == expected, (proc.returncode, stdout.decode(), stderr.decode())
                return stdout, stderr

            def run(argv, expected=0):
                return finish(start(argv), expected)

            def keys(expected=0):
                before = len(state.seen)
                output, _ = run(['ls', 's3://bucket', '-r', '--json'], expected)
                assert len(state.seen) == before, 'local browse made S3 requests'
                return [r['uri'] for r in json.loads(output)]

            def remote_keys():
                path = work / 'received.db'
                path.write_bytes(phase['db'])
                with sqlite3.connect(path) as db:
                    assert db.execute('PRAGMA integrity_check').fetchone() == ('ok',)
                    return [bytes(r[0]).decode() for r in db.execute('SELECT object_key FROM objects ORDER BY object_key')]

            # Import captures its revision before waiting for GET. A separate
            # process can browse/write/save while that original GET is blocked.
            phase.update(hold=1, gets=0)
            old = start(['reconcile', 's3://bucket/a', '-r'])
            assert entered.wait(15)
            assert keys(1) == []
            run(['cp', str(work / 'source'), 's3://bucket/w/file'])
            assert remote_keys() == ['w/file']
            release.set()
            _, stderr = finish(old, 1)
            assert b'index-file-put' in stderr
            assert keys() == ['s3://bucket/a/new', 's3://bucket/w/file']
            assert remote_keys() == ['w/file']
            assert phase['puts'] == [('"initial"', True), ('"initial"', False)], phase['puts']
            passed.append('held import allows independent local ls/cp; post-start upsert survives import/prune; stale conditional save fails once')
            phase.update(hold=None)
            run(['reconcile', 's3://bucket/w', '-r'])
            assert keys() == ['s3://bucket/w/file'] and remote_keys() == ['w/file']
            passed.append('next invocation fetches authoritative copy and does not replay previous unsent changes')
            before = len(state.seen)
            readers = [start(['ls', 's3://bucket', '-r', '--json']) for _ in range(4)]
            results = [json.loads(finish(proc)[0]) for proc in readers]
            assert all(result == results[0] for result in results)
            assert len(state.seen) == before
            passed.append('four simultaneous local readers return the same shared copy without S3 requests')

            cache = next((work / 'home/.private-archive/cache/index').glob('*/working.db')).parent
            def clear_transfer_cache():
                (cache / 'transfer.json').unlink(missing_ok=True)
                for path in cache.glob('snapshot-*'):
                    path.unlink()

            # Both copies capture the same ETag, but must not lock whole data
            # operations or retry/merge the losing index write.
            clear_transfer_cache()
            phase.update(barrier=threading.Barrier(2), puts=[])
            procs = [start(['cp', str(work / 'source'), 's3://bucket/' + key]) for key in ['parallel/one', 'parallel/two']]
            outputs = [finish(proc) for proc in procs]
            phase['barrier'] = None
            assert len(phase['puts']) == 2 and sum(ok for _, ok in phase['puts']) == 1
            assert sum(b'index-file-put' in err for _, err in outputs) == 1
            assert all(key in phase['objects'] for key in ['parallel/one', 'parallel/two'])
            assert all(out.startswith(b'copy ') for out, _ in outputs)
            passed.append('parallel copies both succeed in data store; exactly one conditional index save wins; loser warns without retries or command lock')

            # Force simultaneous publishers. Only the manifest-referenced
            # immutable snapshot should remain once all processes have ended.
            for _ in range(3):
                clear_transfer_cache()
                phase.update(db=initial, etag='"empty"', barrier=threading.Barrier(4), puts=[])
                procs = [start(['reconcile', 's3://bucket/empty-' + str(n), '-r']) for n in range(4)]
                for proc in procs:
                    finish(proc)
                phase['barrier'] = None
                manifest = json.loads((cache / 'transfer.json').read_text())
                snapshots = list(cache.glob('snapshot-*'))
                assert [p.name for p in snapshots] == [manifest['snapshot']], snapshots
                assert hashlib.sha256(snapshots[0].read_bytes()).hexdigest() == manifest['sha256']
                assert manifest['etag'] == '"empty"'
                assert phase['puts'] == [], phase['puts']
            passed.append('four concurrent cache publishers, three rounds: atomic valid snapshot/ETag/hash and no abandoned immutable snapshots')

            clear_transfer_cache()
            with (cache / 'transfer.lock').open('a+b') as held:
                fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
                run(['reconcile', 's3://bucket/empty-held', '-r'])
                assert not (cache / 'transfer.json').exists()
                assert list(cache.glob('snapshot-*')) == []
            passed.append('busy publication lock skips only optional cache publication; reconcile completes without waiting or orphan files')

    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(dict(count=len(passed), passed=passed, real_aws_requests=0), indent=2) + '\n')
    print(json.dumps(dict(passed=len(passed), report=str(args.report))))


if __name__ == '__main__':
    main()
