#!/usr/bin/env python3
"""Integration tests for restore-cargo-mtimes.py."""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("restore-cargo-mtimes.py").resolve()
BASE_TIME = 1_700_000_000
CHECKOUT_TIME = 1_800_000_000


def run(command: list[str], cwd: Path, *, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


class RestoreCargoMtimesTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.repo = Path(self.tempdir.name) / "repo"
        self.repo.mkdir()
        self.git("init", "-b", "main")
        self.git("config", "user.name", "Open Grok CI")
        self.git("config", "user.email", "ci@example.invalid")

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def git(self, *args: str, env: dict[str, str] | None = None) -> str:
        result = run(["git", *args], self.repo, env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.strip()

    def commit(self, message: str, timestamp: int) -> str:
        self.git("add", "-A")
        env = os.environ.copy()
        date = f"@{timestamp} +0000"
        env["GIT_AUTHOR_DATE"] = date
        env["GIT_COMMITTER_DATE"] = date
        self.git("commit", "-m", message, env=env)
        return self.git("rev-parse", "HEAD")

    def write(self, relative: str, content: str) -> Path:
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        return path

    def marker(self, commit: str) -> Path:
        marker = self.repo / "target/.open-grok-cache-commit"
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.write_text(f"{commit}\n", encoding="ascii")
        return marker

    def invoke(self, repo: Path | None = None) -> subprocess.CompletedProcess[str]:
        selected = self.repo if repo is None else repo
        return run(
            [sys.executable, str(SCRIPT), "--repo", str(selected)], selected
        )

    def test_missing_marker_is_safe_noop(self) -> None:
        source = self.write("source.txt", "one")
        self.commit("initial", BASE_TIME)
        os.utime(source, (CHECKOUT_TIME, CHECKOUT_TIME))

        result = self.invoke()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("marker", result.stdout)
        self.assertEqual(source.stat().st_mtime_ns, CHECKOUT_TIME * 1_000_000_000)

    def test_only_unchanged_regular_files_are_backdated(self) -> None:
        unchanged = self.write("dir/space and ünicode.txt", "same")
        leading_newline = self.write("\nleading-newline.txt", "same")
        header_like = self.write(f"OGMT:{'a' * 40}:123", "same")
        changed = self.write("changed.txt", "before")
        renamed = self.write("old-name.txt", "rename me")
        baseline = self.commit("baseline", BASE_TIME)

        changed.write_text("after", encoding="utf-8")
        self.git("mv", str(renamed.relative_to(self.repo)), "new name.txt")
        added = self.write("new.txt", "new")
        self.commit("change sources", BASE_TIME + 100)
        self.marker(baseline)

        current_paths = [
            unchanged,
            leading_newline,
            header_like,
            changed,
            self.repo / "new name.txt",
            added,
        ]
        for path in current_paths:
            os.utime(path, (CHECKOUT_TIME, CHECKOUT_TIME))
        artifact = self.write("target/fake-old-artifact", "cached")
        os.utime(artifact, (CHECKOUT_TIME - 100, CHECKOUT_TIME - 100))

        result = self.invoke()

        self.assertEqual(result.returncode, 0, result.stderr)
        expected_old = BASE_TIME * 1_000_000_000
        self.assertEqual(unchanged.stat().st_mtime_ns, expected_old)
        self.assertEqual(leading_newline.stat().st_mtime_ns, expected_old)
        self.assertEqual(header_like.stat().st_mtime_ns, expected_old)
        for path in (changed, self.repo / "new name.txt", added):
            self.assertEqual(path.stat().st_mtime_ns, CHECKOUT_TIME * 1_000_000_000)
            self.assertGreater(path.stat().st_mtime_ns, artifact.stat().st_mtime_ns)
        self.assertIn("left 3 changed/new files fresh", result.stdout)
        self.assertEqual(self.git("status", "--porcelain", "--untracked-files=no"), "")

    def test_merge_uses_first_parent_merge_timestamp(self) -> None:
        self.write("base.txt", "base")
        baseline = self.commit("baseline", BASE_TIME)
        self.git("switch", "-c", "side")
        merged = self.write("merged file.txt", "from side")
        self.commit("side commit with older clock", BASE_TIME - 100)
        self.git("switch", "main")
        self.write("main.txt", "main")
        self.commit("main advance", BASE_TIME + 50)
        env = os.environ.copy()
        env["GIT_AUTHOR_DATE"] = f"@{BASE_TIME + 100} +0000"
        env["GIT_COMMITTER_DATE"] = f"@{BASE_TIME + 100} +0000"
        self.git("merge", "--no-ff", "side", "-m", "merge side", env=env)
        merge_commit = self.git("rev-parse", "HEAD")
        self.assertNotEqual(baseline, merge_commit)
        self.marker(merge_commit)
        os.utime(merged, (CHECKOUT_TIME, CHECKOUT_TIME))

        result = self.invoke()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            merged.stat().st_mtime_ns, (BASE_TIME + 100) * 1_000_000_000
        )

    def test_symlink_is_not_touched(self) -> None:
        target = self.write("target.txt", "target")
        link = self.repo / "link.txt"
        try:
            link.symlink_to(target.name)
        except (OSError, NotImplementedError) as error:
            self.skipTest(f"symlinks unavailable: {error}")
        head = self.commit("add symlink", BASE_TIME)
        self.marker(head)
        before = link.lstat().st_mtime_ns

        result = self.invoke()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(link.lstat().st_mtime_ns, before)
        self.assertIn("skipped 1 symlink/submodule/non-regular", result.stdout)

    def test_invalid_marker_fails_before_touching_files(self) -> None:
        source = self.write("source.txt", "one")
        self.commit("initial", BASE_TIME)
        self.marker("not-a-commit")
        os.utime(source, (CHECKOUT_TIME, CHECKOUT_TIME))

        result = self.invoke()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("40-character commit SHA", result.stderr)
        self.assertEqual(source.stat().st_mtime_ns, CHECKOUT_TIME * 1_000_000_000)

    def test_shallow_repository_rejected_only_with_marker(self) -> None:
        self.write("source.txt", "one")
        self.commit("initial", BASE_TIME)
        self.write("source.txt", "two")
        head = self.commit("second", BASE_TIME + 100)

        shallow = Path(self.tempdir.name) / "shallow"
        clone = run(
            ["git", "clone", "--depth", "1", self.repo.as_uri(), str(shallow)],
            Path(self.tempdir.name),
        )
        self.assertEqual(clone.returncode, 0, clone.stderr)

        without_marker = self.invoke(shallow)
        self.assertEqual(without_marker.returncode, 0, without_marker.stderr)

        marker = shallow / "target/.open-grok-cache-commit"
        marker.parent.mkdir(parents=True)
        marker.write_text(f"{head}\n", encoding="ascii")
        with_marker = self.invoke(shallow)
        self.assertNotEqual(with_marker.returncode, 0)
        self.assertIn("shallow repository", with_marker.stderr)

    def test_non_first_parent_baseline_safely_falls_back_without_changes(self) -> None:
        source = self.write("source.txt", "base")
        self.commit("base", BASE_TIME)
        self.git("switch", "-c", "side")
        self.write("side.txt", "side")
        side = self.commit("side", BASE_TIME + 10)
        self.git("switch", "main")
        self.write("main.txt", "main")
        self.commit("main", BASE_TIME + 20)
        env = os.environ.copy()
        env["GIT_AUTHOR_DATE"] = f"@{BASE_TIME + 30} +0000"
        env["GIT_COMMITTER_DATE"] = f"@{BASE_TIME + 30} +0000"
        self.git("merge", "--no-ff", "side", "-m", "merge", env=env)
        self.marker(side)
        os.utime(source, (CHECKOUT_TIME, CHECKOUT_TIME))

        result = self.invoke()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("not on HEAD's first-parent history", result.stdout)
        self.assertEqual(source.stat().st_mtime_ns, CHECKOUT_TIME * 1_000_000_000)

    @unittest.skipUnless(shutil.which("cargo"), "cargo is unavailable")
    def test_cargo_freshness_reuses_unchanged_and_rebuilds_changed_source(self) -> None:
        self.write(
            "Cargo.toml",
            '[package]\nname = "cargo-mtime-test"\nversion = "0.1.0"\nedition = "2024"\n',
        )
        source = self.write("src/main.rs", 'fn main() { println!("one"); }\n')
        self.write("README.md", "baseline\n")
        baseline = self.commit("baseline", BASE_TIME)
        self.marker(baseline)

        first = run(["cargo", "check", "--offline", "-v"], self.repo)
        self.assertEqual(first.returncode, 0, first.stderr)

        unrelated = self.write("notes.txt", "unrelated\n")
        self.commit("unrelated source", BASE_TIME + 100)
        for path in (self.repo / "Cargo.toml", source, self.repo / "README.md", unrelated):
            os.utime(path, (CHECKOUT_TIME, CHECKOUT_TIME))

        restored = self.invoke()
        self.assertEqual(restored.returncode, 0, restored.stderr)
        fresh = run(["cargo", "check", "--offline", "-v"], self.repo)
        self.assertEqual(fresh.returncode, 0, fresh.stderr)
        self.assertIn("Fresh cargo-mtime-test", fresh.stderr)

        source.write_text('fn main() { println!("two"); }\n', encoding="utf-8")
        self.commit("change Rust source", BASE_TIME + 200)
        os.utime(source, (CHECKOUT_TIME + 10, CHECKOUT_TIME + 10))
        guarded = self.invoke()
        self.assertEqual(guarded.returncode, 0, guarded.stderr)
        self.assertEqual(
            source.stat().st_mtime_ns, (CHECKOUT_TIME + 10) * 1_000_000_000
        )
        rebuilt = run(["cargo", "check", "--offline", "-v"], self.repo)
        self.assertEqual(rebuilt.returncode, 0, rebuilt.stderr)
        self.assertIn("Dirty cargo-mtime-test", rebuilt.stderr)
        self.assertIn("Checking cargo-mtime-test", rebuilt.stderr)


if __name__ == "__main__":
    unittest.main()
