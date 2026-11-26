"""Exercise the external credential boundary without AWS/network access."""
import json
import os
import pathlib
import shutil
import subprocess
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
REMOVE = "AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN AWS_SECURITY_TOKEN AWS_CREDENTIAL_EXPIRATION AWS_ROLE_ARN AWS_ROLE_SESSION_NAME AWS_WEB_IDENTITY_TOKEN_FILE AWS_CONTAINER_CREDENTIALS_RELATIVE_URI AWS_CONTAINER_CREDENTIALS_FULL_URI AWS_CONTAINER_AUTHORIZATION_TOKEN AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE AWS_PROFILE AWS_DEFAULT_PROFILE AWS_ENDPOINT_URL AWS_ENDPOINT_URL_S3 AWS_ENDPOINT_URL_STS".split()
EXPECTED = {"AWS_ACCESS_KEY_ID": "dummy-resolved-key", "AWS_SECRET_ACCESS_KEY": "dummy-secret/+=", "AWS_SESSION_TOKEN": "dummy-token/+=", "AWS_CREDENTIAL_EXPIRATION": "2099-01-01T00:00:00+00:00", "AWS_REGION": "us-west-2", "AWS_DEFAULT_REGION": "us-west-2"}


def run_suite(include_native=False):
    with tempfile.TemporaryDirectory(prefix="archive-wrapper-") as directory:
        work = pathlib.Path(directory)
        probe = work / "probe with spaces"
        probe.write_text('#!/usr/bin/env python3\nimport json,os,sys\nkeys=' + repr(REMOVE + ["AWS_REGION", "AWS_DEFAULT_REGION"]) + '\nprint(json.dumps({"args":sys.argv[1:],"env":{k:os.environ[k] for k in keys if k in os.environ},"profile":os.environ.get("PRIVATE_ARCHIVE_AWS_PROFILE"),"config":os.environ.get("AWS_CONFIG_FILE")}))\nsys.exit(7)\n')
        probe.chmod(0o700)
        fake = work / "aws"
        fake.write_text('''#!/usr/bin/env python3
import json,os,sys
remove = ''' + repr(REMOVE) + '''
assert not any(k in os.environ for k in remove)
with open(os.environ["ARCHIVE_TEST_CALLS"],"a") as f: f.write(json.dumps(sys.argv[1:])+"\\n")
if sys.argv[1:4] == ["configure","get","region"]:
    print("us-west-2");sys.exit(0)
assert sys.argv[1:3] == ["configure","export-credentials"]
assert sys.argv[-2:] == ["--format","env-no-export"]
if os.environ.get("ARCHIVE_TEST_FAIL"): sys.exit(9)
print(os.environ.get("ARCHIVE_TEST_EXPORT", "AWS_ACCESS_KEY_ID=dummy-resolved-key\\nAWS_SECRET_ACCESS_KEY=dummy-secret/+=\\nAWS_SESSION_TOKEN=dummy-token/+=\\nAWS_CREDENTIAL_EXPIRATION=2099-01-01T00:00:00+00:00"))
''')
        fake.chmod(0o700)
        calls = work / "calls.jsonl"
        base = {k: v for k, v in os.environ.items() if not k.startswith("AWS_") and not k.startswith("PRIVATE_ARCHIVE_")}
        base.update({k: "dummy-parent" for k in REMOVE})
        base.update(PATH=str(work)+os.pathsep+base["PATH"], AWS_CONFIG_FILE="shared-config", AWS_SHARED_CREDENTIALS_FILE="shared-credentials", ARCHIVE_TEST_CALLS=str(calls))
        args = ["--config", "space/設定.toml", "--index-location", "s3://bucket/literal$(noop)", "--ignore", "a,b\\,c"]
        kinds = ["cli", "gui", "native"] if include_native else ["cli"]
        if include_native:
            native = work / "app/Contents/MacOS"
            native.mkdir(parents=True)
            resources = native.parent / "Resources"; resources.mkdir()
            shutil.copy2(ROOT / "scripts/private-archive-session.sh", resources / "private-archive-session.sh")
            shutil.copy2(probe, native / "private-archive-gui")
            subprocess.run(["xcrun", "clang", "-std=c11", "-Wall", "-Wextra", "-Werror", ROOT / "scripts/macos/launcher.c", "-o", native / "launcher"], check=True)
        count = 0
        for kind in kinds:
            wrapper = ROOT / ("scripts/private-archive.sh" if kind == "cli" else "scripts/private-archive-gui.sh")
            function = "private-archive" if kind == "cli" else "private-archive-gui"
            env = dict(base, **{("PRIVATE_ARCHIVE_BIN" if kind == "cli" else "PRIVATE_ARCHIVE_GUI_BIN"):str(probe)})
            commands = [[str(native / "launcher"), *args]] if kind == "native" else [["bash", str(wrapper), *args]]
            for command in commands:
                for profile in [None, "custom-dummy"]:
                    child = env.copy()
                    if profile: child["PRIVATE_ARCHIVE_AWS_PROFILE"] = profile
                    else: child.pop("PRIVATE_ARCHIVE_AWS_PROFILE", None)
                    before = len(calls.read_text().splitlines()) if calls.exists() else 0
                    result = subprocess.run(command, env=child, cwd="/", capture_output=True, text=True)
                    assert result.returncode == 7, result.stderr
                    data = json.loads(result.stdout)
                    assert data == {"args":args,"env":EXPECTED,"profile":None,"config":"shared-config"}, data
                    recorded = [json.loads(x) for x in calls.read_text().splitlines()[before:]]
                    assert recorded == [["configure","get","region","--profile",profile or "private-archive"], ["configure","export-credentials","--profile",profile or "private-archive","--format","env-no-export"]], recorded
                    count += 1
                for exported in ["", "AWS_ACCESS_KEY_ID=only-key", "AWS_ACCESS_KEY_ID=a\nAWS_ACCESS_KEY_ID=b\nAWS_SECRET_ACCESS_KEY=c", "AWS_PROFILE=unexpected"]:
                    child = dict(env, ARCHIVE_TEST_EXPORT=exported)
                    result = subprocess.run(command, env=child, cwd="/", capture_output=True, text=True)
                    assert result.returncode == 1 and result.stdout == "", result
                failed = subprocess.run(command, env=dict(env, ARCHIVE_TEST_FAIL="1"), capture_output=True, text=True)
                assert failed.returncode == 1 and failed.stdout == ""
                # Values are data, even when they contain shell code.
                marker = work / "must-not-execute"
                child = dict(env, ARCHIVE_TEST_EXPORT="AWS_ACCESS_KEY_ID=dummy-resolved-key\nAWS_SECRET_ACCESS_KEY=$(touch " + str(marker) + ")")
                result = subprocess.run(command, env=child, cwd="/", capture_output=True, text=True)
                assert result.returncode == 7 and not marker.exists()
                actual = json.loads(result.stdout)["env"]
                assert actual["AWS_SECRET_ACCESS_KEY"].startswith("$(touch ") and "AWS_SESSION_TOKEN" not in actual
            if kind != "native":
                readonly = subprocess.run(["bash", "-c", 'readonly AWS_ACCESS_KEY_ID AWS_PROFILE; /bin/bash "$1" --help >/dev/null; result=$?; test "$AWS_ACCESS_KEY_ID" = dummy-parent && test "$AWS_PROFILE" = dummy-parent && exit "$result"', "probe", str(wrapper)], env=env, capture_output=True, text=True)
                assert readonly.returncode == 7, readonly.stderr
                # Default binaries are relative to the project, independent of CWD/PATH.
                project = work / ("relocated " + kind)
                scripts = project / "scripts"; scripts.mkdir(parents=True)
                binaries = project / "target/bin"; binaries.mkdir(parents=True)
                shutil.copy2(wrapper, scripts / wrapper.name)
                shutil.copy2(ROOT / "scripts/private-archive-session.sh", scripts / "private-archive-session.sh")
                shutil.copy2(probe, binaries / function)
                child = env.copy(); child.pop("PRIVATE_ARCHIVE_BIN", None); child.pop("PRIVATE_ARCHIVE_GUI_BIN", None)
                path = subprocess.run(["bash", str(scripts / wrapper.name), "--help"], cwd="/", env=child, capture_output=True, text=True)
                assert path.returncode == 7 and json.loads(path.stdout)["args"] == ["--help"]
                # Older source-based invocation must not replace the login shell.
                sourced = subprocess.run(["bash", "-c", 'source "$1" >/dev/null 2>&1; result=$?; test "$result" = 2 && printf still-here', "probe", str(wrapper)], env=env, capture_output=True, text=True)
                assert sourced.returncode == 0 and sourced.stdout == "still-here"
    return {"profile_argv_cases":count,"credential_handoff":True,"no_profile_in_application":True,"fail_closed":True,"no_eval":True,"readonly_parent":True,"project_relative_binary":True,"real_aws_requests":0}


if __name__ == "__main__":
    print(json.dumps(run_suite()))
