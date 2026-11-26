"""CLI/GUI shell and macOS native launchers share the credential boundary."""
import json
import pathlib
import runpy

if __name__ == "__main__":
    helper = pathlib.Path(__file__).resolve().parents[1] / "cli/wrapper.py"
    print(json.dumps(runpy.run_path(str(helper))["run_suite"](include_native=True)))
