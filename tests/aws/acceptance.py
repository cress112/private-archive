"""Explicit, bounded real-S3 acceptance. No infrastructure APIs or provisioning.

Run via scripts/private-archive-session.sh so credentials stay outside argv/files.
The ledger reserves conservative request/transfer bounds BEFORE each operation.
No automatic retries, repeated restore requests, or background polling.
"""
import argparse
import fcntl
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
BUCKET = "cress-archived-data"
ACCOUNT = "975049957858"
REGION = "us-east-1"
MAX_REQUESTS = 400
CLEANUP_RESERVE = 32
MAX_TRANSFER = 20 * 1024 * 1024
MAX_FILE = 4 * 1024 * 1024
SAMPLE_SIZE = 64 * 1024
REPORT = ROOT / "docs/validation/aws-live.json"


def now():
    return dt.datetime.now(dt.timezone.utc).isoformat()


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


class Run:
    def __init__(self, start=False):
        if start:
            if REPORT.exists():
                raise RuntimeError("existing acceptance ledger; inspect/resume it, never start twice")
            prefix = "acceptance-" + uuid.uuid4().hex + "/"
            work = ROOT / "target" / prefix[:-1]
            work.mkdir()
            self.s = dict(started=now(), phase="prepared", prefix=prefix, work=str(work),
                          bucket=BUCKET, account=ACCOUNT, requests_reserved=1,
                          transfer_reserved=4 * 1024 * 1024, overhead_allowance_bytes=4 * 1024 * 1024, max_requests=MAX_REQUESTS,
                          max_transfer=MAX_TRANSFER, owned_keys=[], operations=[],
                          checks={}, errors=[], restore_requested=False,
                          billed_cost_usd=None, cost_is_estimate=True,
                          planning_allowance_usd="0.20", authorized_limit_usd="1.00",
                          infrastructure_apis_used=False)
            self.save()
        else:
            self.s = json.loads(REPORT.read_text())
        self.cleanup_attempted = False
        self.work = Path(self.s["work"])
        self.config = self.work / "config.toml"
        self.index = self.work / "index.db"
        self.sample = self.work / "sample.bin"
        self.env = dict(os.environ, AWS_MAX_ATTEMPTS="1", AWS_RETRY_MODE="standard",
                        AWS_REGION=REGION, AWS_DEFAULT_REGION=REGION,
                        AWS_EC2_METADATA_DISABLED="true", AWS_PAGER="",
                        HOME=str(self.work / "home"))
        Path(self.env["HOME"]).mkdir(exist_ok=True)
        # Resolved env credentials are inherited; never read/write user AWS files.
        self.env.pop("AWS_PROFILE", None)
        self.env.pop("AWS_DEFAULT_PROFILE", None)

    def save(self):
        REPORT.parent.mkdir(parents=True, exist_ok=True)
        tmp = REPORT.with_suffix(".tmp")
        tmp.write_text(json.dumps(self.s, ensure_ascii=False, indent=2) + "\n")
        tmp.replace(REPORT)

    def uri(self, suffix):
        return "s3://" + BUCKET + "/" + self.s["prefix"] + suffix

    def key(self, suffix):
        return self.s["prefix"] + suffix

    def reserve(self, label, requests=0, transfer=0, cleanup=False):
        limit = MAX_REQUESTS if cleanup else MAX_REQUESTS - CLEANUP_RESERVE
        if self.s["requests_reserved"] + requests > limit:
            raise RuntimeError("request allowance exhausted; only cleanup is permitted")
        if self.s["transfer_reserved"] + transfer > MAX_TRANSFER:
            raise RuntimeError("transfer allowance exhausted; stop and clean up")
        self.s["requests_reserved"] += requests
        self.s["transfer_reserved"] += transfer
        # Every request charged at $0.0001; all transfer at $1/GB, plus $0.01
        # for <=20 MiB storage/180-day minimum/metadata/restore. Free tiers ignored.
        bound = self.s["requests_reserved"] * .0001 + self.s["transfer_reserved"] / 1e9 + .01
        if bound >= .20:
            raise RuntimeError("conservative planning allowance exhausted")
        self.s["conservative_reserved_cost_usd"] = round(bound, 8)
        op = dict(label=label, started=now(), request_reservation=requests,
                  transfer_reservation=transfer, result="pending")
        self.s["operations"].append(op)
        self.save()
        return op

    def call(self, label, argv, requests=0, transfer=0, expected=0, cleanup=False, stdin=None):
        op = self.reserve(label, requests, transfer, cleanup)
        try:
            r = subprocess.run([str(x) for x in argv], cwd=ROOT, env=self.env,
                               input=stdin, capture_output=True, timeout=90)
        except BaseException as e:
            op.update(result="unknown", error=type(e).__name__)
            self.save()
            raise
        op.update(finished=now(), exit_code=r.returncode,
                  result="passed" if r.returncode == expected else "failed")
        # Commands are known application/data APIs only. Auth material is never logged.
        op["stdout"] = r.stdout.decode(errors="replace")[:32768]
        op["stderr"] = r.stderr.decode(errors="replace")[:8192]
        self.save()
        if r.returncode != expected:
            raise RuntimeError(label + ": unexpected exit " + str(r.returncode))
        return r

    def cli(self, label, args, requests=0, transfer=0, expected=0, cleanup=False):
        return self.call(label, [ROOT / "target/bin/private-archive", "--config", self.config,
                                *args], requests, transfer, expected, cleanup)

    def aws(self, label, args, requests=1, transfer=0, expected=0, cleanup=False):
        # No high-level s3 commands, pagination, or infrastructure/configuration APIs.
        allowed = {"list-objects-v2", "head-object", "get-object", "put-object", "delete-object"}
        if args[0] not in allowed:
            raise RuntimeError("non-data AWS API rejected")
        return self.call(label, ["/usr/local/bin/aws", "s3api", *args,
                                "--region", REGION, "--no-paginate", "--no-cli-pager"],
                         requests, transfer, expected, cleanup)

    def own(self, suffix):
        key = self.key(suffix)
        self.s["checks"]["bucket_empty"] = False
        if key not in self.s["owned_keys"]:
            if len(self.s["owned_keys"]) >= 4:
                raise RuntimeError("object allowance exhausted")
            self.s["owned_keys"].append(key)
            self.save()  # also tracks writes with uncertain outcomes

    def config_index(self, value):
        self.cli("configure index", ["cfg", "set", "index.location", str(value)])

    def head(self, suffix, label="head own object"):
        r = self.aws(label, ["head-object", "--bucket", BUCKET, "--key", self.key(suffix),
                            "--expected-bucket-owner", ACCOUNT])
        d = json.loads(r.stdout)
        if d["ContentLength"] > MAX_FILE:
            raise RuntimeError("unexpected object size; do not download")
        return d

    def conditional_put(self, suffix, body, etag):
        # Installed AWS CLI 2.18.3 predates PutObject If-Match. curl signs the
        # one explicit request. Credentials travel via stdin, never argv/file.
        def quote(v):
            if any(c in v for c in "\r\n"):
                raise RuntimeError("invalid header/config value")
            return '"' + v.replace('\\', '\\\\').replace('"', '\\"') + '"'
        config = ["aws-sigv4 = " + quote("aws:amz:" + REGION + ":s3"),
                  "user = " + quote(self.env["AWS_ACCESS_KEY_ID"] + ":" + self.env["AWS_SECRET_ACCESS_KEY"]),
                  "header = " + quote("x-amz-security-token: " + self.env["AWS_SESSION_TOKEN"]),
                  "header = " + quote("If-Match: " + etag),
                  "header = " + quote("x-amz-expected-bucket-owner: " + ACCOUNT)]
        r = self.call("stale If-Match PUT", ["/usr/bin/curl", "--disable", "--silent", "--show-error",
                      "--config", "-", "--proto", "=https", "--max-time", "45",
                      "--connect-timeout", "10", "--max-filesize", str(MAX_FILE),
                      "--request", "PUT", "--upload-file", body,
                      "--output", self.work / "conditional-error.xml", "--write-out", "%{http_code}",
                      "https://" + BUCKET + ".s3." + REGION + ".amazonaws.com/" + self.key(suffix)],
                      requests=1, stdin=("\n".join(config) + "\n").encode())
        assert r.stdout == b"412", r.stdout
        assert b"PreconditionFailed" in (self.work / "conditional-error.xml").read_bytes()

    def start(self):
        identity = self.call("application role identity", ["/usr/local/bin/aws", "sts", "get-caller-identity", "--region", REGION, "--no-cli-pager"])
        ident = json.loads(identity.stdout)
        assert ident["Account"] == ACCOUNT and ":assumed-role/private-archive-role/" in ident["Arn"]
        listing = json.loads(self.aws("initial empty bucket", ["list-objects-v2", "--bucket", BUCKET,
                    "--expected-bucket-owner", ACCOUNT, "--max-keys", "1"]).stdout)
        assert listing["KeyCount"] == 0 and not listing["IsTruncated"], "bucket contains non-test data"
        self.sample.write_bytes(bytes(range(256)) * 256)
        self.sample.chmod(0o640)
        os.utime(self.sample, (1700000000, 1700000000))
        self.s["sample_sha256"] = digest(self.sample)
        self.cli("initialize test config", ["cfg", "init"])
        self.config_index(self.index)
        self.own("data/source.bin")
        self.cli("local to S3", ["cp", self.sample, self.uri("data/source.bin")], 16)
        self.own("data/copy.bin")
        self.cli("S3 to S3", ["cp", self.uri("data/source.bin"), self.uri("data/copy.bin")], 16)
        downloaded = self.work / "download.bin"
        self.cli("S3 to local", ["cp", self.uri("data/copy.bin"), downloaded], 16, SAMPLE_SIZE)
        assert digest(downloaded) == self.s["sample_sha256"]
        self.s["checks"]["three_copy_routes_content"] = True
        self.cli("reconcile standard scope", ["reconcile", self.uri("data/"), "--recursive"], 16)
        rows = json.loads(self.cli("local JSON listing", ["ls", self.uri("data/"), "--recursive", "--sort", "size-asc,name-desc", "--json"]).stdout)
        assert len(rows) == 2 and all(r["size_bytes"] == SAMPLE_SIZE for r in rows)
        assert all(r["unix_mode"] == "0640" and r["original_mtime"].startswith("2023-11-14T22:13:20") for r in rows)
        self.s["checks"]["attributes_listing_reconcile"] = True
        with sqlite3.connect(self.index) as db:
            db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
            assert db.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
        assert self.index.stat().st_size <= MAX_FILE
        self.own("index/index.db")
        self.aws("external ordinary DB upload", ["put-object", "--bucket", BUCKET, "--key", self.key("index/index.db"), "--body", str(self.index), "--storage-class", "STANDARD", "--expected-bucket-owner", ACCOUNT])
        self.config_index(self.uri("index/index.db"))
        self.cli("S3 index fetch and conditional save", ["reconcile", self.uri("data/"), "--recursive"], 16, MAX_FILE)
        first = self.head("index/index.db")
        cached = self.work / "external-index.db"
        self.aws("external DB download", ["get-object", "--bucket", BUCKET, "--key", self.key("index/index.db"), "--expected-bucket-owner", ACCOUNT, str(cached)], transfer=MAX_FILE)
        with sqlite3.connect(cached) as db:
            assert db.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
        self.config_index(cached)
        imported = json.loads(self.cli("ordinary downloaded DB listing", ["ls", self.uri("data/"), "--recursive", "--json"]).stdout)
        assert {r["uri"] for r in imported} == {r["uri"] for r in rows}
        self.s["checks"]["ordinary_db_local_s3_local"] = True
        not_modified = self.aws("conditional GET 304", ["get-object", "--bucket", BUCKET, "--key", self.key("index/index.db"), "--if-none-match", first["ETag"], "--expected-bucket-owner", ACCOUNT, str(self.work / "unchanged.db")], expected=254)
        assert b"304" in not_modified.stderr
        self.config_index(self.uri("index/index.db"))
        self.cli("change index version by explicit reconcile", ["reconcile", self.uri("data/"), "--recursive"], 16, MAX_FILE)
        second = self.head("index/index.db")
        assert second["ETag"] != first["ETag"], "index version did not change; stop without conflict request"
        self.conditional_put("index/index.db", cached, first["ETag"])
        assert self.head("index/index.db")["ETag"] == second["ETag"]
        self.s["checks"]["conditional_304_412_no_overwrite"] = True
        self.config_index(self.index)
        self.cli("delete standard data via application", ["rm", self.uri("data/"), "--recursive", "--auto-approve"], 16)
        self.cli("delete inactive remote index via application", ["rm", self.uri("index/"), "--recursive", "--auto-approve"], 16)
        self.s["checks"]["application_delete"] = True
        self.gui()
        self.cleanup()
        self.s["phase"] = "standard_passed"
        self.save()

    def gui(self):
        import shutil
        # Copy the real AWS-derived ordinary DB; remote data was deleted during
        # the bounded cleanup. The GUI must display the copy until reconcile.
        gui_index = self.work / "gui-index.db"
        shutil.copy2(self.work / "external-index.db", gui_index)
        self.config_index(gui_index)
        actions = [{"action":"budget","ms":60000}, {"action":"wait_idle"},
                   {"action":"select_uri","column":0,"uri":"s3://"+BUCKET,"kind":"root"}, {"action":"wait_idle"},
                   {"action":"select_uri","column":1,"uri":self.uri(""),"kind":"directory"}, {"action":"wait_idle"},
                   {"action":"select_uri","column":2,"uri":self.uri("data/"),"kind":"directory"}, {"action":"wait_idle"},
                   {"action":"select_uri","column":3,"uri":self.uri("data/source.bin"),"kind":"object"}, {"action":"wait_idle"},
                   {"action":"snapshot","name":"live-file"},
                   {"action":"select_uri","column":2,"uri":self.uri("data/"),"kind":"directory"}, {"action":"wait_idle"},
                   {"action":"reconcile"}, {"action":"wait_idle"}, {"action":"snapshot","name":"live-empty-reconcile"}]
        action_path = self.work / "gui-actions.json"; action_path.write_text(json.dumps(actions))
        report = self.work / "gui.json"
        self.call("real S3 GUI explicit reconcile", [ROOT / "target/p3-final-fixtures/private-archive-gui", "--config", self.config,
                  "--test-actions", action_path, "--test-report", report], requests=16)
        states = json.loads(report.with_suffix(".states.json").read_text())
        before = next(x for x in states if x["name"] == "live-file")
        after = next(x for x in states if x["name"] == "live-empty-reconcile")
        assert before["window_visible"] and len(before["details"]) == 10
        assert before["details"][2]["value"] == str(SAMPLE_SIZE)
        assert not after["error"] and after["columns"][0]["count"] == 0
        self.s["checks"]["gui_real_s3_reconcile_and_shared_db"] = True
        self.config_index(self.index)

    def finish_standard(self):
        # Resume only the uncompleted portion after the recorded 304 harness
        # mismatch; keep all earlier reservations and never repeat copy tests.
        assert self.s["checks"].get("ordinary_db_local_s3_local")
        assert self.s["checks"].get("conditional_get_304")
        assert not self.s["checks"].get("conditional_304_412_no_overwrite")
        cached = self.work / "external-index.db"
        self.aws("resume index fixture from verified DB", ["put-object", "--bucket", BUCKET, "--key", self.key("index/index.db"),
                 "--body", str(cached), "--storage-class", "STANDARD", "--expected-bucket-owner", ACCOUNT])
        first = self.head("index/index.db")
        self.config_index(self.uri("index/index.db"))
        self.cli("index version change from verified empty data scope", ["reconcile", self.uri("data/"), "--recursive"], 16, MAX_FILE)
        second = self.head("index/index.db")
        assert first["ETag"] != second["ETag"]
        self.conditional_put("index/index.db", cached, first["ETag"])
        assert self.head("index/index.db")["ETag"] == second["ETag"]
        self.s["checks"]["conditional_304_412_no_overwrite"] = True
        self.config_index(self.index)
        self.cli("delete inactive remote index via application", ["rm", self.uri("index/"), "--recursive", "--auto-approve"], 16)
        self.s["checks"]["application_delete"] = True
        self.gui()
        self.cleanup()
        self.s["phase"] = "standard_passed"
        self.save()

    def restore(self):
        assert self.s["phase"] == "standard_passed" and not self.s["restore_requested"]
        self.own("archive/sample.bin")
        self.cli("upload one Deep Archive object", ["cp", self.sample, self.uri("archive/sample.bin"), "--data-class", "archived"], 16)
        assert self.head("archive/sample.bin")["StorageClass"] == "DEEP_ARCHIVE"
        self.s["restore_requested"] = True  # never repeat, even after unknown response
        self.s["restore_requested_at"] = now()
        self.s["restore_deadline"] = (dt.datetime.now(dt.timezone.utc) + dt.timedelta(hours=72)).isoformat()
        self.save()
        self.cli("one Bulk restore request, one day", ["restore", "--restore-duration", "1d", "--restore-tier", "Bulk", self.uri("archive/sample.bin")], 16)
        self.s["phase"] = "awaiting_restore"
        self.s["checks"]["bucket_empty"] = False
        self.save()
        self.check()

    def check(self):
        if self.s["phase"] == "complete":
            return
        assert self.s["phase"] == "awaiting_restore" and self.s["restore_requested"]
        if dt.datetime.now(dt.timezone.utc) >= dt.datetime.fromisoformat(self.s["restore_deadline"]):
            self.s["phase"] = "restore_deadline_exceeded"
            self.cleanup()
            return
        d = self.head("archive/sample.bin", "explicit restore status check")
        self.s["last_restore_status"] = {k:d.get(k) for k in ["StorageClass", "ContentLength", "Restore"]}
        if 'ongoing-request="false"' in d.get("Restore", ""):
            downloaded = self.work / "restored.bin"
            self.cli("download restored object", ["cp", self.uri("archive/sample.bin"), downloaded], 16, SAMPLE_SIZE)
            assert digest(downloaded) == self.s["sample_sha256"]
            self.s["checks"]["deep_archive_restore_content"] = True
            self.cleanup()
            self.s["phase"] = "complete"
        self.save()

    def cleanup(self):
        self.cleanup_attempted = True
        failures = []
        for i,key in enumerate(self.s["owned_keys"]):
            if not key.startswith(self.s["prefix"]):
                raise RuntimeError("cleanup rejected an unowned key")
            try:
                self.aws("delete own tracked key " + str(i), ["delete-object", "--bucket", BUCKET, "--key", key, "--expected-bucket-owner", ACCOUNT], cleanup=True)
            except Exception as e:
                failures.append(str(e))
        try:
            listing = json.loads(self.aws("final bucket data empty check", ["list-objects-v2", "--bucket", BUCKET, "--expected-bucket-owner", ACCOUNT, "--max-keys", "1"], cleanup=True).stdout)
            self.s["checks"]["bucket_empty"] = listing["KeyCount"] == 0 and not listing["IsTruncated"]
            if not self.s["checks"]["bucket_empty"]:
                failures.append("unexpected data remains; do not delete untracked objects")
        except Exception as e:
            failures.append(str(e))
            self.s["checks"]["bucket_empty"] = False
        self.s["cleanup_failures"] = failures
        if not failures:
            self.s["cleaned_at"] = now()
        self.save()
        if failures:
            raise RuntimeError("cleanup incomplete; inspect ledger before retrying explicitly")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("phase", choices=["start", "finish-standard", "restore", "check", "cleanup"])
    args = p.parse_args()
    if not __debug__:
        raise RuntimeError("acceptance must run without Python optimization")
    lock = REPORT.with_suffix(".lock").open("a")
    fcntl.flock(lock, fcntl.LOCK_EX)
    run = Run(start=args.phase == "start")
    try:
        getattr(run, args.phase.replace("-", "_"))()
    except BaseException as e:
        run.s["errors"].append({"time":now(),"type":type(e).__name__,"message":str(e)})
        run.s["phase"] = "failed"
        run.save()
        # One bounded cleanup pass; never repeat the failed operation.
        if not run.cleanup_attempted:
            try:
                run.cleanup()
            except BaseException as cleanup_error:
                run.s["errors"].append({"time":now(),"type":"cleanup", "message":str(cleanup_error)})
                run.save()
        raise
    print(json.dumps({k:run.s.get(k) for k in ["phase", "checks", "requests_reserved", "transfer_reserved", "conservative_reserved_cost_usd", "restore_deadline"]}))


if __name__ == "__main__":
    main()
