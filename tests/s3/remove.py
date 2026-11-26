"""Removal/TTY confirmation contracts through the real fixture CLI and SDK."""
import argparse
import fcntl
import json
import os
import pathlib
import pty
import select
import signal
import subprocess
import tempfile
import time
import urllib.parse

import faults
from reconcile import listing
from restore import Handler as RestoreHandler


class Handler(RestoreHandler):
    do_DELETE = RestoreHandler.handle_request


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=pathlib.Path, required=True)
    parser.add_argument('--report', type=pathlib.Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    phase = dict(keys=[], errors={}, late=False, hang=None, lost=None)
    passed = []

    def route(request, state):
        url = urllib.parse.urlsplit(request['path'])
        key = urllib.parse.unquote(url.path).removeprefix('/bucket/')
        if request['method'] == 'GET':
            assert url.path.rstrip('/') == '/bucket' and 'list-type=2' in url.query, request
            if phase['late'] and 'continuation-token' in url.query:
                return faults.error(500)
            prefix = urllib.parse.parse_qs(url.query).get('prefix', [''])[0]
            return faults.response(body=listing([k for k in reversed(phase['keys']) if k.startswith(prefix)], phase['late']))
        if request['method'] == 'HEAD':
            return faults.head(1) if key in phase['keys'] else faults.response(404)
        assert request['method'] == 'DELETE' and request['body'] == b'', request
        if phase['lost'] == key:
            phase['keys'].remove(key)
            return faults.response(0)
        status = phase['errors'].get(key, 204)
        if status >= 400:
            return faults.error(status)
        if key in phase['keys']:
            phase['keys'].remove(key)
        return faults.response(status, hang=phase['hang'] == key)

    with tempfile.TemporaryDirectory(prefix='private-archive-remove-') as directory:
        work = pathlib.Path(directory)
        (work / 'config').write_text('')
        with faults.fixture(route, Handler) as (state, endpoint):
            def run(argv, expected=0):
                state.seen.clear()
                return faults.run(binary, endpoint, work, argv, expected)
            def deleted():
                return [urllib.parse.unquote(urllib.parse.urlsplit(r['path']).path) for r in state.seen if r['method'] == 'DELETE']
            def cached(expected=0):
                before = len(state.seen)
                rows = json.loads(faults.run(binary, endpoint, work, ['ls', 's3://bucket', '-r', '--json'], expected).stdout)
                assert len(state.seen) == before
                return [r['uri'] for r in rows]
            def fresh_index():
                if (work / 'index.db').exists():
                    (work / 'index.db').unlink()
            def terminal(answer=None, interrupt=False, log_error=False):
                state.seen.clear()
                master, slave = pty.openpty()
                original = fcntl.fcntl(slave, fcntl.F_GETFL)
                argv = ['rm', 's3://bucket/p', '-r']
                if log_error:
                    argv += ['--log-level', 'error']
                proc = subprocess.Popen(faults.command(binary, endpoint, work, argv), cwd=work,
                    env=faults.environment(work), stdin=slave, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                prompt = b''
                try:
                    deadline = time.monotonic() + 10
                    while b'Type yes to continue:' not in prompt:
                        remaining = deadline - time.monotonic()
                        assert remaining > 0 and select.select([proc.stderr], [], [], remaining)[0], prompt
                        chunk = os.read(proc.stderr.fileno(), 4096)
                        assert chunk, (proc.poll(), prompt)
                        prompt += chunk
                    assert deleted() == [], 'deletion before confirmation'
                    assert b'2 objects, 2 bytes' in prompt
                    if interrupt:
                        proc.send_signal(signal.SIGINT)
                    else:
                        os.write(master, answer)
                    out, err = proc.communicate(timeout=12)
                    assert fcntl.fcntl(slave, fcntl.F_GETFL) == original, 'stdin flags leaked to caller'
                    return proc.returncode, out, prompt + err
                finally:
                    if proc.poll() is None:
                        proc.kill()
                        proc.communicate()
                    os.close(master)
                    os.close(slave)

            # None of these may even fetch a configured S3 index.
            command = faults.command(binary, endpoint, work, ['rm', 's3://bucket/p', '-r'])
            command[command.index('--index-location') + 1] = 's3://index-bucket/index.db'
            for mode in ['null', 'pipe', 'file']:
                state.seen.clear()
                if mode == 'pipe':
                    result = subprocess.run(command, input=b'yes\n', cwd=work, env=faults.environment(work), capture_output=True, timeout=10)
                elif mode == 'file':
                    path = work / 'answer'
                    path.write_bytes(b'yes\n')
                    with path.open('rb') as file:
                        result = subprocess.run(command, stdin=file, cwd=work, env=faults.environment(work), capture_output=True, timeout=10)
                        assert file.tell() == 0
                else:
                    result = subprocess.run(command, stdin=subprocess.DEVNULL, cwd=work, env=faults.environment(work), capture_output=True, timeout=10)
                assert result.returncode == 1 and b'rm-input' in result.stderr and result.stdout == b''
                assert state.seen == []
            assert not (work / 'index.db').exists() and not (work / 'home').exists()
            passed.append('non-TTY null/pipe/file rejected without reading or any store/index request')

            phase['keys'] = ['p/a', 'p/b']
            code, out, err = terminal(b'yes\n', log_error=True)
            assert code == 0 and len(out.splitlines()) == 2 and deleted() == ['/bucket/p/a', '/bucket/p/b']
            assert err.count(b'Type yes to continue:') == 1 and phase['keys'] == []
            assert cached(1) == []
            passed.append('one TTY yes confirms URI/count/size even at log-level error; stdout redirection allowed; last root disappears')

            for answer in [b'no\n', b'YES\n', b'yes \n', b'no ' + b'x' * 100 + b'\n', b'\x04']:
                phase['keys'] = ['p/a', 'p/b']
                code, out, err = terminal(answer)
                assert code == 1 and out == b'' and deleted() == [] and len(phase['keys']) == 2
                assert err.count(b'Type yes to continue:') == 1
            passed.append('no/uppercase/trailing-space/EOF reject once, exit1, zero deletes and stdin flags restored')

            phase['keys'] = ['p/a', 'p/b']
            code, out, err = terminal(interrupt=True)
            assert code == 130 and out == b'' and deleted() == []
            passed.append('Ctrl+C while awaiting confirmation exits130, deletes nothing and restores stdin flags')

            # Keep the parent's pipe writer open: --auto-approve must not read.
            phase['keys'] = ['p/a', 'p/b']
            state.seen.clear()
            proc = subprocess.Popen(faults.command(binary, endpoint, work, ['rm', 's3://bucket/p', '-r', '--auto-approve']),
                cwd=work, env=faults.environment(work), stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                assert proc.wait(timeout=10) == 0
                out, err = proc.communicate()
                assert len(out.splitlines()) == 2 and b'Type yes' not in err
            finally:
                if proc.poll() is None:
                    proc.kill()
                    proc.communicate()
            passed.append('auto-approve works with a still-open unread stdin pipe and omits confirmation')

            fresh_index()
            phase['keys'] = ['p/.DS_Store', 'p/.git/config', 'p/visible']
            result = run(['rm', 's3://bucket/p', '-r', '--auto-approve'])
            assert deleted() == ['/bucket/p/.DS_Store', '/bucket/p/.git/config', '/bucket/p/visible']
            assert len(result.stdout.splitlines()) == 3 and cached(1) == []
            passed.append('recursive default includes hidden files/directories; global key order and final root removal')

            phase['keys'] = ['p/a', 'p/keep.tmp', 'p/sub/deep']
            run(['reconcile', 's3://bucket/p', '-r'])
            result = run(['rm', 's3://bucket/p', '-r', '--recursive-depth', '1', '--ignore', '*.tmp', '--auto-approve'])
            assert deleted() == ['/bucket/p/a'] and b'ignore: 1 observed exclusions' in result.stderr
            assert phase['keys'] == ['p/keep.tmp', 'p/sub/deep']
            assert cached() == ['s3://bucket/p/keep.tmp', 's3://bucket/p/sub/deep']
            passed.append('depth and explicit ignore preserve both remote objects and cached excluded/deeper records')

            run(['rm', 's3://bucket/p/keep.tmp', '--ignore', '*.tmp', '--auto-approve'], 1)
            assert state.seen == []
            run(['rm', 's3://bucket/p', '-r', '--ignore', '*', '--auto-approve'], 1)
            assert len(state.seen) == 1 and deleted() == []
            run(['rm', 's3://bucket/missing', '--auto-approve'], 1)
            assert len(state.seen) == 1 and deleted() == []
            run(['rm', 's3://bucket/empty', '-r', '--auto-approve'], 1)
            assert len(state.seen) == 1 and deleted() == []
            passed.append('direct ignore API0, all ignored, exact missing and empty prefix are errors without deletion')

            phase.update(keys=['p/new'], late=True)
            run(['rm', 's3://bucket/p', '-r', '--auto-approve'], 1)
            assert len(state.seen) == 2 and deleted() == []
            assert 's3://bucket/p/new' in cached() and 's3://bucket/p/keep.tmp' in cached()
            phase['late'] = False
            passed.append('late LIST failure sends no deletion or retry and retains observed/unseen cached records')

            phase.update(keys=['p/a', 'p/b', 'p/c'], errors={'p/b': 500})
            result = run(['rm', 's3://bucket/p', '-r', '--auto-approve'], 1)
            assert deleted() == ['/bucket/p/a', '/bucket/p/b'] and len(result.stdout.splitlines()) == 1
            assert b'outcome=Unknown' in result.stderr
            assert 's3://bucket/p/a' not in cached() and 's3://bucket/p/b' in cached()
            phase.update(errors={}, lost='p/b', keys=['p/a', 'p/b', 'p/c'])
            result = run(['rm', 's3://bucket/p', '-r', '--auto-approve'], 1)
            assert deleted() == ['/bucket/p/a', '/bucket/p/b'] and len(result.stdout.splitlines()) == 1
            assert b'outcome=Unknown' in result.stderr and 'p/b' not in phase['keys'] and 's3://bucket/p/b' in cached()
            phase['lost'] = None
            passed.append('500/lost response stop next delete without retry/verification; only confirmed success changes index/output')

            (work / 'index.db').write_bytes(b'corrupt index')
            phase['keys'] = ['p/a']
            result = run(['rm', 's3://bucket/p/a', '--auto-approve'])
            assert len(deleted()) == 1 and b'[Index]' in result.stderr and b'remove ' in result.stdout
            assert (work / 'index.db').read_bytes() == b'corrupt index'
            (work / 'index.db').unlink()
            passed.append('corrupt optional index warns and remains unchanged while data deletion succeeds')

            phase.update(keys=['p/a', 'p/b'], hang='p/a')
            state.seen.clear()
            proc = subprocess.Popen(faults.command(binary, endpoint, work, ['rm', 's3://bucket/p', '-r', '--auto-approve']),
                cwd=work, env=faults.environment(work), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                assert state.part.wait(10), 'delete was not issued'
                start = time.monotonic()
                proc.send_signal(signal.SIGINT)
                time.sleep(0.1)
                state.release.set()
                out, err = proc.communicate(timeout=52)
                assert proc.returncode == 130 and time.monotonic() - start < 50, err.decode()
                assert len(out.splitlines()) == 1 and deleted() == ['/bucket/p/a']
                assert cached() == ['s3://bucket/p/b']
            finally:
                state.release.set()
                if proc.poll() is None:
                    proc.kill()
                    proc.communicate()
            passed.append('Ctrl+C during DELETE retains confirmed result/index removal, exits130 and sends no next delete')

    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(dict(count=len(passed), passed=passed, real_aws_requests=0), indent=2) + '\n')
    print(json.dumps(dict(passed=len(passed), report=str(args.report))))


if __name__ == '__main__':
    main()
