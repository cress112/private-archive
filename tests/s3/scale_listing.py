"""Measure the real CLI process; seed only test-owned DBs, no network/large bodies."""
import argparse
import json
import pathlib
import platform
import re
import sqlite3
import subprocess
import tempfile
import time

import faults


def json_rows(stream):
    decoder = json.JSONDecoder()
    buffer, opened, finished = '', False, False
    while True:
        block = stream.read(65536)
        if block:
            buffer += block.decode('utf-8')
        while True:
            buffer = buffer.lstrip()
            if not opened:
                if not buffer:
                    break
                assert buffer[0] == '['
                opened, buffer = True, buffer[1:]
                continue
            if buffer.startswith(','):
                buffer = buffer[1:]
                continue
            if buffer.startswith(']'):
                finished, buffer = True, buffer[1:]
                break
            if not buffer:
                break
            try:
                row, end = decoder.raw_decode(buffer)
            except json.JSONDecodeError:
                break
            yield row
            buffer = buffer[end:]
        if not block:
            assert opened and finished and not buffer.strip(), buffer[:100]
            return
        assert len(buffer) < 65536, 'unexpectedly large single record'


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=pathlib.Path, required=True)
    parser.add_argument('--report', type=pathlib.Path, required=True)
    parser.add_argument('--counts', default='10000,100000,1000000')
    args = parser.parse_args()
    binary = args.binary.resolve()
    results = []
    assert platform.system() == 'Darwin', 'this runner uses macOS time -l RSS bytes'
    with tempfile.TemporaryDirectory(prefix='private-archive-scale-list-') as directory:
        work = pathlib.Path(directory)
        config, index = work / 'config', work / 'index.db'
        config.write_text('')
        base = [str(binary), '--config', str(config), '--index-location', str(index)]
        env = faults.environment(work)
        init = subprocess.run([*base, 'ls', '--json'], env=env, capture_output=True, timeout=20)
        assert init.returncode == 1 and init.stdout == b'[]\n'
        for count in map(int, args.counts.split(',')):
            with sqlite3.connect(index) as db:
                db.execute('DELETE FROM objects')
                # Source generation is deliberately outside the measured CLI.
                db.executemany('INSERT INTO objects(namespace,scheme,authority,object_key,size_bytes,restore_state,queried_seconds,queried_nanos,revision) VALUES(?,?,?,?,?,?,?,?,?)',
                    (('aws', 's3', 'bucket', f'p/{n // 1000:05}/name-{n % 1000:04}'.encode(),
                      None if n % 29 == 0 else (n * 7919) % 10007, 'archived', 123, 456, 1) for n in range(count)))
                db.execute('UPDATE metadata SET revision=1')
                db.commit()
                db.execute('PRAGMA wal_checkpoint(TRUNCATE)')
            for mode, tail in [('uri-text', []), ('size-name-json', ['--sort', 'size-asc,name-desc', '--json'])]:
                stats = work / 'time.txt'
                command = ['/usr/bin/time', '-l', '-o', str(stats), *base, 'ls', 's3://bucket/p', '-r', *tail]
                start = time.monotonic()
                with tempfile.TemporaryFile() as errors:
                    proc = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=errors)
                    seen, previous = 0, None
                    rows = json_rows(proc.stdout) if mode.endswith('json') else (line.decode().strip() for line in proc.stdout)
                    for row in rows:
                        if isinstance(row, dict):
                            assert len(row) == 10 and row['kind'] == 'object', row
                            size, uri = row['size_bytes'], row['uri']
                            name = uri.rsplit('/', 1)[1]
                            key = (size is None, size or 0, tuple(-ord(c) for c in name), uri)
                        else:
                            key = row
                        assert previous is None or previous <= key, (previous, key)
                        previous, seen = key, seen + 1
                    code = proc.wait(timeout=20)
                    errors.seek(0)
                    assert code == 0, errors.read().decode()
                assert seen == count, (seen, count)
                match = re.search(r'(\d+)\s+maximum resident set size', stats.read_text())
                assert match, stats.read_text()
                rss = int(match[1])
                assert rss <= 512 * 1024 * 1024, rss
                result = dict(count=count, mode=mode, rss_bytes=rss, elapsed_seconds=round(time.monotonic() - start, 3), rows=seen)
                results.append(result)
                print(json.dumps(result), flush=True)
    for mode in ['uri-text', 'size-name-json']:
        by_count = {r['count']: r['rss_bytes'] for r in results if r['mode'] == mode}
        if 100000 in by_count and 1000000 in by_count:
            assert by_count[1000000] - by_count[100000] <= 64 * 1024 * 1024, by_count
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(dict(platform=platform.platform(), rss_unit='bytes (macOS time -l)', results=results, real_aws_requests=0), indent=2) + '\n')


if __name__ == '__main__':
    main()
