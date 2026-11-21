#!/usr/bin/env python3
"""Fill the real sha256s into plugin/plugin.json binaries after a release.

Usage:
  # from built files:
  scripts/update_manifest.py --version 0.1.0 --dir ./dist
  # dist/ must hold the four plain binaries:
  #   hoo-browser-linux-x86_64, hoo-browser-linux-aarch64,
  #   hoo-browser-darwin-aarch64, hoo-browser-darwin-x86_64
  # from release assets (needs `gh`):
  scripts/update_manifest.py --version 0.1.0 --from-release

The release workflow runs this and opens the manifest PR itself; humans use
it the same way when cutting a release by hand.
"""
import argparse
import hashlib
import json
import subprocess
import sys
from pathlib import Path

PLATFORMS = [
    "linux-x86_64",
    "linux-aarch64",
    "darwin-aarch64",
    "darwin-x86_64",
]
REPO = "kolisachint/hoo-browser-plugin"


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True, help="release version, e.g. 0.1.0")
    parser.add_argument("--dir", help="directory holding the built hoo-browser-<platform> files")
    parser.add_argument("--from-release", action="store_true", help="download assets from the GitHub release with gh")
    args = parser.parse_args()

    root = Path(__file__).resolve().parent.parent
    manifest_path = root / "plugin" / "plugin.json"
    manifest = json.loads(manifest_path.read_text())
    if manifest.get("version") != args.version:
        print(f"plugin.json version is {manifest.get('version')}, expected {args.version}", file=sys.stderr)
        return 1

    workdir = Path(args.dir) if args.dir else root / "dist"
    if args.from_release:
        workdir.mkdir(exist_ok=True)
        for platform in PLATFORMS:
            asset = f"hoo-browser-{platform}"
            subprocess.run(
                ["gh", "release", "download", f"v{args.version}", "--repo", REPO,
                 "--pattern", asset, "--dir", str(workdir), "--clobber"],
                check=True,
            )
    binaries = manifest["extensions"]["dev.hoocode"]["native"]["binaries"]
    for platform in PLATFORMS:
        path = workdir / f"hoo-browser-{platform}"
        if not path.is_file():
            print(f"missing {path}", file=sys.stderr)
            return 1
        digest = sha256_of(path)
        url = f"https://github.com/{REPO}/releases/download/v{args.version}/hoo-browser-{platform}"
        binaries[platform] = {"url": url, "sha256": digest}
        print(f"{platform}: {digest}  {url}")
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"wrote {manifest_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
