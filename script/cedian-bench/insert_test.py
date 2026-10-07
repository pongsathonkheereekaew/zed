#!/usr/bin/env python3
"""Insert the test fn in SNIPPET into FILE's trailing `mod tests` (before its last `}`)."""
import sys

path, snippet = sys.argv[1], sys.argv[2]
src = open(path).read().rstrip()
if not src.endswith("}") or "#[cfg(test)]" not in src:
    print(f"insert_test: {path} has no trailing test module", file=sys.stderr)
    sys.exit(1)
open(path, "w").write(src[:-1].rstrip() + "\n\n" + open(snippet).read().rstrip() + "\n}\n")
