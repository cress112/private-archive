"""ARM64 Xvfb GUI + Mac CLI share a DB/config and a tiny Seaweed prefix."""
import argparse
import contextlib
import json
import os
import pathlib
import select as sockets
import shutil
import socket
import socketserver
import subprocess
import tempfile
import threading
import uuid

from workflow import faults, idle, select, shot, wait_state
from startup import save_png

S3 = "private-archive-s3-tests-s3-1"
ENDPOINT = "http://127.0.0.1:8333"


@contextlib.contextmanager
def forwarding():
    class Forward(socketserver.BaseRequestHandler):
        def handle(self):
            with socket.create_connection(("127.0.0.1", 18333), timeout=20) as remote:
                peers = {self.request: remote, remote: self.request}
                while True:
                    ready, _, _ = sockets.select(list(peers), [], [], 30)
                    if not ready: return
                    for source in ready:
                        block = source.recv(65536)
                        if not block: return
                        peers[source].sendall(block)
    class Server(socketserver.ThreadingTCPServer):
        allow_reuse_address = True
        daemon_threads = True
    with Server(("127.0.0.1", 8333), Forward) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
        try: yield
        finally: server.shutdown(); thread.join()


def docker(*args, timeout=60):
    return subprocess.run(["docker", *map(str, args)], capture_output=True, check=True, timeout=timeout).stdout.decode()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=pathlib.Path, required=True)
    parser.add_argument("--cli", type=pathlib.Path, required=True)
    parser.add_argument("--report", type=pathlib.Path, required=True)
    parser.add_argument("--screenshots", type=pathlib.Path, required=True)
    args = parser.parse_args(); args.screenshots.mkdir(parents=True, exist_ok=True)
    header = args.binary.read_bytes()[:20]
    assert header[:5] == b"\x7fELF\x02" and int.from_bytes(header[18:20], "little") == 183
    prefix = "gui-p3-arm-" + uuid.uuid4().hex + "/"
    uri = "s3://private-archive-test/" + prefix
    name = "archive-gui-arm-" + uuid.uuid4().hex[:10]
    with tempfile.TemporaryDirectory(prefix="archive-gui-arm-") as directory, forwarding():
        work = pathlib.Path(directory).resolve(); (work / "config").write_text("[index]\nlocation='index.db'\n")
        other = work / "other"; other.mkdir(); (other / "config").write_text("")
        sample = work / "sample"; sample.write_bytes(b"ARM GUI")
        binary = work / "private-archive-gui"; shutil.copy2(args.binary, binary); binary.chmod(0o755)
        def cli(folder, tail, expected=0): return faults.run(args.cli.resolve(), ENDPOINT, folder, tail, expected)
        started = False
        try:
            cli(work, ["cp", str(sample), uri + "folder/a.txt"])
            docker("run", "--rm", "-d", "--platform", "linux/arm64", "--name", name,
                   "--network", "container:" + S3, "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
                   "--memory", "1g", "--cpus", "2", "--mount", f"type=bind,src={work},dst=/work",
                   "private-archive-gui-runtime:bookworm", "sleep", "infinity")
            started = True
            uname = docker("exec", name, "uname", "-sm").strip(); assert uname == "Linux aarch64"
            ldd = docker("exec", name, "ldd", "/work/private-archive-gui")
            assert not any(lib in ldd for lib in ["libsqlite", "libssl", "libcrypto"]), ldd
            docker("exec", name, "mkdir", "-m", "700", "-p", "/tmp/archive-runtime")
            ready = work / "external-ready"; deleted = work / "delete-ready"
            actions = [{"action":"budget","ms":300000}, idle(), select(0,"s3://private-archive-test","root"), idle(),
                       select(1,uri), idle(), select(2,uri+"folder/"), idle(), select(3,uri+"folder/a.txt","object"), idle(), shot("file"),
                       {"action":"settings"},{"action":"edit_setting","index":4,"value":"debug"},{"action":"save_settings"},idle(),shot("saved"),
                       {"action":"sort","terms":[1]},idle(),shot("sorted"),shot("before-external"),
                       {"action":"wait_file","path":"/work/external-ready"},shot("stale"),select(2,uri+"folder/"),idle(),
                       {"action":"reconcile"},idle(),shot("reconciled"),shot("before-delete"),
                       {"action":"wait_file","path":"/work/delete-ready"},shot("before-reload"),{"action":"reload"},idle(),shot("after-delete")]
            (work / "actions.json").write_text(json.dumps(actions))
            command = ["docker","exec","-e","HOME=/work/home",name,"xvfb-run","-a","-s","-screen 0 1280x900x24",
                       "/work/private-archive-gui","--config","/work/config","--index-location","/work/index.db",
                       "--test-endpoint",ENDPOINT,"--test-report","/work/arm.json","--test-actions","/work/actions.json"]
            proc = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                wait_state(proc,work/"arm.json","before-external",timeout=260)
                cli(other,["cp",str(sample),uri+"folder/b.txt"]);ready.touch()
                wait_state(proc,work/"arm.json","before-delete",timeout=120)
                cli(work,["rm",uri,"--recursive","--auto-approve"]);deleted.touch()
                out,err=proc.communicate(timeout=120)
            finally:
                if proc.poll() is None: proc.kill();proc.wait()
            assert proc.returncode==0 and not out,(proc.returncode,err.decode())
            states=json.loads((work/"arm.states.json").read_text());by={s["name"]:s for s in states}
            assert len(by["file"]["details"])==10 and by["file"]["details"][2]["value"]=="7"
            assert by["stale"]["columns"][3]["count"]==1
            assert by["reconciled"]["columns"][3]["count"]==2 and not by["reconciled"]["error"]
            assert by["before-reload"]["columns"][3]["count"]==2
            assert len(by["after-delete"]["columns"])==1 and by["after-delete"]["columns"][0]["count"]==0
            assert b'"debug"' in cli(work,["cfg","get","logging.level"]).stdout
            assert all(s["window_visible"] and s["row_cache_bytes"]<=32*1024*1024 and not s["error"] for s in states)
            for ppm in work.glob("arm*.ppm"): save_png(ppm,args.screenshots/(ppm.stem+".png"))
            result={"passed":True,"uname":uname,"elf_machine":183,"ldd":ldd,"states":states,"same_config_and_db":True,"test_prefix":prefix,"test_prefix_cleaned":True,"real_aws_requests":0}
            args.report.parent.mkdir(parents=True,exist_ok=True);args.report.write_text(json.dumps(result,ensure_ascii=False,indent=2)+"\n")
            print(json.dumps({"passed":True,"uname":uname,"test_prefix_cleaned":True,"real_aws_requests":0}))
        finally:
            if started: docker("stop",name,timeout=30)
            # Remove only this test's prefix, including if an earlier assertion failed.
            try: cli(other,["rm",uri,"--recursive","--auto-approve"],expected=1)
            except AssertionError: pass


if __name__ == "__main__": main()
