#!/usr/bin/env python3
"""Saves a shields.io badge into docs/badges/ for the README.

Usage: python3 -I tools/readme_badge.py <https://img.shields.io/... URL> <docs/badges/name.svg>

The README shows its fixed badges from these files rather than from img.shields.io: GitHub loads
outside images through its image proxy, which gives up after about four seconds, and shields.io
often answers later than that, which leaves broken images on the page. The logo, which shields.io
embeds as a data: URI, is written as a nested <svg> element so that the file loads nothing else.
"""
import base64
import re
import sys
import urllib.request

LOGO = re.compile(r'<image x="([\d.]+)" y="([\d.]+)" width="([\d.]+)" height="([\d.]+)" '
                  r'href="data:image/svg\+xml;base64,([A-Za-z0-9+/=]+)"/>')


def nested_logo(match):
    x, y, width, height, data = match.groups()
    logo = re.fullmatch(r'<svg ([^>]*)>(.*)</svg>', base64.b64decode(data).decode("utf-8"), re.S)
    attributes = dict(re.findall(r'([\w:-]+)="([^"]*)"', logo.group(1)))
    body = re.sub(r"<title>.*?</title>", "", logo.group(2), flags=re.S)
    return (f'<svg x="{x}" y="{y}" width="{width}" height="{height}" '
            f'viewBox="{attributes["viewBox"]}" fill="{attributes["fill"]}">{body}</svg>')


def self_contained(svg):
    svg = LOGO.sub(nested_logo, svg)
    if "data:" in svg or "<image" in svg:
        sys.exit("the badge still refers to an image this tool does not know how to inline")
    return svg


def main(argv):
    if len(argv) != 3:
        sys.exit(__doc__.strip().splitlines()[2])
    url, path = argv[1], argv[2]
    if not url.startswith("https://img.shields.io/"):
        sys.exit("expected an https://img.shields.io/ URL")
    request = urllib.request.Request(url, headers={"User-Agent": "scacelith-readme-badge"})
    with urllib.request.urlopen(request, timeout=30) as response:
        svg = response.read().decode("utf-8")
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(self_contained(svg.strip()) + "\n")
    print(path)


if __name__ == "__main__":
    main(sys.argv)
