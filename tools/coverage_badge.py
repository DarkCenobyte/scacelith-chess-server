#!/usr/bin/env python3
"""The README's coverage badge, from cargo-llvm-cov's JSON summary (.github/workflows/coverage.yml).

Usage: python3 -I tools/coverage_badge.py <llvm-cov summary.json> <badge.json> [<markdown summary>]

Writes a shields.io endpoint badge (https://shields.io/badges/endpoint-badge) with the line
coverage and the same badge as an image next to it (<badge>.svg, the one the README shows), and
appends a short table of the line, function and region coverage to the Markdown file (the run's step
summary) when one is given.
"""
import json
import os
import sys

# Advance widths of the badge's characters in 11 px Verdana, and the hex values of the colour names,
# as shields.io's badge-maker has them: the SVG below is the "flat" badge shields.io draws, so the
# README shows it straight from the badges branch. GitHub loads outside images through its image
# proxy, which gives up after about four seconds, and shields.io often answers later than that.
WIDTHS = {"c": 5.73, "o": 6.68, "v": 6.51, "e": 6.55, "r": 4.69, "a": 6.61, "g": 6.85, ".": 4, "%": 11.84,
          **dict.fromkeys("0123456789", 6.99)}
HEX = {"brightgreen": "#4b0", "green": "#67ac09", "yellowgreen": "#95991a", "yellow": "#d8b800",
       "orange": "#ea7233", "red": "#dd4343"}


def colour(percent):
    for limit, name in ((90, "brightgreen"), (80, "green"), (70, "yellowgreen"), (60, "yellow"), (50, "orange")):
        if percent >= limit:
            return name
    return "red"


def text_width(text):
    width = int(sum(WIDTHS[c] for c in text))
    return width if width % 2 else width + 1


def svg(label, message, colour_name):
    """shields.io writes white text on all the colours of HEX."""
    label_width, message_width = text_width(label), text_width(message)
    left, right = label_width + 10, message_width + 10
    width = left + right

    def text(x, length, value):
        return (f'<g transform="scale(.1)"><g aria-hidden="true" fill="#010101">'
                f'<text x="{x}" y="150" fill-opacity=".8" filter="url(#blur)" textLength="{length}">{value}</text>'
                f'<text x="{x}" y="150" fill-opacity=".3" textLength="{length}">{value}</text></g>'
                f'<text x="{x}" y="140" textLength="{length}">{value}</text></g>')

    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="20" role="img" '
            f'aria-label="{label}: {message}"><title>{label}: {message}</title>'
            f'<filter id="blur"><feGaussianBlur stdDeviation="16"/></filter>'
            f'<linearGradient id="s" x2="0" y2="100%"><stop offset="0" stop-color="#bbb" stop-opacity=".1"/>'
            f'<stop offset="1" stop-opacity=".1"/></linearGradient>'
            f'<clipPath id="r"><rect width="{width}" height="20" rx="3"/></clipPath>'
            f'<g clip-path="url(#r)"><rect width="{left}" height="20" fill="#555"/>'
            f'<rect x="{left}" width="{right}" height="20" fill="{HEX[colour_name]}"/>'
            f'<rect width="{width}" height="20" fill="url(#s)"/></g>'
            f'<g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" '
            f'text-rendering="geometricPrecision" font-size="110">'
            f'{text(60 + 5 * label_width, 10 * label_width, label)}'
            f'{text(10 * left + 40 + 5 * message_width, 10 * message_width, message)}</g></svg>')


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
    with open(os.path.splitext(argv[2])[0] + ".svg", "w", encoding="utf-8", newline="\n") as f:
        f.write(svg(badge["label"], badge["message"], badge["color"]) + "\n")
    if len(argv) == 4:
        with open(argv[3], "a", encoding="utf-8") as f:
            f.write("### Test coverage of the crates\n\n| | Covered | Total | % |\n|---|---:|---:|---:|\n")
            for name, key in (("Lines", "lines"), ("Functions", "functions"), ("Regions", "regions")):
                t = totals[key]
                f.write(f"| {name} | {t['covered']} | {t['count']} | {float(t['percent']):.1f} |\n")
    print(f"line coverage {lines:.1f}%")


if __name__ == "__main__":
    main(sys.argv)
