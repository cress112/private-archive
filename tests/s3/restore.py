"""Restore CLI/SDK preflight, acceptance and interruption on loopback only."""
import argparse
import json
import pathlib
import signal
import socket
import subprocess
import time
import urllib.parse
import xml.etree.ElementTree as ET
from xml.sax.saxutils import escape

import faults


class Handler(faults.Handler):
    def handle_request(self):
        n = int(self.headers.get('Content-Length', 0))
        assert n <= 64 * 1024, 'restore must not transfer object bodies'
        request = dict(method=self.command, path=self.path, body=self.rfile.read(n),
                       restore_status=self.headers.get('x-amz-optional-object-attributes'),
                       condition=self.headers.get('If-Match') or self.headers.get('If-None-Match'),
                       encoding=self.headers.get('Content-Encoding'))
        state = self.server.state
        with state.lock:
            state.seen.append(request)
        status, headers, body, hang = state.route(request, state)
        if hang:
            state.part.set()
            state.release.wait(60)
        if status == 0:
            self.close_connection = True
            self.connection.shutdown(socket.SHUT_RDWR)
            return
        try:
            self.send_response(status)
            self.send_header('Content-Length', str(headers.pop('Content-Length', len(body))))
            self.send_header('x-amz-request-id', 'restore-fixture-request')
            for k, v in headers.items():
                self.send_header(k, v)
            self.send_header('Connection', 'close')
            self.end_headers()
            if self.command != 'HEAD':
                self.wfile.write(body)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        self.close_connection = True

    do_GET = do_HEAD = do_POST = handle_request


