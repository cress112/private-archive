"""Measure each metadata acceptance driver in a fresh process (macOS time -l)."""
import argparse
import json
import os
import pathlib
import platform
import re
import signal
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--driver', type=pathlib.Path, required=True)
    parser.add_argument('--report', type=pathlib.Path, required=True)
    parser.add_argument('--counts', default='10000,100000,1000000')
    parser.add_argument('--timeout', type=int, default=7200)
    parser.add_argument('--resume', action='store_true', help='keep successful measurements already in this report')
    args = parser.parse_args()
    assert platform.system() == 'Darwin'
    driver = args.driver.resolve()
    results = json.loads(args.report.read_text())['results'] if args.resume and args.report.exists() else []
    def stop(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, stop)
    with tempfile.TemporaryDirectory(prefix='private-archive-scale-operations-') as directory:
        stats = pathlib.Path(directory) / 'time.txt'
        for count in map(int, args.counts.split(',')):
            for mode in ['reconcile', 'rm', 'restore']:
                if any(r['count'] == count and r['mode'] == mode for r in results):
                    continue
                env = dict(os.environ, TMPDIR=directory)
                with subprocess.Popen(['/usr/bin/time', '-l', '-o', str(stats), str(driver), mode, str(count)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, start_new_session=True) as proc:
                    try:
                        stdout, stderr = proc.communicate(timeout=args.timeout)
                    except BaseException:
                        os.killpg(proc.pid, signal.SIGTERM)
                        proc.communicate(timeout=20)
                        raise
                    assert proc.returncode == 0, (mode, count, stdout.decode(), stderr.decode())
                row = json.loads(stdout)
                row['rss_bytes'] = int(re.search(r'(\d+)\s+maximum resident set size', stats.read_text())[1])
                assert row['count'] == count and row['body_reads'] == 0
                assert row['rss_bytes'] <= 512 * 1024 * 1024, row
                results.append(row)
                print(json.dumps(row), flush=True)
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(json.dumps(dict(platform=platform.platform(), rss_unit='bytes (macOS time -l)', results=results, complete=False), indent=2) + '\n')
    for mode in ['reconcile', 'rm', 'restore']:
        by_count = {r['count']: r['rss_bytes'] for r in results if r['mode'] == mode}
        if 100000 in by_count and 1000000 in by_count:
            assert by_count[1000000] - by_count[100000] <= 64 * 1024 * 1024, by_count
    args.report.write_text(json.dumps(dict(platform=platform.platform(), rss_unit='bytes (macOS time -l)', results=results, complete=True, real_aws_requests=0), indent=2) + '\n')


if __name__ == '__main__':
    main()
