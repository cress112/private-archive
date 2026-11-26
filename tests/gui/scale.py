"""Measure real native GUI RSS while browsing/sorting a million-row column."""
import argparse
import heapq
import json
import pathlib
import re
import subprocess
import tempfile

from columns import wheel
from workflow import ROOT, Runner, faults, idle, seed, select, shot, wait_state


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=pathlib.Path, required=True)
    parser.add_argument("--report", type=pathlib.Path, required=True)
    parser.add_argument("--screenshots", type=pathlib.Path, required=True)
    parser.add_argument("--count", type=int, default=1_000_000)
    parser.add_argument("--modes", nargs="+", choices=["uri", "size-name", "responsive"], default=["uri", "size-name", "responsive"])
    args = parser.parse_args()
    assert args.count >= 1000
    args.screenshots.mkdir(parents=True, exist_ok=True)
    args.report.parent.mkdir(parents=True, exist_ok=True)
    count = args.count
    helper = ROOT / "target/p3-window-helper"
    if not helper.exists():
        subprocess.run(["xcrun", "clang", "-fobjc-arc", str(ROOT / "tests/gui/macos_window.m"), "-framework", "AppKit", "-framework", "ImageIO", "-o", str(helper)], check=True)
    def size(n): return None if n % 29 == 0 else n * 7919 % 10007
    def uri(n): return f"s3://bucket/f{n:07}.txt"
    results = []
    with tempfile.TemporaryDirectory(prefix="archive-gui-scale-") as directory:
        work = pathlib.Path(directory)
        (work / "config").write_text("")
        seed(work / "index.db", ((f"f{n:07}.txt", size(n)) for n in range(count)))
        expected = heapq.nsmallest(256, range(count), key=lambda n: (size(n) is None, size(n) or 0, -n))
        def trap(request, state): return faults.error(500)
        with faults.fixture(trap) as (network, endpoint):
            for mode in args.modes:
                stats = work / (mode + "-time.txt")
                runner = Runner(args.binary.resolve(), work, args.screenshots, network_trap=endpoint,
                                command_prefix=["/usr/sbin/taskpolicy", "-a", "-d", "default", "-g", "default", "/usr/bin/time", "-l", "-o", str(stats), "/usr/bin/caffeinate", "-d", "-i"], timeout=955)
                actions = [{"action": "budget", "ms": 900000},
                           idle(), shot("roots")]
                if mode == "size-name": actions += [{"action": "sort", "terms": [2, 1]}, idle()]
                actions += [select(0, "s3://bucket", "root")]
                actions += [shot("ready")]
                if mode == "responsive":
                    actions += [{"action": "sort", "terms": [2, 1], "ms": 20},
                                {"action": "snapshot", "name": "query-inflight", "force": True, "ms": 20},
                                {"action": "top", "force": True, "ms": 20},
                                {"action": "snapshot", "name": "query-responsive", "force": True},
                                idle(), shot("top")]
                else:
                    actions += [wheel(350, dy=-count * 24), shot("bottom"),
                                {"action": "click", "x": 350, "y": 152}, idle(), shot("selected")]
                def foreground(process, report):
                    wait_state(process, report, "roots", timeout=60)
                    text = subprocess.check_output(["ps", "-axo", "pid,ppid,comm"], text=True)
                    processes = {int(parts[0]): (int(parts[1]), parts[2]) for line in text.splitlines()[1:] if len(parts := line.strip().split(None, 2)) == 3}
                    for pid, (_, executable) in processes.items():
                        if executable != str(args.binary.resolve()): continue
                        ancestor = pid
                        for _ in range(8):
                            ancestor = processes.get(ancestor, (0, ""))[0]
                            if ancestor == process.pid:
                                subprocess.run(["taskpolicy", "-B", "-p", str(pid)], check=True, timeout=10)
                                subprocess.run([str(helper), "activate", str(pid)], check=True, timeout=10)
                                return
                            if not ancestor: break
                    raise AssertionError("measured GUI process not found")
                states = runner.run("scale-" + mode, actions, coordinate=foreground)
                ready = states["ready"]
                assert ready["columns"][1]["count"] == count
                names = [row["uri"] for row in ready["columns"][1]["rows"]]
                assert names == [uri(n) for n in (expected if mode == "size-name" else range(256))], (mode, names[:4])
                if mode == "responsive":
                    assert states["query-inflight"]["loading"]
                    latency = states["query-responsive"]["elapsed_ms"] - states["query-inflight"]["elapsed_ms"]
                    assert latency < 3000, latency
                    assert len(states["top"]["columns"]) == 1
                else:
                    assert states["selected"]["columns"][1]["selected"] >= count - 30
                    assert states["bottom"]["columns"][1]["scroll_y"] == states["selected"]["columns"][1]["scroll_y"]
                for state in states.values():
                    assert not state["error"], (mode, state["error"])
                    assert state["row_cache_bytes"] <= 32 * 1024 * 1024
                text = stats.read_text()
                match = re.search(r"(\d+)\s+maximum resident set size", text)
                assert match, text
                rss = int(match[1]); assert rss <= 512 * 1024 * 1024, (mode, rss)
                result = {"mode": mode, "count": count, "rss_bytes": rss,
                          "seconds": runner.results[0]["seconds"], "max_row_cache_bytes": max(s["row_cache_bytes"] for s in states.values()),
                          "passed": True, "states": runner.results[0]["states"]}
                if mode == "responsive": result["response_ms"] = latency
                results.append(result)
                print(json.dumps({k:v for k,v in result.items() if k != "states"}), flush=True)
                args.report.write_text(json.dumps({"cases": results, "passed": len(results), "network_requests": len(network.seen), "real_aws_requests": 0}, indent=2, ensure_ascii=False) + "\n")
            assert not network.seen


if __name__ == "__main__": main()
