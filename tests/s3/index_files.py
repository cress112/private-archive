"""Real CLI/SDK index transport contracts; local loopback and tiny DBs only."""
import argparse
import contextlib
import hashlib
import http.server
import json
import pathlib
import signal
import sqlite3
import subprocess
import tempfile
import threading
import urllib.parse

import faults
from reconcile import listing


def chunks(raw):
    result = bytearray()
    while raw:
        header, raw = raw.split(b'\r\n', 1)
        n = int(header.split(b';', 1)[0], 16)
        if not n:
            break
        result.extend(raw[:n])
        raw = raw[n + 2:]
    return bytes(result)


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *args):
        pass

    def handle_request(self):
        if self.headers.get('Transfer-Encoding') == 'chunked':
            raw = bytearray()
            while True:
                header = self.rfile.readline()
                n = int(header.strip().split(b';', 1)[0], 16)
                if not n:
                    while self.rfile.readline() != b'\r\n':
                        pass
                    break
                raw.extend(self.rfile.read(n))
                assert self.rfile.read(2) == b'\r\n'
            body = bytes(raw)
        else:
            body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
        if 'aws-chunked' in self.headers.get('Content-Encoding', ''):
            body = chunks(body)
        request = dict(method=self.command, path=self.path,
                       headers={k.lower(): v for k, v in self.headers.items()}, body=body)
        state = self.server.state
        state['seen'].append(request)
        path = urllib.parse.urlsplit(self.path).path
        status, headers, response = 200, {}, b''
        if path == '/index-bucket/index.db':
            if self.command == 'GET':
                if state.get('get_error'):
                    status = state['get_error']
                elif state['db'] is None:
                    status = 404
                elif self.headers.get('If-None-Match') == state['etag']:
                    status = 304
                else:
                    headers['ETag'], response = state['etag'], state['db']
            elif self.command == 'PUT':
                assert body.startswith(b'SQLite format 3\0'), body[:80]
                assert self.headers.get('x-amz-storage-class') == 'STANDARD'
                if state.get('hang_put'):
                    state['part'].set()
                    state['release'].wait(60)
                if state.get('put_error'):
                    status = state['put_error']
                elif (state['db'] is None and self.headers.get('If-None-Match') != '*') or (
                        state['db'] is not None and self.headers.get('If-Match') != state['etag']):
                    status = 412
                else:
                    state['db'] = body
                    state['etag'] = '"' + hashlib.sha256(body).hexdigest() + '"'
                    headers['ETag'] = state['etag']
            else:
                raise AssertionError(request)
        elif self.command == 'GET' and 'list-type=' in self.path:
            if state.get('list_error'):
                status = state['list_error']
            elif state.get('late') and 'continuation-token' in self.path:
                if state.get('hang_later'):
                    state['part'].set()
                    state['release'].wait(60)
                status = 500
            else:
                response = listing(state['keys'], state.get('late', False))
        elif self.command == 'HEAD':
            if path == '/bucket/source':
                headers.update({'Content-Length': '1', 'ETag': '"data"'})
            else:
                status = 404
        elif self.command == 'GET' and path == '/bucket/source':
            response = b'x'
        else:
            raise AssertionError(request)
        if status >= 400:
            response = b'<Error><Code>Injected</Code><Message>injected failure</Message></Error>'
        try:
            self.send_response(status)
            self.send_header('Content-Length', headers.pop('Content-Length', str(len(response))))
            for k, v in headers.items():
                self.send_header(k, v)
            self.send_header('Connection', 'close')
            self.end_headers()
            if self.command != 'HEAD':
                self.wfile.write(response)
        except (BrokenPipeError, ConnectionResetError):
            pass
        self.close_connection = True

    do_GET = do_PUT = do_HEAD = handle_request


