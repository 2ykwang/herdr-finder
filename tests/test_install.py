"""Exercise release installation without network access or compilers."""

from pathlib import Path
import hashlib
import io
import os
import shutil
import subprocess
import tarfile
import tempfile
import unittest


class InstallTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="finder install ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "scripts").mkdir()
        (self.root / "bin").mkdir()
        self.old = b"previous binary\n"
        (self.root / "bin/herdr-finder").write_bytes(self.old)
        source = Path(__file__).resolve().parents[1]
        shutil.copy2(source / "scripts/install.sh", self.root / "scripts/install.sh")
        (self.root / "herdr-plugin.toml").write_text('version = "1.0.0"\n')
        self.assets = self.root / "assets"
        self.assets.mkdir()
        commands = self.root / "commands"
        commands.mkdir()
        (commands / "uname").write_text(
            '#!/bin/sh\ncase "$1" in\n'
            '-s) printf "%s\\n" "$FINDER_TEST_OS";;\n'
            '-m) printf "%s\\n" "$FINDER_TEST_ARCH";;\nesac\n'
        )
        (commands / "curl").write_text(
            '#!/usr/bin/env python3\n'
            'from pathlib import Path\nimport os,shutil,sys\n'
            'url=sys.argv[-1]\n'
            'assert url.startswith("https://github.com/2ykwang/herdr-finder/releases/download/v1.0.0/")\n'
            'src=Path(os.environ["FINDER_TEST_ASSETS"])/url.rsplit("/",1)[-1]\n'
            'if not src.is_file():sys.exit(22)\n'
            'shutil.copyfile(src,sys.argv[sys.argv.index("--output")+1])\n'
        )
        for name in ["cargo", "rustc", "cc", "gcc", "clang"]:
            (commands / name).write_text("#!/bin/sh\nexit 99\n")
        for path in commands.iterdir():
            path.chmod(0o755)
        self.env = dict(os.environ, PATH=str(commands) + os.pathsep + os.environ["PATH"],
                        FINDER_TEST_OS="Darwin", FINDER_TEST_ARCH="arm64",
                        FINDER_TEST_ASSETS=str(self.assets))

    def archive(self, target="aarch64-apple-darwin", version="1.0.0", member=None):
        name = f"herdr-finder-1.0.0-{target}.tar.gz"
        path = self.assets / name
        binary = f"#!/bin/sh\nprintf '%s\\n' '{version}'\n".encode()
        with tarfile.open(path, "w:gz") as tar:
            info = tarfile.TarInfo(member or "herdr-finder-1.0.0/bin/herdr-finder")
            info.size = len(binary)
            info.mode = 0o755
            tar.addfile(info, io.BytesIO(binary))
        (self.assets / (name + ".sha256")).write_text(
            hashlib.sha256(path.read_bytes()).hexdigest() + "  " + name + "\n"
        )
        return path, binary

    def install(self):
        return subprocess.run(["sh", "scripts/install.sh"], cwd=self.root, env=self.env,
                              text=True, capture_output=True, timeout=5)

    def assert_kept(self, result):
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((self.root / "bin/herdr-finder").read_bytes(), self.old)
        self.assertEqual(list((self.root / "bin").glob(".herdr-finder.*")), [])

    def test_four_platforms_install_without_compilers(self):
        for system, arch, target in [
            ("Darwin", "arm64", "aarch64-apple-darwin"),
            ("Darwin", "x86_64", "x86_64-apple-darwin"),
            ("Linux", "x86_64", "x86_64-unknown-linux-musl"),
            ("Linux", "aarch64", "aarch64-unknown-linux-musl"),
        ]:
            with self.subTest(target=target):
                _, binary = self.archive(target)
                self.env.update(FINDER_TEST_OS=system, FINDER_TEST_ARCH=arch)
                result = self.install()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((self.root / "bin/herdr-finder").read_bytes(), binary)
                self.assertEqual((self.root / "bin/herdr-finder").stat().st_mode & 0o777, 0o755)

    def test_bad_checksum_keeps_existing_binary(self):
        path, _ = self.archive()
        Path(str(path) + ".sha256").write_text("0" * 64 + "  " + path.name + "\n")
        self.assert_kept(self.install())

    def test_checksum_for_another_filename_is_rejected(self):
        path, _ = self.archive()
        Path(str(path) + ".sha256").write_text(hashlib.sha256(path.read_bytes()).hexdigest() + "  other.tar.gz\n")
        self.assert_kept(self.install())

    def test_missing_release_keeps_existing_binary(self):
        self.assert_kept(self.install())

    def test_missing_checksum_keeps_existing_binary(self):
        path, _ = self.archive()
        Path(str(path) + ".sha256").unlink()
        self.assert_kept(self.install())

    def test_archive_without_expected_binary_is_rejected(self):
        self.archive(member="herdr-finder-1.0.0/other")
        self.assert_kept(self.install())

    def test_wrong_binary_version_is_rejected(self):
        self.archive(version="9.9.9")
        self.assert_kept(self.install())

    def test_unsupported_cpu_does_not_replace_binary(self):
        self.env["FINDER_TEST_ARCH"] = "ppc64"
        self.assert_kept(self.install())


if __name__ == "__main__":
    unittest.main()
