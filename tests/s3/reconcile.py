"""Reconciliation tests of the real fixture CLI, using loopback HTTP only."""
import argparse
import json
import pathlib
import signal
import subprocess
import tempfile
import urllib.parse
from xml.sax.saxutils import escape

import faults


def listing(keys, truncated=False):
    contents = ''.join(
        f'<Contents><Key>{escape(key)}</Key><Size>1</Size>'
        '<LastModified>2026-11-21T00:00:00Z</LastModified><ETag>"v1"</ETag>'
        '<StorageClass>STANDARD</StorageClass></Contents>' for key in keys
    )
    token = '<NextContinuationToken>later</NextContinuationToken>' if truncated else ''
    return ('<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">'
            f'<IsTruncated>{str(truncated).lower()}</IsTruncated>{token}{contents}'
            '</ListBucketResult>').encode()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=pathlib.Path, required=True)
    parser.add_argument('--report', type=pathlib.Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    phase = {'keys': ['p/a', 'p/.DS_Store', 'p/sub/deep'], 'error': None,
             'late': False, 'hang': False, 'head_exists': False}

    def route(request, state):
        if phase['hang']:
            return faults.response(hang=True)
        if request['method'] == 'HEAD':
            if phase['head_exists'] and urllib.parse.urlsplit(request['path']).path == '/bucket/p/present':
                return faults.head(1, ETag='"head1"', **{'Last-Modified': 'Sat, 21 Nov 2026 00:00:00 GMT'})
            return faults.response(404)
        assert faults.request_kind(request) == 'list', request
        assert urllib.parse.parse_qs(urllib.parse.urlsplit(request['path']).query).get('prefix') == ['p/'], request
        if phase['error']:
            return faults.error(phase['error'])
        if phase['late'] and 'continuation-token' in request['path']:
            return faults.error(500)
        return faults.response(body=listing(phase['keys'], phase['late']))

    passed = []
    with tempfile.TemporaryDirectory(prefix='private-archive-reconcile-') as directory:
        work = pathlib.Path(directory)
        (work / 'config').write_text('')
        with faults.fixture(route) as (state, endpoint):
            def run(command, expected=0):
                return faults.run(binary, endpoint, work, command, expected)

            def reconcile(*extra, expected=0):
                state.seen.clear()
                return run(['reconcile', 's3://bucket/p', '--recursive', *extra], expected)

            def cached(expected=0):
                before = len(state.seen)
                result = run(['ls', 's3://bucket/p', '--recursive', '--json'], expected)
                assert len(state.seen) == before, 'ls issued a remote request'
                return [row['uri'] for row in json.loads(result.stdout)]

            result = reconcile()
            assert b'observed=3 removed=0 ignored=0' in result.stdout
            assert len(state.seen) == 1 and faults.request_kind(state.seen[0]) == 'list'
            assert cached() == ['s3://bucket/p/.DS_Store', 's3://bucket/p/a', 's3://bucket/p/sub/deep']
            passed.append('fresh discovery includes hidden names; following ls makes zero requests')

            phase['keys'] = ['p/a']
            result = reconcile('--recursive-depth', '1')
            assert b'observed=1 removed=1 ignored=0' in result.stdout
            assert cached() == ['s3://bucket/p/a', 's3://bucket/p/sub/deep']
            passed.append('depth-limited reconciliation preserves untraversed deeper records')

            phase['keys'] = ['p/a', 'p/new']
            result = reconcile()
            assert b'observed=2 removed=1 ignored=0' in result.stdout
            assert cached() == ['s3://bucket/p/a', 's3://bucket/p/new']
            passed.append('external additions/changes/removals reconcile within the requested prefix')

            phase['keys'] = []
            result = reconcile()
            assert b'observed=0 removed=2 ignored=0' in result.stdout
            assert cached(expected=1) == []
            passed.append('confirmed empty scope succeeds and removes stale records')

            phase['head_exists'] = True
            state.seen.clear()
            result = run(['reconcile', 's3://bucket/p/present'])
            assert b'observed=1 removed=0 ignored=0' in result.stdout
            assert len(state.seen) == 1 and state.seen[0]['method'] == 'HEAD'
            assert cached() == ['s3://bucket/p/present']
            phase['head_exists'] = False
            passed.append('non-recursive existing object uses one HEAD and no LIST/body request')

            phase['keys'] = ['p/stale']
            reconcile()
            phase['keys'] = ['p/a.tmp']
            result = reconcile('--ignore', '*.tmp', expected=1)
            assert b'ignore: 1 observed exclusions' in result.stderr
            assert cached() == ['s3://bucket/p/stale']
            passed.append('all ignored is an error with info notification and no pruning')

            phase.update(keys=['p/fresh'], late=True)
            reconcile(expected=1)
            assert len(state.seen) == 2, 'failed LIST was retried'
            assert cached() == ['s3://bucket/p/fresh', 's3://bucket/p/stale']
            passed.append('later LIST failure preserves observed and unseen rows without retry')

            phase.update(late=False, error=403)
            reconcile(expected=1)
            assert len(state.seen) == 1
            assert cached() == ['s3://bucket/p/fresh', 's3://bucket/p/stale']
            passed.append('permission failure is not treated as empty')

            phase['error'] = None
            state.seen.clear()
            result = run(['reconcile', 's3://bucket/p/stale'])
            assert b'observed=0 removed=1 ignored=0' in result.stdout
            assert len(state.seen) == 1 and state.seen[0]['method'] == 'HEAD'
            passed.append('exact 404 confirms absence and prunes only the exact cached key')

            state.seen.clear()
            result = run(['reconcile', 's3://bucket/p/fresh', '--ignore', 'fresh'], 1)
            assert state.seen == [] and b'ignore: 1 observed exclusions' in result.stderr
            assert cached() == ['s3://bucket/p/fresh']
            passed.append('explicit ignored target performs zero store requests and remains cached')

            phase['hang'] = True
            state.seen.clear()
            proc = subprocess.Popen(faults.command(binary, endpoint, work,
                ['reconcile', 's3://bucket/p', '--recursive']), cwd=work,
                env=faults.environment(work), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                assert state.part.wait(10), 'CLI did not issue the LIST'
                proc.send_signal(signal.SIGINT)
                out, err = proc.communicate(timeout=55)
                assert proc.returncode == 130, err.decode()
                assert out == b'' and len(state.seen) == 1
            finally:
                state.release.set()
                if proc.poll() is None:
                    proc.kill()
                    proc.communicate()
            phase['hang'] = False
            assert cached() == ['s3://bucket/p/fresh']
            passed.append('Ctrl+C during LIST exits 130 without pruning or new requests')

    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps({'passed': passed, 'count': len(passed),
        'remote': 'loopback only', 'real_aws_requests': 0}, indent=2) + '\n')
    print(json.dumps({'passed': len(passed), 'report': str(args.report)}))


if __name__ == '__main__':
    main()
