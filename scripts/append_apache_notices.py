#!/usr/bin/env python3
"""Append root NOTICE files from third-party crates onto cargo-about output.

cargo-about does not forward Apache-2.0 NOTICE files. This walks the crate
sources already downloaded for the workspace graph and appends each root
NOTICE, in a stable order, without recording local paths.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

NOTICE_NAMES = {"notice", "notice.txt", "notice.md"}
OWN_CRATES = {
    "wild-agent-os-core",
    "hyperspace-engine",
    "ontologies",
    "wild-code-cli",
}


def metadata() -> dict:
    completed = subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1"],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    return json.loads(completed.stdout)


def notice_files(crate_root: Path) -> list[Path]:
    found: list[Path] = []
    if not crate_root.is_dir():
        return found
    for child in crate_root.iterdir():
        if child.is_file() and child.name.casefold() in NOTICE_NAMES:
            found.append(child)
    found.sort(key=lambda path: path.name.casefold())
    return found


def included_ids(graph_path: Path) -> set[tuple[str, str]]:
    graph = json.loads(graph_path.read_text(encoding="utf-8"))
    selected: set[tuple[str, str]] = set()
    for item in graph["crates"]:
        package = item["package"]
        selected.add((package["name"], str(package["version"])))
    return selected


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: append_apache_notices.py OUTPUT ABOUT_JSON", file=sys.stderr)
        return 2
    output = Path(sys.argv[1])
    selected = included_ids(Path(sys.argv[2]))
    packages = metadata()["packages"]
    sections: list[str] = []
    for package in sorted(packages, key=lambda item: (item["name"], item["version"])):
        name = package["name"]
        version = package["version"]
        if name in OWN_CRATES or (name, version) not in selected:
            continue
        source = package.get("source") or ""
        if not source.startswith("registry+"):
            continue
        manifest = Path(package["manifest_path"])
        files = notice_files(manifest.parent)
        if not files:
            continue
        for notice in files:
            text = notice.read_text(encoding="utf-8", errors="replace").replace("\r\n", "\n").strip()
            if not text:
                continue
            sections.append(
                f"### {name} {version} ({notice.name})\n\n~~~~\n{text}\n~~~~\n"
            )

    body = output.read_text(encoding="utf-8")
    if not body.endswith("\n"):
        body += "\n"
    body += "\n## Apache-2.0 NOTICE files\n\n"
    if sections:
        body += (
            "The following text is reproduced from NOTICE files shipped by "
            "third-party crates, as required by Apache-2.0 section 4(d).\n\n"
        )
        body += "\n".join(sections)
        if not body.endswith("\n"):
            body += "\n"
    else:
        body += "No third-party crate in this graph ships a root NOTICE file.\n"
    output.write_text(body, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
