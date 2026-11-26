"""Native P3-03 startup acceptance. Dummy credentials and loopback trap only.

The fixture binary opens a real Winit/software window, snapshots its rendered
content after startup, then sends the normal close request through Slint.
"""
import argparse
import contextlib
import hashlib
import json
import os
import pathlib
import shutil
import socketserver
import sqlite3
import struct
import subprocess
import tempfile
import threading
import zlib


def save_png(ppm, output):
    header, dimensions, maximum, pixels = ppm.read_bytes().split(b"\n", 3)
    assert header == b"P6" and maximum == b"255"
    width, height = map(int, dimensions.split())
    assert len(pixels) == width * height * 3

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    scanlines = b"".join(b"\0" + pixels[y * width * 3:(y + 1) * width * 3] for y in range(height))
    output.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(scanlines)) + chunk(b"IEND", b""))


class Trap(socketserver.BaseRequestHandler):
    calls = 0

    def handle(self):
        type(self).calls += 1
        self.request.sendall(b"HTTP/1.1 500 Unexpected network access\r\nContent-Length: 0\r\n\r\n")


def seed(path):
    # Use the DB created by the application; insert tiny ordinary observations.
    with contextlib.closing(sqlite3.connect(path)) as db, db:
        for bucket, key in [("archive-alpha", ".DS_Store"), ("archive-zeta", "folder/file.txt")]:
            db.execute("INSERT INTO objects(namespace,scheme,authority,object_key,size_bytes,queried_seconds,queried_nanos,revision) VALUES('aws','s3',?,?,17,123,0,1)", (bucket, key.encode()))
        db.execute("UPDATE metadata SET revision=1")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=pathlib.Path, required=True)
    parser.add_argument("--cli", type=pathlib.Path, required=True)
    parser.add_argument("--report", type=pathlib.Path, required=True)
    parser.add_argument("--screenshots", type=pathlib.Path, required=True)
    args = parser.parse_args()
    binary, cli = args.binary.resolve(), args.cli.resolve()
    args.screenshots.mkdir(parents=True, exist_ok=True)
    cases = []
    with tempfile.TemporaryDirectory(prefix="private-archive-gui-") as directory, socketserver.ThreadingTCPServer(("127.0.0.1", 0), Trap) as server:
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        work = pathlib.Path(directory)
        home = work / "home"
        home.mkdir()
        marker = work / "credential-process-was-invoked"
        credential = work / "credential.sh"
        credential.write_text("#!/bin/sh\ntouch '" + str(marker) + "'\nexit 1\n")
        credential.chmod(0o700)
        aws_config = work / "aws-config"
        aws_config.write_text(f"[profile gui-trap]\nregion = us-east-1\ncredential_process = {credential}\n")
        endpoint = f"http://127.0.0.1:{server.server_address[1]}"
        env = {k: v for k, v in os.environ.items() if not k.startswith("AWS_")}
        env.update(HOME=str(home), AWS_PROFILE="gui-trap", AWS_REGION="us-east-1",
                   AWS_CONFIG_FILE=str(aws_config), AWS_SHARED_CREDENTIALS_FILE=str(work / "no-credentials"),
                   AWS_ENDPOINT_URL=endpoint, AWS_ENDPOINT_URL_S3=endpoint,
                   AWS_EC2_METADATA_SERVICE_ENDPOINT=endpoint,
                   HTTP_PROXY=endpoint, HTTPS_PROXY=endpoint, ALL_PROXY=endpoint,
                   NO_PROXY="127.0.0.1,localhost")
        config, local = work / "config.toml", work / "index.db"

        def run(name, index, options=(), code=0, expected=(), error=False):
            report = work / (name + ".json")
            completed = subprocess.run([str(binary), "--config", str(config), "--index-location", str(index),
                                        "--test-report", str(report), *options], env=env, capture_output=True, text=True, timeout=60)
            assert completed.returncode == code, (name, completed.returncode, completed.stderr)
            assert completed.stdout == "", (name, completed.stdout)
            data = json.loads(report.read_text())
            assert bool(data["error"]) == error, (name, data)
            assert [r["name"] for r in data["rows"]] == list(expected), (name, data)
            assert data["window_visible"] and data["width"] >= 660 and data["height"] >= 400, data
            image = report.with_suffix(".ppm")
            assert image.stat().st_size > 660 * 400 * 3
            save_png(image, args.screenshots / (name + ".png"))
            cases.append({"name": name, "exit": code, "window": [data["width"], data["height"]],
                          "rows": len(data["rows"]), "status": data["status"], "passed": True})
            assert Trap.calls == 0 and not marker.exists(), (name, Trap.calls, marker.exists())
            return data

        try:
            run("first-local", local)
            assert not config.exists() and local.exists()
            seed(local)
            wal = pathlib.Path(str(local) + "-wal")
            assert not wal.exists() or wal.stat().st_size == 0, "seed must be a checkpointed, portable DB"
            expected = ("archive-alpha", "archive-zeta")
            run("shared-local", local, expected=expected)
            observed = subprocess.run([str(cli), "--config", str(config), "--index-location", str(local), "ls", "--json"], env=env, capture_output=True, text=True, timeout=20)
            assert observed.returncode == 0, observed.stderr
            assert [entry["uri"] for entry in json.loads(observed.stdout)] == ["s3://" + name for name in expected]
            # Exercise the installed CLI configuration lifecycle, then consume
            # exactly that file from the GUI; do not maintain a second format.
            def configure(*options, expected_stdout=None):
                result = subprocess.run([str(cli), "--config", str(config), *options], env=env, capture_output=True, text=True, timeout=20)
                assert result.returncode == 0, (options, result.stderr)
                if expected_stdout is not None:
                    assert result.stdout == expected_stdout, (options, result.stdout)

            configure("cfg", "init")
            for key, value in [("index.location", str(local)), ("copy.data-class", "archived"),
                               ("restore.tier", "Standard"), ("ignore", "*"), ("logging.level", "error")]:
                configure("cfg", "set", key, value)
            configure("cfg", "get", "logging.level", expected_stdout='"error"\n')
            configure("--log-level", "trace", "cfg", "get", "logging.level", expected_stdout='"trace"\n')
            before = config.read_bytes()
            run("all-ignored-empty", local)
            run("runtime-clears-ignore", local,
                ["--ignore", "", "--data-class", "archived", "--restore-tier", "Standard", "--log-level", "trace"], expected=expected)
            assert config.read_bytes() == before
            config.unlink()
            uri = "s3://index-bucket/archive.db"
            digest = hashlib.sha256(b"aws\0" + uri.encode()).hexdigest()
            cached = home / ".private-archive/cache/index" / digest / "working.db"
            first = run("first-s3-local-copy", uri)
            assert first["index"] == uri and cached.exists()
            shutil.copyfile(local, cached)
            run("cached-s3-local-copy", uri, expected=expected)
            corrupt = work / "corrupt.db"
            original = b"broken index; preserve these bytes"
            corrupt.write_bytes(original)
            run("corrupt-local", corrupt, code=1, error=True)
            assert corrupt.read_bytes() == original
            cached.write_bytes(original)
            run("corrupt-s3-local-copy", uri, code=1, error=True)
            assert cached.read_bytes() == original
            config.write_text('[logging]\nlevel = "invalid"\n')
            run("invalid-config-even-with-override", work / "never-created.db", ["--log-level", "info"], code=2, error=True)
            assert not (work / "never-created.db").exists()
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps({"platform": os.uname().sysname + " " + os.uname().release,
                "binary": str(binary.relative_to(pathlib.Path.cwd())), "cases": cases,
                "network_requests": Trap.calls, "credential_process_calls": int(marker.exists()),
                "cli_configuration_commands": 8,
                "real_aws_requests": 0}, indent=2, ensure_ascii=False) + "\n")
            print(json.dumps({"passed": len(cases), "network_requests": Trap.calls, "credential_process_calls": 0}))
        finally:
            server.shutdown()
            thread.join()


if __name__ == "__main__":
    main()