def listing(objects):
    rows = []
    for key, state in reversed(list(objects.items())):
        storage = '' if state.startswith('unknown') else '<StorageClass>' + ('STANDARD' if state == 'standard' else 'DEEP_ARCHIVE') + '</StorageClass>'
        restore = '<RestoreStatus><IsRestoreInProgress>true</IsRestoreInProgress></RestoreStatus>' if state == 'restoring' else (
            '<RestoreStatus><IsRestoreInProgress>false</IsRestoreInProgress><RestoreExpiryDate>2026-12-01T00:00:00Z</RestoreExpiryDate></RestoreStatus>' if state == 'restored' else (
                '<RestoreStatus><IsRestoreInProgress>false</IsRestoreInProgress></RestoreStatus>' if state == 'restored-no-expiry' else '<RestoreStatus/>' if state == 'cold-empty-status' else ''))
        rows.append(f'<Contents><Key>{escape(key)}</Key><Size>1</Size><ETag>"v1"</ETag>{storage}{restore}</Contents>')
    return ('<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><IsTruncated>false</IsTruncated>' + ''.join(rows) + '</ListBucketResult>').encode()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=pathlib.Path, required=True)
    parser.add_argument('--report', type=pathlib.Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    phase = dict(objects={}, errors={}, hang=None)
    passed = []

    def route(request, state):
        path = urllib.parse.urlsplit(request['path'])
        key = urllib.parse.unquote(path.path).removeprefix('/bucket/')
        if request['method'] == 'GET':
            assert path.path.rstrip('/') == '/bucket' and 'list-type=2' in path.query, request
            assert request['restore_status'] == 'RestoreStatus'
            prefix = urllib.parse.parse_qs(path.query).get('prefix', [''])[0]
            return faults.response(body=listing({k: v for k, v in phase['objects'].items() if k.startswith(prefix)}))
        if request['method'] == 'HEAD':
            status = phase.get('head_override', {}).get(key, phase['objects'].get(key))
            if status is None:
                return faults.response(404)
            if status == 'unknown-failed':
                return faults.response(403)
            headers = {'ETag': '"v1"', 'x-amz-storage-class': 'STANDARD' if status == 'standard' else (
                'EXTERNAL_UNKNOWN' if status == 'unknown-unresolved' else 'DEEP_ARCHIVE')}
            if status == 'restoring':
                headers['x-amz-restore'] = 'ongoing-request="true"'
            if status == 'restored':
                headers['x-amz-restore'] = 'ongoing-request="false", expiry-date="Tue, 01 Dec 2026 00:00:00 GMT"'
            return faults.head(1, **headers)
        assert request['method'] == 'POST' and 'restore' in path.query, request
        root = ET.fromstring(request['body'])
        ns = {'s3': 'http://s3.amazonaws.com/doc/2006-03-01/'}
        assert root.findtext('s3:Days', namespaces=ns) == '7'
        assert root.findtext('s3:GlacierJobParameters/s3:Tier', namespaces=ns) == phase.get('tier', 'Bulk')
        status = phase['errors'].get(key, 202)
        if status >= 400:
            return faults.error(status)
        return faults.response(status, hang=phase['hang'] == key)

    import tempfile
    with tempfile.TemporaryDirectory(prefix='private-archive-restore-') as directory:
        work = pathlib.Path(directory)
        (work / 'config').write_text('')
        with faults.fixture(route, Handler) as (state, endpoint):
            def run(targets, expected=0, globals=()):
                state.seen.clear()
                return faults.run(binary, endpoint, work,
                                  ['restore', '--restore-duration', '1w', *globals, *targets], expected)
            def requests():
                return [urllib.parse.unquote(urllib.parse.urlsplit(r['path']).path) for r in state.seen if r['method'] == 'POST']
            def cached():
                before = len(state.seen)
                rows = json.loads(faults.run(binary, endpoint, work, ['ls', 's3://bucket', '-r', '--json'], 0).stdout)
                assert len(state.seen) == before
                return {r['uri']: r for r in rows}

            phase['objects'] = {'p/z': 'archived', 'p/a': 'archived', 'q/c': 'archived'}
            result = run(['s3://bucket/p', '-r', 's3://bucket/p/z', 's3://bucket/q/c'])
            assert requests() == ['/bucket/p/a', '/bucket/p/z', '/bucket/q/c']
            assert [r['method'] for r in state.seen[:3]] == ['GET', 'HEAD', 'HEAD']
            assert len(result.stdout.splitlines()) == 3 and b'accepted' in result.stdout and b'days=7 tier=Bulk' in result.stdout
            assert all(cached()[f's3://bucket/{key}']['restore_state'] == 'restoring' for key in phase['objects'])
            passed.append('all URI preflight precedes requests; dedup first URI/key order; acceptance indexed without polling')

            for invalid in ['standard', 'restoring', 'restored']:
                phase['objects'] = {'p/a': 'archived', 'q/b': invalid}
                run(['s3://bucket/p', '-r', 's3://bucket/q/b'], 1)
                assert requests() == [] and [r['method'] for r in state.seen] == ['GET', 'HEAD']
            passed.append('known immediate/restoring/restored object in later URI blocks all requests')

            phase['objects'] = {'p/unknown': 'cold-empty-status'}
            run(['s3://bucket/p', '-r'])
            assert sum(r['method'] == 'HEAD' for r in state.seen) == 1 and requests() == ['/bucket/p/unknown']
            phase['objects'] = {'p/no-expiry': 'restored-no-expiry'}
            run(['s3://bucket/p', '-r'], 1)
            assert requests() == [] and len(state.seen) == 1
            assert cached()['s3://bucket/p/no-expiry']['restore_state'] == 'restored'
            assert cached()['s3://bucket/p/no-expiry']['restore_until'] is None
            passed.append('empty RestoreStatus is unknown and requires HEAD; false without expiry is still known restored')

            phase['objects'] = {'p/a': 'archived'}
            phase['head_override'] = {'p/a': 'restoring'}
            run(['s3://bucket/p', '-r', 's3://bucket/p/a'], 1)
            assert requests() == [] and [r['method'] for r in state.seen] == ['GET', 'HEAD']
            phase['head_override'] = {'p/a': 'unknown-unresolved'}
            run(['s3://bucket/p', '-r', 's3://bucket/p/a'], 1)
            assert requests() == [] and sum(r['method'] == 'HEAD' for r in state.seen) == 2
            phase['head_override'] = {}
            passed.append('overlapping URI dedup keeps first ordering but cannot hide later restoring or unknown observation')

            phase['objects'] = {'p/a': 'archived', 'p/b': 'unknown-archived'}
            run(['s3://bucket/p', '-r'])
            assert [r['path'] for r in state.seen if r['method'] == 'HEAD'] == ['/bucket/p/b']
            assert len(requests()) == 2
            for unknown in ['unknown-unresolved', 'unknown-failed']:
                phase['objects']['p/b'] = unknown
                run(['s3://bucket/p', '-r'], 1)
                assert requests() == [] and sum(r['method'] == 'HEAD' for r in state.seen) == 1
            passed.append('only unknown status receives HEAD; failed or still unknown preflight sends zero restore requests')

            phase['objects'] = {'p/a': 'archived', 'p/sub/deep': 'archived', 'q': 'archived'}
            run(['s3://bucket/p', '-r', '--recursive-depth', '1', 's3://bucket/q'])
            assert requests() == ['/bucket/p/a', '/bucket/q']
            passed.append('depth belongs to each URI and resets for following exact URI')

            phase['objects'] = {'p/.DS_Store': 'archived', 'p/.git/config': 'archived'}
            run(['s3://bucket/p', '-r'])
            assert requests() == ['/bucket/p/.DS_Store', '/bucket/p/.git/config']
            passed.append('default recursive restore includes hidden files and hidden directories')

            phase['objects'] = {'p/a.tmp': 'archived', 'p/b.tmp': 'archived'}
            result = run(['s3://bucket/p', '-r'], 1, ['--ignore', '*.tmp'])
            assert len(state.seen) == 1 and requests() == [] and b'ignore: 2 observed exclusions' in result.stderr
            result = run(['s3://bucket/p/a.tmp'], 1, ['--ignore', '*.tmp'])
            assert state.seen == [] and b'ignore: 1 observed exclusions' in result.stderr
            result = run(['s3://bucket/p/a.tmp'], 1, ['--ignore', '*.tmp', '--log-level', 'error'])
            assert state.seen == [] and b'ignore:' not in result.stderr
            passed.append('all ignored errors; direct ignore makes zero requests; info/debug logging respects level')

            phase['objects'] = {}
            run(['s3://bucket/missing'], 1)
            assert len(state.seen) == 1 and requests() == []
            run(['s3://bucket/empty', '-r'], 1)
            assert len(state.seen) == 1 and requests() == []
            passed.append('exact missing and empty recursive scope are errors without restore requests')

            phase['objects'] = {'p/a': 'archived', 'p/b': 'archived', 'p/c': 'archived'}
            phase['errors'] = {'p/b': 500}
            result = run(['s3://bucket/p', '-r'], 1)
            assert requests() == ['/bucket/p/a', '/bucket/p/b'] and len(result.stdout.splitlines()) == 1
            assert b'outcome=Unknown' in result.stderr and b'restore-fixture-request' in result.stderr
            assert cached()['s3://bucket/p/a']['restore_state'] == 'restoring'
            assert cached()['s3://bucket/p/b']['restore_state'] == 'archived'
            passed.append('first restore 500 stops next request without retry; prior acceptance and index update remain')

            phase['errors'] = {'p/a': 200}
            result = run(['s3://bucket/p', '-r'], 1)
            assert requests() == ['/bucket/p/a'] and result.stdout == b'' and b'HTTP 200' in result.stderr
            phase['errors'] = {}
            passed.append('HTTP200 already-restored race is an error, not a new acceptance or expiry-update success')

            phase['tier'] = 'Standard'
            run(['s3://bucket/p/a'], globals=['--restore-tier', 'Standard'])
            phase['tier'] = 'Bulk'
            (work / 'index.db').write_bytes(b'corrupt existing index')
            result = run(['s3://bucket/p/a'])
            assert requests() == ['/bucket/p/a'] and b'[Index]' in result.stderr and b'accepted' in result.stdout
            assert (work / 'index.db').read_bytes() == b'corrupt existing index'
            (work / 'index.db').unlink()
            passed.append('configured tier override works; corrupt attached index warns while restore acceptance succeeds')

            phase['hang'] = 'p/a'
            state.seen.clear()
            cmd = faults.command(binary, endpoint, work, ['restore', '--restore-duration', '7d', 's3://bucket/p', '-r'])
            proc = subprocess.Popen(cmd, cwd=work, env=faults.environment(work), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                assert state.part.wait(10), 'restore request was not issued'
                start = time.monotonic()
                proc.send_signal(signal.SIGINT)
                time.sleep(0.1)
                state.release.set()
                out, err = proc.communicate(timeout=52)
                assert proc.returncode == 130, (proc.returncode, err.decode())
                assert time.monotonic() - start < 50
                assert requests() == ['/bucket/p/a'] and len(out.splitlines()) == 1 and b'accepted' in out
                assert cached()['s3://bucket/p/a']['restore_state'] == 'restoring'
            finally:
                state.release.set()
                if proc.poll() is None:
                    proc.kill()
                    proc.communicate()
            passed.append('SIGINT after dispatch retains confirmed 202 acceptance, exits130 and sends no next request')

    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(dict(count=len(passed), passed=passed, real_aws_requests=0), indent=2) + '\n')
    print(json.dumps(dict(passed=len(passed), report=str(args.report))))


if __name__ == '__main__':
    main()