@contextlib.contextmanager
def fixture():
    state = dict(db=None, etag=None, keys=['p/a'], seen=[], part=threading.Event(), release=threading.Event())
    server = faults.Server(('127.0.0.1', 0), Handler)
    server.state = state
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield state, f'http://127.0.0.1:{server.server_port}'
    finally:
        state['release'].set()
        server.shutdown()
        server.server_close()
        thread.join()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=pathlib.Path, required=True)
    parser.add_argument('--report', type=pathlib.Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    passed = []
    with tempfile.TemporaryDirectory(prefix='private-archive-index-files-') as directory, fixture() as (state, endpoint):
        work = pathlib.Path(directory)
        (work / 'config').write_text('')
        def command(argv, location='s3://index-bucket/index.db'):
            return [str(binary), '--test-endpoint', endpoint, '--config', str(work / 'config'), '--index-location', str(location), *argv]
        def run(argv, expected=0, location='s3://index-bucket/index.db'):
            state['seen'].clear()
            proc = subprocess.run(command(argv, location), cwd=work, env=faults.environment(work), capture_output=True, timeout=58)
            assert proc.returncode == expected, (proc.returncode, proc.stderr.decode())
            return proc
        def reconcile(expected=0):
            return run(['reconcile', 's3://bucket/p', '-r'], expected)
        def keys(expected=0, location='s3://index-bucket/index.db'):
            rows = json.loads(run(['ls', 's3://bucket', '-r', '--json'], expected, location).stdout)
            assert state['seen'] == [], 'ordinary ls made remote requests'
            return [r['uri'] for r in rows]
        def remote_keys():
            path = work / 'remote.db'
            path.write_bytes(state['db'])
            with sqlite3.connect(path) as db:
                return [bytes(r[0]).decode() for r in db.execute('SELECT object_key FROM objects ORDER BY object_key')]

        assert keys(1) == []
        passed.append('first S3 index ls creates only an empty local copy; requests=0')
        reconcile()
        assert [r['method'] for r in state['seen']] == ['GET', 'GET', 'PUT']
        assert state['seen'][2]['headers']['if-none-match'] == '*'
        assert remote_keys() == ['p/a']
        assert keys() == ['s3://bucket/p/a']
        # Plain external byte copy, no transfer metadata or envelope.
        local = work / 'manual.db'
        local.write_bytes(state['db'])
        assert keys(location=local) == ['s3://bucket/p/a']
        passed.append('missing remote index uses conditional create; ordinary DB file opens by location change')

        working = next((work / 'home/.private-archive/cache/index').glob('*/working.db'))
        with sqlite3.connect(working) as db:
            db.execute('UPDATE objects SET object_key=?', (b'outside/dirty',))
        previous = state['etag']
        reconcile()
        assert state['seen'][0]['headers']['if-none-match'] == previous
        assert state['seen'][-1]['headers']['if-match'] == previous
        assert remote_keys() == ['p/a'], '304 reused mutated working DB'
        passed.append('304 reuses immutable matching snapshot, not mutated working DB; save uses If-Match')

        cache = working.parent / 'transfer.json'
        record = json.loads(cache.read_text())
        (working.parent / record['snapshot']).write_bytes(b'mismatched file')
        reconcile()
        assert 'if-none-match' not in state['seen'][0]['headers']
        passed.append('mismatched snapshot/hash disables conditional GET and performs one normal GET')

        before = state['db']
        state.update(keys=['p/new'], put_error=412)
        result = reconcile(1)
        assert state['db'] == before
        assert sum(r['method'] == 'PUT' for r in state['seen']) == 1
        assert b'index-file-put' in result.stderr
        assert keys() == ['s3://bucket/p/new']
        state.update(put_error=None, list_error=403)
        reconcile(1)
        assert not any(r['method'] == 'PUT' for r in state['seen'])
        assert keys() == ['s3://bucket/p/a'], 'next run treated unsent changes as authoritative'
        state['list_error'] = None
        passed.append('412 stops without overwrite/retry; next fetch replaces previous unsent working changes')

        # A partial successful observation is saved once even on LIST failure.
        state.update(keys=['p/partial'], late=True)
        reconcile(1)
        assert sum(r['method'] == 'PUT' for r in state['seen']) == 1
        assert remote_keys() == ['p/a', 'p/partial']
        state['late'] = False
        passed.append('later LIST failure saves confirmed partial observations once without pruning unseen rows')

        state.update(keys=['p/cancelled'], late=True, hang_later=True)
        state['seen'].clear()
        proc = subprocess.Popen(command(['reconcile', 's3://bucket/p', '-r']), cwd=work,
                                env=faults.environment(work), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            assert state['part'].wait(10), 'second LIST was not issued'
            proc.send_signal(signal.SIGINT)
            out, err = proc.communicate(timeout=52)
            assert proc.returncode == 130, (proc.returncode, err.decode())
            assert out == b''
            assert sum(r['method'] == 'PUT' for r in state['seen']) == 1
            assert remote_keys() == ['p/a', 'p/cancelled', 'p/partial']
        finally:
            state['release'].set()
            if proc.poll() is None:
                proc.kill()
                proc.communicate()
        state.update(late=False, hang_later=False)
        passed.append('Ctrl+C during later LIST exits130 and saves confirmed observations once within stop budget')

        state['get_error'] = 403
        output = work / 'download'
        result = run(['cp', 's3://bucket/source', str(output)])
        assert output.read_bytes() == b'x' and b'[Index]' in result.stderr
        assert not any(r['method'] == 'PUT' for r in state['seen'])
        state['get_error'] = None
        passed.append('index fetch failure warns while data copy succeeds; unknown baseline cannot overwrite index')

        before = state['db']
        state['put_error'] = 500
        result = run(['cp', 's3://bucket/source', str(output)])
        assert output.read_bytes() == b'x' and b'[Index]' in result.stderr
        assert state['db'] == before
        assert sum(r['method'] == 'PUT' for r in state['seen']) == 1
        state['put_error'] = None
        passed.append('index save 500 warns with copy success and one PUT, without retry or rollback')

        state.update(db=local.read_bytes(), etag='"external-copy"', keys=['p/a'])
        cache.unlink()
        reconcile()
        assert 'if-none-match' not in state['seen'][0]['headers']
        assert remote_keys() == ['p/a']
        passed.append('ordinary local DB copied externally to S3 works without cache/ETag sidecar')

        state['db'], state['etag'] = b'broken existing index', '"corrupt"'
        run(['cp', 's3://bucket/source', str(output)])
        assert state['db'] == b'broken existing index'
        assert not any(r['method'] == 'PUT' for r in state['seen'])
        reconcile(1)
        assert len(state['seen']) == 1, 'corrupt index was ignored/rebuilt'
        passed.append('corrupt fetched DB is preserved, never initialized or overwritten, and blocks required reconcile')

        # An absent remote file must not reuse stale local objects.
        state.update(db=None, etag=None, keys=[])
        reconcile()
        assert not any(r['method'] == 'PUT' for r in state['seen'])
        assert keys(1) == []
        passed.append('confirmed missing remote index imports empty copy; no data changes means no save')
        if hasattr(pathlib.Path, 'stat'):
            assert working.parent.stat().st_mode & 0o777 == 0o700
            assert working.stat().st_mode & 0o077 == 0
        passed.append('cache directory and working DB are private to owner')

    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(dict(count=len(passed), passed=passed, real_aws_requests=0), indent=2) + '\n')
    print(json.dumps(dict(passed=len(passed), report=str(args.report))))


if __name__ == '__main__':
    main()
