#!/usr/bin/env python3
"""Small Morph-KGC sidecar adapter for Wild AgentOS.

SPDX-License-Identifier: AGPL-3.0-only

This script is an executable process boundary, not a Rust dependency. It uses
Morph-KGC (Apache-2.0) when installed in the worker image/environment.
"""

import argparse
import os
import sys
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Materialize an RML mapping into N-Triples using Morph-KGC."
    )
    parser.add_argument("--source-dir", required=True, type=Path)
    parser.add_argument("--mapping", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    source_dir = args.source_dir.resolve()
    mapping = args.mapping.resolve()
    output = args.output.resolve()
    if not source_dir.is_dir() or not mapping.is_file():
        print("source directory or mapping file does not exist", file=sys.stderr)
        return 2

    try:
        import morph_kgc
    except ImportError:
        print(
            "Morph-KGC is not installed; install the Apache-2.0 package in the worker environment",
            file=sys.stderr,
        )
        return 3

    # Relative rml:source paths resolve only inside the directory populated by
    # WAO for this invocation. The Rust API validates uploaded basenames.
    os.chdir(source_dir)
    config = f"""[CONFIGURATION]
output_file={output}
output_format=N-TRIPLES

[DataSource1]
mappings={mapping}
"""
    try:
        graph = morph_kgc.materialize(config)
        graph.serialize(destination=str(output), format="nt")
    except Exception as error:  # worker errors are surfaced as HTTP 502
        print(f"Morph-KGC materialization failed: {error}", file=sys.stderr)
        return 4
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
