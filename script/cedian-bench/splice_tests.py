#!/usr/bin/env python3
"""Replace the `#[cfg(test)]` tail of FILE with the one in REF's version of it.

The benchmark's hidden tests are the reference commit's inline Rust test
module. They are applied to the agent's result after the turn, so the
agent never sees them. A file without a test module gets the module
appended; a missing file fails the predicate (exit 1).
"""
import subprocess
import sys

MARK = "#[cfg(test)]"


def main(path: str, ref: str) -> int:
    try:
        mine = open(path).read()
    except FileNotFoundError:
        print(f"splice: {path} does not exist in the result", file=sys.stderr)
        return 1
    theirs = subprocess.run(
        ["git", "show", f"{ref}:{path}"], check=True, capture_output=True, text=True
    ).stdout
    if MARK not in theirs:
        print(f"splice: {ref}:{path} has no test module", file=sys.stderr)
        return 2
    head = mine.split(MARK, 1)[0].rstrip("\n") + "\n\n"
    open(path, "w").write(head + MARK + theirs.split(MARK, 1)[1])
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1], sys.argv[2]))
