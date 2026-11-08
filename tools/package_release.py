#!/usr/bin/env python3
"""Build reproducible source and platform-labeled binary archives."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import re
import subprocess
import tarfile
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", help="Rust target of the binary in bin/")
    parser.add_argument("--tag", help="Require this release tag to match the package version")
    parser.add_argument("--check-version", action="store_true", help="Validate the release version without packaging")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
            cwd=root,
            text=True,
        )
    )
    package = next(
        package
        for package in metadata["packages"]
        if Path(package["manifest_path"]).resolve() == root / "Cargo.toml"
    )
    version = package["version"]
    manifest = (root / "herdr-plugin.toml").read_text()
    manifest_version = re.search(r'^version\s*=\s*"([^"]+)"', manifest, re.MULTILINE)
    if not manifest_version or manifest_version[1] != version:
        raise SystemExit("Cargo.toml and herdr-plugin.toml versions must match")
    if args.tag is not None and args.tag != f"v{version}":
        raise SystemExit(f"Expected tag v{version}, got {args.tag}")
    if args.check_version:
        print(version)
        return
    binary = next(target["name"] for target in package["targets"] if "bin" in target["kind"])
    host = args.target or next(
        line.removeprefix("host: ")
        for line in subprocess.check_output(["rustc", "-vV"], text=True).splitlines()
        if line.startswith("host: ")
    )
    if not re.fullmatch(r"[a-zA-Z0-9_-]+", host):
        raise SystemExit("Invalid Rust target")
    base = f"{package['name']}-{version}"
    files = [
        root / name
        for name in (
            "Cargo.toml",
            "Cargo.lock",
            "herdr-plugin.toml",
            "README.md",
            "LICENSE",
            "THIRD_PARTY.md",
            "third-party/herdr-LICENSE",
            ".gitignore",
            "scripts/build.sh",
            "scripts/install.sh",
            ".github/workflows/release.yml",
            "tools/package_release.py",
            "assets/demo.gif",
            "assets/search.png",
        )
    ]
    files.extend(sorted((root / "src").rglob("*.rs")))
    files.extend(sorted((root / "tests").rglob("*.rs")))
    files.extend(sorted((root / "tests").rglob("*.py")))

    dist = root / "dist"
    dist.mkdir(exist_ok=True)
    for tag, extra in (("source", []), (host, [root / "bin" / binary])):
        target = dist / f"{base}-{tag}.tar.gz"
        with target.open("wb") as output:
            with gzip.GzipFile(filename="", fileobj=output, mode="wb", mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w") as archive:
                    for path in sorted(files + extra):
                        data = path.read_bytes()
                        info = tarfile.TarInfo(f"{base}/{path.relative_to(root)}")
                        info.size = len(data)
                        executable = path.suffix == ".sh" or path.parent.name == "bin"
                        info.mode = 0o755 if executable else 0o644
                        archive.addfile(info, io.BytesIO(data))
        digest = hashlib.sha256(target.read_bytes()).hexdigest()
        target.with_suffix(target.suffix + ".sha256").write_text(f"{digest}  {target.name}\n")
        print(f"{target}\n{digest}")


if __name__ == "__main__":
    main()
