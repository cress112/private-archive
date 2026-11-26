"""Package an existing native GUI as a relocatable, ad-hoc signed .app."""
import argparse
import os
import pathlib
import platform
import plistlib
import shutil
import subprocess
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]


def package(binary, output):
    assert platform.system() == "Darwin", "packaging needs the macOS SDK and codesign"
    binary, output = binary.resolve(), output.absolute()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError("--binary must be an executable native GUI")
    if output.suffix != ".app" or output.exists():
        raise ValueError("choose a new .app path; existing bundles are never overwritten")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".archive-app-", dir=output.parent) as directory:
        bundle = pathlib.Path(directory) / output.name
        macos = bundle / "Contents/MacOS"
        macos.mkdir(parents=True)
        (bundle / "Contents/Resources").mkdir()
        gui = macos / "private-archive-gui"
        shutil.copy2(binary, gui)
        gui.chmod(0o755)
        shutil.copy2(ROOT / "scripts/private-archive-session.sh", bundle / "Contents/Resources/private-archive-session.sh")
        launcher = macos / "private-archive-launcher"
        subprocess.run(["xcrun", "clang", "-std=c11", "-Wall", "-Wextra", "-Werror", "-O2",
                        "-mmacosx-version-min=12.0", str(ROOT / "scripts/macos/launcher.c"),
                        "-o", str(launcher)], check=True)
        metadata = {
            "CFBundleDevelopmentRegion": "ja", "CFBundleDisplayName": "Private Archive",
            "CFBundleName": "Private Archive", "CFBundleExecutable": launcher.name,
            "CFBundleIdentifier": "local.private-archive.gui", "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": "0.1.0", "CFBundleVersion": "1",
            "LSMinimumSystemVersion": "12.0", "NSHighResolutionCapable": True,
        }
        (bundle / "Contents/Info.plist").write_bytes(plistlib.dumps(metadata))
        (bundle / "Contents/PkgInfo").write_bytes(b"APPL????")
        for target in [gui, bundle]:
            subprocess.run(["codesign", "--force", "--sign", "-", str(target)], check=True)
        subprocess.run(["codesign", "--verify", "--deep", "--strict", str(bundle)], check=True)
        bundle.rename(output)
    return output


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, default=ROOT / "target/bin/private-archive-gui")
    parser.add_argument("--output", type=pathlib.Path, default=ROOT / "target/apps/Private Archive.app")
    args = parser.parse_args()
    print(package(args.binary, args.output))
