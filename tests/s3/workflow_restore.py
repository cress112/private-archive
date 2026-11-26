"""One explicit archive/restore/status/download/delete flow, loopback fake store."""
import argparse
import json
import pathlib
import tempfile
import urllib.parse

import faults
from guards import Handler
from restore import listing


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=pathlib.Path, required=True)
    parser.add_argument('--report', type=pathlib.Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    phase = dict(state=None)
    key, uri = 'p/cold', 's3://bucket/p/cold'

    def route(r, state):
        path = urllib.parse.urlsplit(r['path'])
        if r['method'] == 'GET' and 'list-type=2' in path.query:
            return faults.response(body=listing({key: phase['state']} if phase['state'] else {}))
        assert path.path == '/bucket/' + key, r
        if r['method'] == 'HEAD':
            if not phase['state']:
                return faults.response(404)
            headers = {'ETag': '"data"', 'x-amz-storage-class': 'DEEP_ARCHIVE'}
            if phase['state'] == 'restoring':
                headers['x-amz-restore'] = 'ongoing-request="true"'
            if phase['state'] == 'restored':
                headers['x-amz-restore'] = 'ongoing-request="false", expiry-date="Tue, 01 Dec 2026 00:00:00 GMT"'
            return faults.head(1, **headers)
        if r['method'] == 'PUT':
            phase['state'] = 'archived'
            return faults.response(headers={'ETag': '"data"'})
        if r['method'] == 'POST' and 'restore' in path.query:
            assert phase['state'] == 'archived'
            phase['state'] = 'restoring'
            return faults.response(202)
        if r['method'] == 'GET':
            assert phase['state'] == 'restored'
            return faults.response(body=b'x')
        if r['method'] == 'DELETE':
            phase['state'] = None
            return faults.response(204)
        raise AssertionError(r)

    with tempfile.TemporaryDirectory(prefix='private-archive-restore-workflow-') as directory:
        work = pathlib.Path(directory)
        (work / 'source').write_bytes(b'x')
        with faults.fixture(route, Handler) as (state, endpoint):
            def run(argv, expected=0):
                return faults.run(binary, endpoint, work, argv, expected)

            def status(expected):
                before = len(state.seen)
                row = json.loads(run(['ls', uri, '--json']).stdout)[0]
                assert row['restore_state'] == expected
                assert len(state.seen) == before, 'status browse must not poll'

            run(['cfg', 'init'])
            run(['cp', str(work / 'source'), uri])
            status('archived')
            run(['restore', '--restore-duration', '1d', uri])
            status('restoring')
            # Only the external test actor changes completion state. Product
            # behavior has no wait/poll/recovery scheduler between commands.
            phase['state'] = 'restored'
            status('restoring')
            run(['reconcile', uri])
            status('restored')
            run(['cp', uri, str(work / 'download')])
            assert (work / 'download').read_bytes() == b'x'
            run(['rm', uri, '--auto-approve'])
            assert json.loads(run(['ls', '--json'], 1).stdout) == []
            assert sum(r['method'] == 'POST' for r in state.seen) == 1
            assert sum(r['method'] == 'DELETE' for r in state.seen) == 1
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(dict(passed=True, operations=['cfg init', 'archive cp', 'local status', 'restore acceptance', 'external completion', 'stale local status', 'explicit reconcile', 'download', 'rm', 'empty roots'],
        polling_requests=0, real_aws_requests=0), indent=2) + '\n')
    print(json.dumps(dict(passed=True, report=str(args.report))))


if __name__ == '__main__':
    main()
