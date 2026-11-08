#!/usr/bin/env python3
"""Build reproducible source and platform-labeled binary archives."""

from __future__ import annotations

import gzip
import hashlib
import io
import json
import subprocess
import tarfile
from pathlib import Path


def main() -> None:
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
    binary = next(target["name"] for target in package["targets"] if "bin" in target["kind"])
    host = next(
        line.removeprefix("host: ")
        for line in subprocess.check_output(["rustc", "-vV"], text=True).splitlines()
        if line.startswith("host: ")
    )
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
            "tools/package_release.py",
            "assets/demo.gif",
            "assets/search.png",
        )
    ]
    files.extend(sorted((root / "src").rglob("*.rs")))
    files.extend(sorted((root / "tests").rglob("*.rs")))

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
