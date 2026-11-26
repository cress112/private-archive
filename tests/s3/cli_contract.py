"""Common log/exit/pipe cancellation/fd contracts, using loopback only."""
import argparse
import fcntl
import json
import os
import pathlib
import signal
import sqlite3
import subprocess
import tempfile
import time
import urllib.parse

import faults
from guards import Handler
from index_files import chunks
from reconcile import listing


def full_pipe():
    reader, writer = os.pipe()
    original = fcntl.fcntl(writer, fcntl.F_GETFL)
    fcntl.fcntl(writer, fcntl.F_SETFL, original | os.O_NONBLOCK)
    total = 0
    for block in [b'x' * 4096, b'x']:
        while True:
            try:
                total += os.write(writer, block)
            except BlockingIOError:
                break
    fcntl.fcntl(writer, fcntl.F_SETFL, original)
    # Compare against the descriptor state after this harness has filled it.
    original = fcntl.fcntl(writer, fcntl.F_GETFL)
    return reader, writer, original, total


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=pathlib.Path, required=True)
    parser.add_argument('--report', type=pathlib.Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    phase = dict(objects={}, rows=[], cold=False, event=None, hang=None)
    passed = []

    def route(r, state):
        url = urllib.parse.urlsplit(r['path'])
        key = urllib.parse.unquote(url.path).removeprefix('/bucket/')
        if r['method'] == phase['event']:
            state.part.set()
        if r['method'] == 'GET' and 'list-type=2' in url.query:
            prefix = urllib.parse.parse_qs(url.query).get('prefix', [''])[0]
            return faults.response(body=listing([k for k in phase['rows'] if k.startswith(prefix)]))
        if r['method'] == 'HEAD':
            if key not in phase['objects']:
                return faults.response(404)
            return faults.head(len(phase['objects'][key]), ETag='"data"',
                **{'x-amz-storage-class': 'DEEP_ARCHIVE' if phase['cold'] else 'STANDARD'})
        if r['method'] == 'PUT':
            phase['objects'][key] = chunks(r['body']) if 'aws-chunked' in (r['encoding'] or '') else r['body']
            return faults.response(headers={'ETag': '"data"'}, hang=phase['hang'] == key)
        if r['method'] == 'GET':
            return faults.response(body=phase['objects'][key])
        if r['method'] == 'POST':
            assert 'restore' in url.query
            return faults.response(202)
        if r['method'] == 'DELETE':
            phase['objects'].pop(key, None)
            return faults.response(204)
        raise AssertionError(r)

    with tempfile.TemporaryDirectory(prefix='private-archive-cli-contract-') as directory, faults.fixture(route, Handler) as (state, endpoint):
        work = pathlib.Path(directory)
        (work / 'config').write_text('ignore=["*.tmp"]\n')
        quiet = subprocess.run(faults.command(binary, endpoint, work, ['cfg']), cwd=work,
            env=faults.environment(work), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
        assert quiet.returncode == 0
        passed.append('non-pollable /dev/null stdout/stderr support ordinary command redirection')
        tree = work / 'tree'
        tree.mkdir()
        (tree / '.git').mkdir()
        for path in [tree / 'plain', tree / '.DS_Store', tree / '.git/config', tree / 'skip.tmp']:
            path.write_bytes(b'x')
        outputs = []
        for level in ['error', 'warn', 'info', 'debug', 'trace']:
            state.seen.clear()
            result = faults.run(binary, endpoint, work,
                ['--log-level', level, 'cp', str(tree), 's3://bucket/logs', '-r'], 0)
            outputs.append(sorted(result.stdout.splitlines()))
            assert len(outputs[-1]) == 3
            assert (b'ignore: 1 observed exclusions' in result.stderr) == (level in ['info', 'debug', 'trace'])
            assert (b'skip.tmp' in result.stderr) == (level in ['debug', 'trace'])
            assert (b'trace: cp start' in result.stderr) == (level == 'trace')
            assert (b'trace: cp finished' in result.stderr) == (level == 'trace')
        assert all(rows == outputs[0] for rows in outputs)
        passed.append('all five log levels keep identical result/exit; recursive ignore debug, count info, trace stages')

        for argv in [['--log-level', 'info', 'cp', str(tree / 'skip.tmp'), 's3://bucket/skip'],
                     ['--log-level', 'info', '--ignore', 'tree/', 'cp', str(tree), 's3://bucket/skip', '-r']]:
            state.seen.clear()
            result = faults.run(binary, endpoint, work, argv, 1)
            assert state.seen == [] and result.stdout == b'' and b'ignored ' in result.stderr and b'ignore: 1 observed exclusions' in result.stderr
        state.seen.clear()
        result = faults.run(binary, endpoint, work, ['cp', 's3://bucket/empty', str(work / 'empty'), '-r'], 1)
        assert len(state.seen) == 1 and result.stdout == b'' and not (work / 'empty').exists()
        passed.append('cp direct/all-root ignored and empty remote scope exit1 with no transfer and actual exclusion counts')

        faults.run(binary, endpoint, work, ['cfg', 'set', 'logging.level', 'warn'], 0)
        result = faults.run(binary, endpoint, work, ['cp', str(tree), 's3://bucket/logs', '-r'], 0)
        assert b'ignore:' not in result.stderr and b'skip.tmp' not in result.stderr
        result = faults.run(binary, endpoint, work, ['--log-level', 'debug', 'cp', str(tree), 's3://bucket/logs', '-r'], 0)
        assert b'ignore: 1 observed exclusions' in result.stderr and b'skip.tmp' in result.stderr
        passed.append('saved logging.level applies and runtime override changes only diagnostics')

        for case in ['cfg', 'cfg-write', 'ls', 'help', 'reconcile', 'cp', 'restore', 'rm', 'help-alias']:
            here = work / ('pipe-' + case)
            here.mkdir()
            (here / 'config').write_text('')
            (here / 'source').write_bytes(b'x')
            phase.update(rows=[], cold=case == 'restore', event=None)
            state.part.clear()
            argv = {'cfg': ['cfg'], 'cfg-write': ['cfg', 'set', 'logging.level', 'debug'], 'ls': ['ls', '--json'], 'help': ['--help'],
                'help-alias': ['--help'], 'reconcile': ['reconcile', 's3://bucket/empty', '-r'],
                'cp': ['cp', str(here / 'source'), 's3://bucket/cancel-copy'],
                'restore': ['restore', '--restore-duration', '7d', 's3://bucket/cancel-restore'],
                'rm': ['rm', 's3://bucket/cancel-remove', '--auto-approve']}[case]
            if case in ['restore', 'rm']:
                phase['objects']['cancel-' + ('restore' if case == 'restore' else 'remove')] = b'x'
            phase['event'] = {'cp': 'PUT', 'restore': 'POST', 'rm': 'DELETE', 'reconcile': 'GET'}.get(case)
            reader, writer, original, filled = full_pipe()
            proc = subprocess.Popen(faults.command(binary, endpoint, here, argv), cwd=here,
                env=faults.environment(here), stdin=subprocess.DEVNULL, stdout=writer,
                stderr=writer if case == 'help-alias' else subprocess.PIPE)
            try:
                deadline = time.monotonic() + 10
                while not (fcntl.fcntl(writer, fcntl.F_GETFL) & os.O_NONBLOCK):
                    assert time.monotonic() < deadline and proc.poll() is None, (case, proc.poll())
                    time.sleep(0.01)
                if phase['event']:
                    assert state.part.wait(10), (case, 'no expected request')
                    time.sleep(0.1)
                if case == 'cfg-write':
                    while 'debug' not in (here / 'config').read_text():
                        assert time.monotonic() < deadline and proc.poll() is None
                        time.sleep(0.01)
                start = time.monotonic()
                proc.send_signal(signal.SIGINT)
                _, err = proc.communicate(timeout=52)
                elapsed = time.monotonic() - start
                assert proc.returncode == 130 and elapsed < 50, (case, proc.returncode, err)
                assert fcntl.fcntl(writer, fcntl.F_GETFL) == original, (case, 'output flags leaked')
                os.close(writer)
                writer = None
                received = bytearray()
                while True:
                    block = os.read(reader, 65536)
                    if not block:
                        break
                    received.extend(block)
                assert bytes(received) == b'x' * filled, (case, 'wrote a false/partial success into full output')
                if case in ['cp', 'restore', 'rm']:
                    with sqlite3.connect(here / 'index.db') as db:
                        row = db.execute('SELECT restore_state FROM objects WHERE object_key=?',
                            (('cancel-' + {'cp': 'copy', 'restore': 'restore', 'rm': 'remove'}[case]).encode(),)).fetchone()
                        assert (row is None) == (case == 'rm'), (case, row)
                        if case == 'restore':
                            assert row[0] == 'restoring'
                if case == 'cfg-write':
                    assert 'debug' in (here / 'config').read_text()
                passed.append(f'{case}: blocked stdout Ctrl+C exits130 in {elapsed:.3f}s and restores shared fd flags')
                print(json.dumps({'checked': case, 'elapsed': round(elapsed, 3)}), flush=True)
            finally:
                if proc.poll() is None:
                    proc.kill()
                    proc.communicate()
                if writer is not None:
                    os.close(writer)
                os.close(reader)

        # A second signal forces exit while the issued PUT is still waiting.
        here = work / 'second-signal'
        here.mkdir()
        (here / 'config').write_text('')
        (here / 'source').write_bytes(b'x')
        (here / 'tmp').mkdir()
        phase.update(hang='cancel-strong', event='PUT', cold=False)
        state.part.clear()
        state.release.clear()
        reader, writer, original, _ = full_pipe()
        env = faults.environment(here)
        env['TMPDIR'] = str(here / 'tmp')
        proc = subprocess.Popen(faults.command(binary, endpoint, here, ['cp', str(here / 'source'), 's3://bucket/cancel-strong']),
            cwd=here, env=env, stdin=subprocess.DEVNULL, stdout=writer, stderr=subprocess.PIPE)
        try:
            assert state.part.wait(10)
            proc.send_signal(signal.SIGINT)
            time.sleep(0.1)
            assert proc.poll() is None, 'did not retain the in-flight mutation after first signal'
            start = time.monotonic()
            proc.send_signal(signal.SIGINT)
            proc.communicate(timeout=5)
            assert proc.returncode == 130 and time.monotonic() - start < 5
            assert fcntl.fcntl(writer, fcntl.F_GETFL) == original
        finally:
            state.release.set()
            phase.update(hang=None, event=None)
            if proc.poll() is None:
                proc.kill()
                proc.communicate()
            os.close(reader)
            os.close(writer)
        passed.append('second Ctrl+C forces130 during unresolved mutation and restores shared fd flags')

        # Fatal startup diagnostics must not leave stderr in nonblocking mode.
        for bad_arguments in [False, True]:
            here = work / ('bad-arguments' if bad_arguments else 'bad-config')
            here.mkdir()
            (here / 'config').write_text('' if bad_arguments else '[invalid TOML')
            reader, writer, original, _ = full_pipe()
            try:
                proc = subprocess.Popen(faults.command(binary, endpoint, here, ['--unknown-option'] if bad_arguments else ['cfg']), cwd=here,
                    env=faults.environment(here), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=writer)
                out, _ = proc.communicate(timeout=5)
                assert proc.returncode == 2 and out == b'' and fcntl.fcntl(writer, fcntl.F_GETFL) == original
            finally:
                os.close(reader)
                os.close(writer)
            passed.append(('argument' if bad_arguments else 'configuration') + ' startup error with full stderr remains exit2 and restores descriptor flags')

        # Cancellation after output began can still close a valid partial JSON
        # array when the consumer resumes reading within the stop budget.
        here = work / 'partial-json'
        here.mkdir()
        (here / 'config').write_text('')
        phase.update(rows=[f'json/{n:04}' for n in range(600)], event=None, cold=False)
        faults.run(binary, endpoint, here, ['reconcile', 's3://bucket/json', '-r'], 0)
        before = len(state.seen)
        proc = subprocess.Popen(faults.command(binary, endpoint, here, ['ls', 's3://bucket/json', '-r', '--json']),
            cwd=here, env=faults.environment(here), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            first = b''
            deadline = time.monotonic() + 10
            import select
            while b'}' not in first:
                remaining = deadline - time.monotonic()
                assert remaining > 0 and select.select([proc.stdout], [], [], remaining)[0]
                block = os.read(proc.stdout.fileno(), 4096)
                assert block
                first += block
            assert first.startswith(b'[')
            time.sleep(0.1)
            proc.send_signal(signal.SIGINT)
            rest, err = proc.communicate(timeout=52)
            rows = json.loads(first + rest)
            assert proc.returncode == 130 and 0 < len(rows) < 600, (proc.returncode, len(rows), err)
            assert len(state.seen) == before
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.communicate()
        passed.append('cancelled JSON finishes current record/closes valid partial array when consumer drains; S3 API0')

    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(dict(count=len(passed), passed=passed, real_aws_requests=0), indent=2) + '\n')
    print(json.dumps(dict(passed=len(passed), report=str(args.report))))


if __name__ == '__main__':
    main()
