#!/usr/bin/env python3
"""Process-isolated entity-linking worker for Wild AgentOS.

SPDX-License-Identifier: AGPL-3.0-only

The wire contract follows GLinker's Apache-2.0 mention/retrieve/disambiguate
pattern. This minimal worker deliberately uses only frozen exact-normalized
label matching; deployments may install GLinker in this separate environment
without linking its Python stack, models, or any LGPL component into WAO.
"""

import json
import sys


def normalize(value: str) -> str:
    return "".join(char.lower() for char in value if char.isalnum())


def main() -> int:
    try:
        request = json.load(sys.stdin)
        mention = normalize(request["mention"])
        minimum = float(request["min_score"])
        candidates = request["candidates"]
    except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        print(f"invalid entity-resolution request: {error}", file=sys.stderr)
        return 2

    matches = sorted(
        candidate for candidate in candidates
        if normalize(str(candidate.get("label", ""))) == mention
    )
    if not matches or 1.0 < minimum:
        print(json.dumps({"target_iri": "", "score": 0.0, "evidence": []}))
        return 0
    target = matches[0]
    print(json.dumps({
        "target_iri": target["iri"],
        "score": 1.0,
        "evidence": [
            "matcher:exact-normalized-label-v1",
            f"candidate_label:{target['label']}",
        ],
    }))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
