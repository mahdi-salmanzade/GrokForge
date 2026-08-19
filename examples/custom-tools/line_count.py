#!/usr/bin/env python3
"""Minimal GrokForge custom-tool v1 example using only Python's standard library."""

import json
import pathlib
import sys


def respond(*, content: str | None = None, error: str | None = None) -> None:
    response = {"protocol_version": 1, "ok": error is None}
    response["content" if error is None else "error"] = content if error is None else error
    json.dump(response, sys.stdout, separators=(",", ":"))


def main() -> None:
    request = json.load(sys.stdin)
    if request.get("protocol_version") != 1:
        respond(error="unsupported protocol version")
        return
    arguments = request.get("arguments")
    if not isinstance(arguments, dict) or not isinstance(arguments.get("path"), str):
        respond(error="`path` must be a string")
        return

    root = pathlib.Path(request["workspace_root"]).resolve(strict=True)
    target = (root / arguments["path"]).resolve(strict=True)
    try:
        target.relative_to(root)
    except ValueError:
        respond(error="path escapes the workspace")
        return
    if not target.is_file():
        respond(error="path is not a regular file")
        return

    try:
        lines = len(target.read_text(encoding="utf-8").splitlines())
    except (OSError, UnicodeError) as error:
        respond(error=f"cannot read UTF-8 file: {error}")
        return
    respond(content=f"{lines} lines")


if __name__ == "__main__":
    main()
