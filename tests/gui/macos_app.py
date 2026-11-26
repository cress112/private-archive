"""LaunchServices launch of production and fixture .apps, isolated HOME/HTTP."""
import argparse
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import time

from workflow import ROOT, faults, seed
from startup import save_png


def call(args, **kw):
    return subprocess.run([str(a) for a in args], check=True, capture_output=True, timeout=40, **kw)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--app", type=pathlib.Path, required=True)
    parser.add_argument("--fixture", type=pathlib.Path, required=True)
    parser.add_argument("--report", type=pathlib.Path, required=True)
    parser.add_argument("--screenshots", type=pathlib.Path, required=True)
    args = parser.parse_args()
    args.screenshots.mkdir(parents=True, exist_ok=True)
    results = []
    with tempfile.TemporaryDirectory(prefix="archive-macos-app-") as directory:
        work = pathlib.Path(directory)
        helper = work / "window-helper"
        call(["xcrun", "clang", "-fobjc-arc", ROOT / "tests/gui/macos_window.m", "-framework", "AppKit", "-framework", "ImageIO", "-o", helper])
        relocated = work / "移動した Private Archive.app"
        shutil.copytree(args.app, relocated)
        call(["codesign", "--verify", "--deep", "--strict", relocated])
        home = work / "home"; home.mkdir()
        marker = work / "credential-called"
        credential = work / "credential.sh"
        credential.write_text("#!/bin/sh\nprintf 'resolved\\n' >> '" + str(marker) + "'\nprintf '%s\\n' '{\"Version\":1,\"AccessKeyId\":\"dummy-resolved-key\",\"SecretAccessKey\":\"dummy-secret\",\"SessionToken\":\"dummy-token\",\"Expiration\":\"2099-01-01T00:00:00Z\"}'\n")
        credential.chmod(0o700)
        aws_config = work / "aws-config"
        aws_config.write_text("[profile private-archive]\nregion=us-east-1\ncredential_process=" + str(credential) + "\n")
        config = work / "shared.toml"; config.write_text("[index]\nlocation='index.db'\n[logging]\nlevel='warn'\n")
        seed(work / "index.db", [("folder/visible-from-shared-index.txt", 31)])

        def trap(request, state):
            return faults.error(500)

        with faults.fixture(trap) as (network, endpoint):
            env = dict(HOME=str(home), AWS_CONFIG_FILE=str(aws_config), AWS_SHARED_CREDENTIALS_FILE=str(work / "no-credentials"), HTTP_PROXY=endpoint, HTTPS_PROXY=endpoint, ALL_PROXY=endpoint, NO_PROXY="")
            def launch(bundle, tail):
                cmd = ["open", "-n", "-W", "-a", str(bundle), "-o", str(work / "app.stdout"), "--stderr", str(work / "app.stderr")]
                for key, value in env.items(): cmd += ["--env", key + "=" + value]
                return subprocess.Popen(cmd + ["--args", "--config", str(config), *tail], stdout=subprocess.PIPE, stderr=subprocess.PIPE)

            process = launch(relocated, [])
            owned_pid = None
            last = {}
            try:
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    apps = json.loads(call([helper, "list", relocated]).stdout)
                    last = {"apps": apps, "open_exit": process.poll()}
                    if apps:
                        owned_pid = apps[0]["pid"]
                        shot = subprocess.run([str(helper), "snapshot", str(owned_pid), str(args.screenshots / "production-app.png")], capture_output=True, timeout=15)
                        last.update(capture_exit=shot.returncode, capture_error=shot.stderr.decode())
                        if shot.returncode == 0:
                            # Allow asynchronous local index materialization, then
                            # capture the actual production window for visual QA.
                            time.sleep(2)
                            window = json.loads(call([helper, "snapshot", owned_pid, args.screenshots / "production-app.png"]).stdout)
                            assert window["width"] >= 800 and window["height"] >= 520
                            results.append({"name": "production-relocated-launchservices", "passed": True, "window": window})
                            break
                    time.sleep(.15)
                else:
                    last["app_error"] = (work / "app.stderr").read_text() if (work / "app.stderr").exists() else ""
                    raise AssertionError(("production .app did not produce a visible window", last))
            finally:
                if owned_pid:
                    subprocess.run([str(helper), "quit", str(owned_pid)], capture_output=True, timeout=10)
                try: process.communicate(timeout=12)
                except subprocess.TimeoutExpired:
                    if owned_pid: os.kill(owned_pid, 15)
                    process.communicate(timeout=10)
            assert not network.seen and marker.read_text().splitlines() == ["resolved"]

            fixture_app = work / "Fixture Private Archive.app"
            call(["python3", ROOT / "scripts/package_macos.py", "--binary", args.fixture.resolve(), "--output", fixture_app])
            report = work / "fixture.json"
            actions = work / "actions.json"
            actions.write_text(json.dumps([{"action":"wait_idle"},{"action":"select_uri","column":0,"uri":"s3://bucket","kind":"root"},{"action":"wait_idle"},{"action":"snapshot","name":"existing-index"},{"action":"settings"},{"action":"snapshot","name":"saved-config"},{"action":"cancel_settings"}]))
            process = launch(fixture_app, ["--test-report", str(report), "--test-actions", str(actions)])
            out, err = process.communicate(timeout=75)
            assert process.returncode == 0, err.decode()
            states = json.loads(report.with_suffix(".states.json").read_text())
            state = next(s for s in states if s["name"] == "existing-index")
            assert state["window_visible"] and len(state["columns"]) == 2
            assert state["columns"][1]["rows"][0]["name"] == "folder"
            saved = next(s for s in states if s["name"] == "saved-config")
            assert state["index"] == str(work / "index.db") and saved["settings"][4]["origin"] == "config-file"
            save_png(report.with_name("fixture-existing-index.ppm"), args.screenshots / "fixture-app.png")
            results.append({"name": "fixture-launchservices-shared-index", "passed": True, "state": state})
            assert not network.seen and marker.read_text().splitlines() == ["resolved", "resolved"]
    args.report.write_text(json.dumps({"cases": results, "passed": len(results), "network_requests": 0, "wrapper_credential_process_calls": 2, "application_credential_process_calls": 0, "real_aws_requests": 0}, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"passed": len(results), "network_requests": 0}))


if __name__ == "__main__": main()
