#!/usr/bin/env python3
"""The README's coverage badge, from cargo-llvm-cov's JSON summary (.github/workflows/coverage.yml).

Usage: python3 -I tools/coverage_badge.py <llvm-cov summary.json> <badge.json> [<markdown summary>]

Writes a shields.io endpoint badge (https://shields.io/badges/endpoint-badge) with the line
coverage, and appends a short table of the line, function and region coverage to the Markdown
file (the run's step summary) when one is given.
"""
import json
import sys


def colour(percent):
    for limit, name in ((90, "brightgreen"), (80, "green"), (70, "yellowgreen"), (60, "yellow"), (50, "orange")):
        if percent >= limit:
            return name
    return "red"


def main(argv):
    if len(argv) not in (3, 4):
        sys.exit(__doc__.strip().splitlines()[2])
    with open(argv[1], encoding="utf-8") as f:
        totals = json.load(f)["data"][0]["totals"]
    lines = float(totals["lines"]["percent"])
    badge = {"schemaVersion": 1, "label": "coverage", "message": f"{lines:.1f}%", "color": colour(lines)}
    with open(argv[2], "w", encoding="utf-8") as f:
        json.dump(badge, f)
        f.write("\n")
    if len(argv) == 4:
        with open(argv[3], "a", encoding="utf-8") as f:
            f.write("### Test coverage of the crates\n\n| | Covered | Total | % |\n|---|---:|---:|---:|\n")
            for name, key in (("Lines", "lines"), ("Functions", "functions"), ("Regions", "regions")):
                t = totals[key]
                f.write(f"| {name} | {t['covered']} | {t['count']} | {float(t['percent']):.1f} |\n")
    print(f"line coverage {lines:.1f}%")


if __name__ == "__main__":
    main(sys.argv)
