"""Protect the configured S3 index from manual data operations, after ignore."""
import argparse
import hashlib
import json
import pathlib
import tempfile
import urllib.parse

import faults
from index_files import chunks
from remove import Handler as RemoveHandler
from restore import Handler as CaptureHandler, listing

class Handler(RemoveHandler):
    do_PUT = CaptureHandler.handle_request


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=pathlib.Path, required=True)
    parser.add_argument('--report', type=pathlib.Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    phase = dict(db=None, etag='"initial"', cold=False)
    passed = []
    active = 's3://bucket/p/index.db'

    def route(r, state):
        url = urllib.parse.urlsplit(r['path'])
        key = urllib.parse.unquote(url.path).removeprefix('/bucket/')
        if r['method'] == 'GET' and key == 'p/index.db':
            if r['condition'] == phase['etag']:
                return faults.response(304)
            return faults.response(body=phase['db'], headers={'ETag': phase['etag']})
        if r['method'] == 'PUT' and key == 'p/index.db':
            # Internal file save is allowed; a data PUT must never reach here.
            assert r['condition'] == phase['etag'], 'unconditional data overwrite of active index'
            body = chunks(r['body']) if 'aws-chunked' in (r['encoding'] or '') else r['body']
            assert body.startswith(b'SQLite format 3\0'), body[:60]
            phase['db'], phase['etag'] = body, '"' + hashlib.sha256(body).hexdigest() + '"'
            return faults.response(headers={'ETag': phase['etag']})
        if r['method'] == 'HEAD':
            if key == 'p/index.db':
                return faults.head(len(phase['db']), ETag=phase['etag'])
            return faults.head(1, ETag='"data"') if key == 'p/000-first' else faults.response(404)
        if r['method'] == 'GET' and 'list-type=2' in url.query:
            return faults.response(body=listing({'p/index.db': 'standard', 'p/000-first': 'archived' if phase['cold'] else 'standard'}))
        if r['method'] == 'GET' and key == 'p/000-first':
            return faults.response(body=b'x')
        if r['method'] == 'DELETE' and key == 'p/000-first':
            return faults.response(204)
        if r['method'] == 'POST' and key == 'p/000-first' and 'restore' in url.query:
            return faults.response(202)
        raise AssertionError(r)

    with tempfile.TemporaryDirectory(prefix='private-archive-guards-') as directory:
        work = pathlib.Path(directory)
        (work / 'config').write_text('')
        (work / 'source').write_bytes(b'x')
        with faults.fixture(route, Handler) as (state, endpoint):
            # A closed ordinary DB makes index preparation real, not a warning
            # path that would bypass the configured-file lifecycle.
            faults.run(binary, endpoint, work, ['ls', '--json'], 1)
            phase['db'] = (work / 'index.db').read_bytes()
            def run(argv, expected=0):
                state.seen.clear()
                command = faults.command(binary, endpoint, work, argv)
                command[command.index('--index-location') + 1] = active
                import subprocess
                result = subprocess.run(command, cwd=work, env=faults.environment(work), capture_output=True, timeout=58)
                assert result.returncode == expected, (result.returncode, result.stderr.decode())
                return result
            def manual_mutations():
                return [r for r in state.seen if r['method'] in ['DELETE', 'POST'] or (r['method'] == 'PUT' and r['condition'] is None)]

            for argv in [['cp', active, str(work / 'download')],
                         ['cp', str(work / 'source'), active]]:
                result = run(argv, 2)
                assert b'active-index' in result.stderr and result.stdout == b''
                assert manual_mutations() == []
                assert sum(r['method'] == 'GET' and '/p/index.db' in r['path'] for r in state.seen) == 1
                assert not (work / 'download').exists()
            passed.append('active index exact source/destination copy blocked before body transfer or data PUT')

            result = run(['cp', 's3://bucket/p', str(work / 'tree'), '-r'], 2)
            assert b'active-index' in result.stderr and manual_mutations() == [] and not (work / 'tree').exists()
            assert not any(r['method'] == 'GET' and '/p/000-first' in r['path'] for r in state.seen)
            passed.append('recursive cp detects later protected key before first data transfer; internal conditional DB save remains separate')

            phase['cold'] = True
            result = run(['restore', '--restore-duration', '7d', 's3://bucket/p', '-r'], 2)
            assert b'active-index' in result.stderr and manual_mutations() == [] and result.stdout == b''
            result = run(['rm', 's3://bucket/p', '-r', '--auto-approve'], 2)
            assert b'active-index' in result.stderr and manual_mutations() == [] and result.stdout == b''
            passed.append('restore and rm reject later active-index target with zero data-operation requests')

            result = run(['restore', '--restore-duration', '7d', '--ignore', 'index.db', 's3://bucket/p', '-r'])
            assert len(manual_mutations()) == 1 and manual_mutations()[0]['method'] == 'POST'
            result = run(['rm', 's3://bucket/p', '-r', '--ignore', 'index.db', '--auto-approve'])
            assert len(manual_mutations()) == 1 and manual_mutations()[0]['method'] == 'DELETE'
            passed.append('explicit ignore excludes protected key before restore/rm guard and allows other targets')

            phase['cold'] = False
            result = run(['cp', 's3://bucket/p', str(work / 'allowed'), '-r', '--ignore', 'index.db'])
            assert (work / 'allowed/000-first').read_bytes() == b'x'
            assert not (work / 'allowed/index.db').exists() and len(result.stdout.splitlines()) == 1
            passed.append('explicit ignore permits recursive cp of other data while active index is untouched by data operations')

    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(dict(count=len(passed), passed=passed, real_aws_requests=0), indent=2) + '\n')
    print(json.dumps(dict(passed=len(passed), report=str(args.report))))


if __name__ == '__main__':
    main()
