#!/usr/bin/env python3
"""Restore safe source mtimes after a Cargo target-cache restore.

Cargo's default freshness checks use source mtimes. GitHub Actions checks out
every source file with a new mtime, so a restored ``target`` directory keeps
third-party dependencies warm but still causes all workspace crates to rebuild.

This helper only backdates files whose contents are unchanged from the commit
recorded in the restored cache marker. Files changed since that baseline retain
their checkout mtimes, ensuring Cargo cannot mistake an older cached artifact
for a fresh one. The workflow must write the successfully built commit to the
marker before saving the cache.
"""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
from typing import Sequence


DEFAULT_MARKER = "target/.open-grok-cache-commit"
COMMIT_RE = re.compile(r"[0-9a-fA-F]{40}")
LOG_HEADER_RE = re.compile(rb"OGMT:([0-9a-f]{40}):([0-9]+)")


class RestoreError(RuntimeError):
    """An expected repository or cache invariant was not satisfied."""


def git(repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[bytes]:
    result = subprocess.run(
        ["git", *args],
        cwd=repo,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if check and result.returncode != 0:
        detail = result.stderr.decode("utf-8", "replace").strip()
        raise RestoreError(f"git {' '.join(args)} failed: {detail or result.returncode}")
    return result


def decode_path(raw: bytes) -> str:
    # Git emits unquoted UTF-8 path bytes with -z, including on Git for Windows.
    return raw.decode("utf-8", "surrogateescape")


def nul_paths(raw: bytes) -> set[str]:
    return {decode_path(item) for item in raw.split(b"\0") if item}


def read_marker(marker: Path) -> str | None:
    try:
        value = marker.read_text(encoding="ascii").strip()
    except FileNotFoundError:
        return None
    except (OSError, UnicodeError) as error:
        raise RestoreError(f"cannot read cache marker {marker}: {error}") from error
    if not COMMIT_RE.fullmatch(value):
        raise RestoreError(
            f"cache marker {marker} must contain exactly one 40-character commit SHA"
        )
    return value.lower()


def parse_history(raw: bytes) -> tuple[dict[str, int], set[str]]:
    """Return each path's newest first-parent timestamp and traversed commits.

    ``git log --name-only -z`` inserts one formatting newline before the first
    path of each commit. Removing exactly one newline preserves a real leading
    newline in a filename. A header is recognized only after an empty NUL token,
    so a path resembling the header marker cannot be confused with a commit.
    """

    mtimes: dict[str, int] = {}
    commits: set[str] = set()
    current_timestamp: int | None = None
    first_path = False
    previous_was_empty = False

    for token in raw.split(b"\0"):
        if not token:
            previous_was_empty = True
            continue

        header = LOG_HEADER_RE.fullmatch(token) if previous_was_empty else None
        if header is not None:
            commit = header.group(1).decode("ascii")
            commits.add(commit)
            current_timestamp = int(header.group(2))
            first_path = True
            previous_was_empty = False
            continue

        if current_timestamp is None:
            raise RestoreError("malformed git log output: path appeared before commit header")
        if first_path:
            if not token.startswith(b"\n"):
                raise RestoreError("malformed git log output: first path lacks separator newline")
            token = token[1:]
            first_path = False
        if not token:
            raise RestoreError("malformed git log output: empty tracked path")
        mtimes.setdefault(decode_path(token), current_timestamp)
        previous_was_empty = False

    if not commits:
        raise RestoreError("git history traversal returned no commits")
    return mtimes, commits


def restore(repo_arg: Path, marker_arg: Path) -> int:
    try:
        repo = Path(
            git(repo_arg.resolve(), "rev-parse", "--show-toplevel").stdout.decode(
                "utf-8", "surrogateescape"
            ).strip()
        ).resolve()
    except RestoreError as error:
        raise RestoreError(f"not inside a Git worktree: {error}") from error

    marker = marker_arg if marker_arg.is_absolute() else repo / marker_arg
    baseline = read_marker(marker)
    if baseline is None:
        print(
            f"Cargo cache marker {marker} is absent; leaving checkout mtimes unchanged."
        )
        return 0

    shallow = git(repo, "rev-parse", "--is-shallow-repository").stdout.strip()
    if shallow != b"false":
        raise RestoreError(
            "cannot safely restore Cargo mtimes from a shallow repository; "
            "checkout with fetch-depth: 0"
        )

    git(repo, "cat-file", "-e", f"{baseline}^{{commit}}")
    ancestor = git(repo, "merge-base", "--is-ancestor", baseline, "HEAD", check=False)
    if ancestor.returncode == 1:
        print(
            f"Cargo cache baseline {baseline} is not an ancestor of HEAD; "
            "leaving checkout mtimes unchanged."
        )
        return 0
    if ancestor.returncode != 0:
        detail = ancestor.stderr.decode("utf-8", "replace").strip()
        raise RestoreError(f"cannot validate cache baseline ancestry: {detail}")

    dirty_before = git(
        repo, "status", "--porcelain=v1", "-z", "--untracked-files=no"
    ).stdout
    if dirty_before:
        raise RestoreError("tracked worktree changes are present; refusing to change mtimes")

    tracked = nul_paths(git(repo, "ls-files", "--cached", "-z").stdout)
    changed = nul_paths(
        git(
            repo,
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            baseline,
            "HEAD",
            "--",
        ).stdout
    )

    history = git(
        repo,
        "log",
        "--first-parent",
        "-m",
        "--root",
        "--no-renames",
        "--name-only",
        "-z",
        "--format=%x00OGMT:%H:%ct%x00",
        "HEAD",
        "--",
    ).stdout
    mtimes, commits = parse_history(history)
    if baseline not in commits:
        print(
            f"Cargo cache baseline {baseline} is not on HEAD's first-parent history; "
            "leaving checkout mtimes unchanged."
        )
        return 0

    skipped_changed = 0
    skipped_non_regular = 0
    missing_history: list[str] = []
    candidates: list[tuple[Path, str, int]] = []

    for relative in sorted(tracked):
        path = repo / relative
        if relative in changed:
            skipped_changed += 1
            continue
        if path.is_symlink() or not path.is_file():
            skipped_non_regular += 1
            continue
        timestamp = mtimes.get(relative)
        if timestamp is None:
            missing_history.append(relative)
            continue
        candidates.append((path, relative, timestamp))

    if missing_history:
        preview = ", ".join(repr(path) for path in missing_history[:5])
        suffix = " ..." if len(missing_history) > 5 else ""
        raise RestoreError(
            f"no first-parent history timestamp for {len(missing_history)} tracked files: "
            f"{preview}{suffix}"
        )

    restored = 0
    for path, relative, timestamp in candidates:
        nanoseconds = timestamp * 1_000_000_000
        try:
            # Symlinks were excluded above. Omitting follow_symlinks keeps this
            # call supported by the Python/Windows combinations used by CI.
            os.utime(path, ns=(nanoseconds, nanoseconds))
        except OSError as error:
            raise RestoreError(f"cannot set mtime for {relative!r}: {error}") from error
        restored += 1

    dirty_after = git(
        repo, "status", "--porcelain=v1", "-z", "--untracked-files=no"
    ).stdout
    if dirty_after != dirty_before:
        raise RestoreError("restoring mtimes unexpectedly changed tracked worktree status")

    print(
        f"Restored deterministic mtimes for {restored} unchanged tracked files; "
        f"left {skipped_changed} changed/new files fresh and skipped "
        f"{skipped_non_regular} symlink/submodule/non-regular paths."
    )
    return 0


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo",
        type=Path,
        default=Path.cwd(),
        help="Git worktree to normalize (default: current directory)",
    )
    parser.add_argument(
        "--cache-marker",
        type=Path,
        default=Path(DEFAULT_MARKER),
        help=f"cached baseline commit marker (default: {DEFAULT_MARKER})",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        return restore(args.repo, args.cache_marker)
    except RestoreError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
