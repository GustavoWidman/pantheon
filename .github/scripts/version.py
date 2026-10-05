#!/usr/bin/env python3
"""Conventional commit and stable SemVer gates; Cargo.toml owns the version."""
import argparse
import os
from pathlib import Path
import re
import subprocess
import tomllib

SEMVER = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
CONVENTIONAL = re.compile(r"(?:feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert)(?:\([a-zA-Z0-9_./-]+\))?!?: \S.*")


def semver(value):
    match = SEMVER.fullmatch(value)
    if not match:
        raise ValueError(f"version must be stable major.minor.patch: {value}")
    return tuple(map(int, match.groups()))


def conventional(subject):
    if not CONVENTIONAL.fullmatch(subject):
        raise ValueError(f"commit/title must follow Conventional Commits: {subject}")


def git(*arguments):
    return subprocess.check_output(["git", *arguments], text=True).strip()


def current():
    value = tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]
    semver(value)
    lock = tomllib.loads(Path("Cargo.lock").read_text())
    versions = [entry["version"] for entry in lock["package"] if entry["name"] == "pantheon"]
    if versions != [value]:
        raise ValueError("Cargo.lock must agree with Cargo.toml; run cargo check")
    return value


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=["current", "check", "commits"])
    parser.add_argument("--base")
    parser.add_argument("--head", default="HEAD")
    args = parser.parse_args()
    version = current()
    if args.action == "current":
        print(version)
    elif args.action == "check":
        if "no-release" in os.environ.get("LABELS", "").split(","):
            print("no-release: version bump exempted")
            return
        if not args.base:
            parser.error("check requires --base")
        previous = tomllib.loads(git("show", f"{args.base}:Cargo.toml"))["package"]["version"]
        if semver(version) <= semver(previous):
            raise ValueError(f"version must increase from {previous}; got {version}")
        print(f"version: {previous} -> {version}")
    else:
        title = os.environ.get("PR_TITLE")
        if title:
            conventional(title)
        if args.base and set(args.base) != {"0"}:
            subjects = git("log", "--no-merges", "--format=%s", f"{args.base}..{args.head}")
        else:
            subjects = git("log", "-1", "--format=%s", args.head)
        for subject in subjects.splitlines():
            conventional(subject)
        print("conventional commits: valid")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"::error::{error}") from error
