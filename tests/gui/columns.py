"""Native column resizing and wheel routing; local 10-level/300-file fixture."""
import argparse
import json
import pathlib
import tempfile

from workflow import Runner, faults, idle, seed, select, shot


def drag(x, end, y=240):
    actions = [{"action": "pointer_down", "x": x, "y": y}]
    actions += [
        {"action": "pointer_move", "x": x + (end - x) * step / 8, "y": y, "ms": 40}
        for step in range(1, 9)
    ]
    return actions + [{"action": "pointer_up", "x": end, "y": y}]


def wheel(x, dx=0, dy=0, y=300):
    return {"action": "scroll", "x": x, "y": y, "dx": dx, "dy": dy, "ms": 250}


def offsets(state):
    return [round(c["scroll_y"], 1) for c in state["columns"]]


def near(actual, expected):
    assert abs(actual - expected) < 1, (actual, expected)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=pathlib.Path, required=True)
    parser.add_argument("--report", type=pathlib.Path, required=True)
    parser.add_argument("--screenshots", type=pathlib.Path, required=True)
    parser.add_argument("--case", choices=["all", "resize", "vertical", "horizontal"], default="all")
    args = parser.parse_args()
    args.screenshots.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="archive-gui-columns-") as directory:
        work = pathlib.Path(directory)
        (work / "config").write_text("")
        prefix = ""
        keys, navigation = [], [select(0, "s3://bucket", "root")]
        for depth in range(11):
            keys += [(prefix + f"f{number:03}.txt", number + 1) for number in range(300)]
            if depth < 10:
                prefix += f"level{depth + 1:02}/"
                navigation.append(select(depth + 1, "s3://bucket/" + prefix))
        seed(work / "index.db", keys)

        def trap(request, state):
            raise AssertionError(("local browsing contacted S3", request))

        with faults.fixture(trap) as (network, endpoint):
            # Ordinary aws namespace, dummy credentials; SDK endpoints and
            # HTTP proxies point to a trap without changing the index namespace.
            runner = Runner(args.binary.resolve(), work, args.screenshots, network_trap=endpoint)
            if args.case in ("all", "resize"):
                actions = [navigation[0], shot("initial")]
                actions += drag(241, 361) + [shot("wider")]
                actions += drag(606, 666) + [shot("second-wider")]
                actions += [{"action": "pointer_move", "x": 500, "y": 240}, shot("released")]
                actions += [{"action": "click", "x": 400, "y": 152}, idle(), shot("selected")]
                actions += [{"action": "reload"}, idle(), shot("reloaded")]
                actions += drag(361, 20) + [shot("minimum")]
                actions += drag(156, 850) + [shot("maximum")]
                actions += drag(716, 241) + [shot("restored")]
                resized = runner.run("resize", actions)
                near(resized["initial"]["row_height"], 24)
                near(resized["wider"]["columns"][0]["width"], 365)
                near(resized["second-wider"]["columns"][0]["width"], 365)
                near(resized["second-wider"]["columns"][1]["width"], 305)
                for name in ["released", "selected", "reloaded"]:
                    near(resized[name]["columns"][0]["width"], 365)
                    near(resized[name]["columns"][1]["width"], 305)
                assert resized["selected"]["details"][0]["value"] == "s3://bucket/f000.txt"
                near(resized["minimum"]["columns"][0]["width"], 160)
                near(resized["maximum"]["columns"][0]["width"], 720)
                near(resized["restored"]["columns"][0]["width"], 245)

            if args.case in ("all", "vertical"):
                # No selection/click changes focus between wheel events: only the
                # pointer position chooses the column. Scroll both ways and beyond
                # the first 256-row page, then click a real rendered late-page row.
                actions = navigation[:2] + [shot("initial")]
                actions += [wheel(350, dy=-240), shot("left-down")]
                actions += [wheel(600, dy=-480), shot("right-down")]
                actions += [wheel(350, dy=96), shot("left-up")]
                actions += [wheel(600, dy=-12000), shot("bottom")]
                actions += [{"action": "click", "x": 600, "y": 152}, idle(), shot("late-selected")]
                actions += [wheel(600, dy=12000), shot("right-top")]
                actions += [wheel(350, dy=12000), shot("both-top")]
                vertical = runner.run("vertical", actions)
                assert [c["count"] for c in vertical["initial"]["columns"]] == [1, 301, 301]
                assert offsets(vertical["initial"]) == [0, 0, 0]
                assert offsets(vertical["left-down"]) == [0, -240, 0]
                assert offsets(vertical["right-down"]) == [0, -240, -480]
                assert offsets(vertical["left-up"]) == [0, -144, -480]
                assert offsets(vertical["bottom"])[1] == -144
                assert offsets(vertical["bottom"])[2] < -6000
                assert offsets(vertical["late-selected"]) == offsets(vertical["bottom"])
                assert vertical["late-selected"]["columns"][2]["selected"] >= 256
                assert vertical["late-selected"]["details"][0]["value"].startswith("s3://bucket/level01/f2")
                assert offsets(vertical["right-top"]) == [0, -144, 0]
                assert offsets(vertical["both-top"]) == [0, 0, 0]
                for state in vertical.values():
                    near(state["columns_x"], 0)

            if args.case in ("all", "horizontal"):
                # Ten nested folders produce twelve columns including bucket list
                # and bucket content. Wheel horizontally over populated ListViews,
                # then check the bottom scrollbar through a real pointer drag too.
                actions = navigation + [shot("deep-left")]
                actions += [wheel(350, dx=-600), shot("deep-middle")]
                actions += drag(376, 426) + [shot("deep-resized")]
                actions += [wheel(350, dx=-12000), shot("deep-right")]
                actions += [wheel(350, dx=600), shot("deep-back")]
                actions += [wheel(350, dx=12000), shot("deep-left-again")]
                actions += [wheel(350, dx=-12000)]
                actions += drag(740, 50, y=719) + [shot("scrollbar-left")]
                horizontal = runner.run("horizontal", actions)
                left = horizontal["deep-left"]
                assert len(left["columns"]) == 12
                assert left["columns"][-1]["count"] == 300
                assert left["columns_content_width"] > left["columns_visible_width"] * 3
                near(left["columns_x"], 0)
                near(horizontal["deep-middle"]["columns_x"], -600)
                near(horizontal["deep-resized"]["columns"][3]["width"], 295)
                end = horizontal["deep-right"]
                near(end["columns_x"], end["columns_visible_width"] - end["columns_content_width"])
                near(horizontal["deep-back"]["columns_x"], end["columns_x"] + 600)
                near(horizontal["deep-left-again"]["columns_x"], 0)
                near(horizontal["scrollbar-left"]["columns_x"], 0)
                for state in horizontal.values():
                    assert all(y == 0 for y in offsets(state)), offsets(state)
            assert not network.seen
            for result in runner.results:
                for state in result["states"]:
                    assert not state["error"] and state["window_visible"]
                    assert state["row_cache_bytes"] <= 32 * 1024 * 1024
            summary = {
                "passed": len(runner.results),
                "depth": 10,
                "files_per_level": 300,
                "objects": len(keys),
                "row_height": 24,
                "real_pointer_drag": any(c["name"] in ("resize", "horizontal") for c in runner.results),
                "real_wheel_events": any(c["name"] in ("vertical", "horizontal") for c in runner.results),
                "network_requests": len(network.seen),
                "real_aws_requests": 0,
                "cases": runner.results,
            }
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({k: v for k, v in summary.items() if k != "cases"}))


if __name__ == "__main__":
    main()
