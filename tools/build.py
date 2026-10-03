#!/usr/bin/env python3
"""Builds the translated API reference of this branch from the English OpenAPI description.

    en/openapi.yaml       the English description, the source of everything else
    i18n/ui.json          per language: its name, direction and the strings of the page itself
                          (Redoc's `labels`, and its other strings that assets/site.js replaces)
    i18n/<lang>.json      per language: {JSON pointer: {"en": English text, "text": translation}}
                          for every title, summary, description and x-displayName of the English
                          description (the prose; examples, defaults and enums are data and stay
                          as they are)

    tools/build.py            writes <lang>/openapi.yaml and <lang>/index.html for every language
                              of i18n/ui.json, and the language picker (index.html)
    tools/build.py --check    writes nothing; fails when a generated file is not up to date or a
                              translation is missing, stale (its "en" no longer matches the English
                              description) or damaged (different code spans, links or Markdown
                              structure than the English)
    tools/build.py --todo L   writes i18n/<L>.todo.json: the strings of language L that are missing
                              or stale, {pointer: English text}, to translate; put the
                              translations back with --merge
    tools/build.py --merge L FILE
                              merges FILE ({pointer: translation}, the answer to a todo file) into
                              i18n/<L>.json, recording the English text each one translates

Requires Python 3.9+ and ruamel.yaml.
"""

import html
import json
import re
import sys
from collections import Counter
from io import StringIO
from pathlib import Path

from ruamel.yaml import YAML
from ruamel.yaml.scalarstring import FoldedScalarString, LiteralScalarString

ROOT = Path(__file__).resolve().parent.parent
PROSE = {"title", "summary", "description", "x-displayName"}
DATA = {"example", "default", "enum", "const", "x-example"}
REDOC = "assets/redoc/redoc.standalone.js"


def yaml_rt():
    y = YAML()
    y.preserve_quotes = True
    y.width = 4096
    y.indent(mapping=2, sequence=4, offset=2)
    y.representer.add_representer(
        type(None), lambda r, _: r.represent_scalar("tag:yaml.org,2002:null", "null")
    )
    return y


def esc(key):
    return str(key).replace("~", "~0").replace("/", "~1")


def prose(node, path="", out=None, in_examples=False):
    """{pointer: text} of the prose strings of an OpenAPI document."""
    out = {} if out is None else out
    if isinstance(node, dict):
        for k, v in node.items():
            p = f"{path}/{esc(k)}"
            if k in DATA or (in_examples and k == "value"):
                continue
            if k in PROSE and isinstance(v, str) and not path.endswith("/properties"):
                out[p] = str(v)
                continue
            prose(v, p, out, k == "examples" or (in_examples and path.endswith("/examples")))
    elif isinstance(node, list):
        for i, v in enumerate(node):
            prose(v, f"{path}/{i}", out, in_examples)
    return out


def resolve(doc, pointer):
    parts = [p.replace("~1", "/").replace("~0", "~") for p in pointer.split("/")[1:]]
    parent = doc
    for p in parts[:-1]:
        parent = parent[int(p)] if isinstance(parent, list) else parent[p]
    return parent, parts[-1]


# The parts of a Markdown string a translation must keep as they are.
def features(s):
    body = re.sub(r"```.*?```", "", s, flags=re.S)
    lines = body.split("\n")
    return {
        "fenced code blocks": Counter(re.findall(r"```.*?```", s, re.S)),
        "code spans": Counter(re.findall(r"`[^`\n]+`", body)),
        "URLs": Counter(re.findall(r"https?://[^\s)>\]`]+", body)),
        "link targets": Counter(re.findall(r"\]\(([^)]+)\)", body)),
        "headings": [len(m.group(1)) for line in lines for m in [re.match(r"^(#+) ", line)] if m],
        "list items": sum(1 for line in lines if re.match(r"^\s*([-*+]|\d+\.) ", line)),
        "table rows": sum(1 for line in lines if line.lstrip().startswith("|")),
        "bold markers": body.count("**"),
        "paragraph breaks": body.count("\n\n"),
    }


def damage(en, text):
    fe, ft = features(en), features(text)
    return [name for name in fe if fe[name] != ft[name]]


def load_json(path, default=None):
    if not path.exists():
        return default
    return json.loads(path.read_text(encoding="utf-8"))


def dump_json(data):
    return json.dumps(data, ensure_ascii=False, indent=1) + "\n"


def translated_yaml(en_text, strings, translations):
    doc = yaml_rt().load(en_text)
    for pointer, en in strings.items():
        parent, key = resolve(doc, pointer)
        text = translations[pointer]["text"]
        original = parent[int(key) if isinstance(parent, list) else key]
        if "\n" in text or isinstance(original, LiteralScalarString):
            text = LiteralScalarString(text)
        elif isinstance(original, FoldedScalarString):
            text = FoldedScalarString(text)
        parent[int(key) if isinstance(parent, list) else key] = text
    out = StringIO()
    yaml_rt().dump(doc, out)
    return out.getvalue()


PAGE = """<!doctype html>
<html lang="{lang}" dir="{dir}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'self'; worker-src 'self' blob:; base-uri 'none'; form-action 'none'">
<meta name="referrer" content="no-referrer">
<title>{title}</title>
<meta name="description" content="{summary}">
<link rel="alternate" type="application/yaml" href="openapi.yaml">
{alternates}
<link rel="stylesheet" href="../assets/site.css">
</head>
<body>
<header class="site-bar">
  <a class="site-home" href="../">Scacelith</a>
  <span class="site-title">{title}</span>
  <label class="site-lang"><span>{language}</span>
    <select id="site-lang">
{options}
    </select>
  </label>
</header>
<main id="redoc"></main>
<script type="application/json" id="site-config">{config}</script>
<script src="../{redoc}"></script>
<script src="../assets/site.js"></script>
</body>
</html>
"""

PICKER = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'self'; style-src 'self'; base-uri 'none'; form-action 'none'">
<meta name="referrer" content="no-referrer">
<title>Scacelith server API reference</title>
<meta name="description" content="The HTTPS API of the Scacelith chess server, in ten languages.">
<link rel="stylesheet" href="assets/site.css">
</head>
<body class="picker">
<main>
  <h1>Scacelith server API reference</h1>
  <p>The HTTPS API of every Scacelith server (OpenAPI {openapi}, API version {version}). Pick a
  language; each page also links to its OpenAPI description.</p>
  <ul class="languages">
{items}
  </ul>
  <p class="source">Source: <a href="https://github.com/DarkCenobyte/scacelith-chess-server">DarkCenobyte/scacelith-chess-server</a>
  (the server, its <code>docs/API.md</code> and the realtime protocol). Rendered with
  <a href="https://github.com/Redocly/redoc">Redoc</a> (MIT licence, <a href="assets/redoc/LICENSE">LICENSE</a>).</p>
</main>
<script src="assets/picker.js"></script>
</body>
</html>
"""


def pages(ui, specs):
    langs = list(ui)
    files = {}
    for lang in langs:
        u, spec = ui[lang], specs[lang]
        info = spec["info"]
        options = "\n".join(
            f'      <option value="{code}"{" selected" if code == lang else ""} lang="{code}">'
            f"{html.escape(ui[code]['name'])}</option>"
            for code in langs
        )
        alternates = "\n".join(
            f'<link rel="alternate" hreflang="{code}" href="../{code}/">' for code in langs
        )
        config = {
            "lang": lang,
            "redoc": {
                "labels": u["labels"],
                "scrollYOffset": ".site-bar",
                "expandResponses": "200,201,202,204",
                "requiredPropsFirst": True,
                "pathInMiddlePanel": True,
                "jsonSamplesExpandLevel": 3,
                "hideHostname": False,
                "showExtensions": False,
                "downloadFileName": f"scacelith-server-api-{lang}.yaml",
                "theme": None,
            },
            "text": u["text"],
            "words": u["words"],
        }
        # The JSON sits in a script element: "</" must not end it.
        config_json = json.dumps(config, ensure_ascii=False).replace("</", "<\\/")
        files[f"{lang}/index.html"] = PAGE.format(
            lang=lang,
            dir=u["dir"],
            title=html.escape(info["title"]),
            summary=html.escape(info.get("summary", "")),
            language=html.escape(u["language"]),
            options=options,
            alternates=alternates,
            config=config_json,
            redoc=REDOC,
        )
    en = specs["en"]
    items = "\n".join(
        f'    <li lang="{code}" dir="{ui[code]["dir"]}"><a href="{code}/">'
        f'<span class="name">{html.escape(ui[code]["name"])}</span>'
        f'<span class="title">{html.escape(specs[code]["info"]["title"])}</span></a></li>'
        for code in langs
    )
    files["index.html"] = PICKER.format(
        items=items, openapi=en["openapi"], version=html.escape(str(en["info"]["version"]))
    )
    return files


def main(argv):
    mode = argv[1] if len(argv) > 1 else "--build"
    ui = load_json(ROOT / "i18n" / "ui.json")
    en_text = (ROOT / "en" / "openapi.yaml").read_text(encoding="utf-8")
    en_doc = YAML(typ="safe").load(en_text)
    strings = prose(en_doc)

    if mode == "--todo":
        lang = argv[2]
        have = load_json(ROOT / "i18n" / f"{lang}.json", {})
        todo = {p: en for p, en in strings.items() if p not in have or have[p]["en"] != en}
        path = ROOT / "i18n" / f"{lang}.todo.json"
        path.write_text(dump_json(todo), encoding="utf-8")
        print(f"{path.relative_to(ROOT)}: {len(todo)} string(s) to translate")
        return 0

    if mode == "--merge":
        lang, answer = argv[2], load_json(Path(argv[3]))
        path = ROOT / "i18n" / f"{lang}.json"
        have = load_json(path, {})
        problems = 0
        for p, text in answer.items():
            if p not in strings:
                print(f"{p}: not a string of en/openapi.yaml")
                problems += 1
                continue
            bad = damage(strings[p], text)
            if bad:
                print(f"{p}: {', '.join(bad)} differ from the English")
                problems += 1
                continue
            have[p] = {"en": strings[p], "text": text}
        ordered = {p: have[p] for p in strings if p in have}
        path.write_text(dump_json(ordered), encoding="utf-8")
        print(f"{path.relative_to(ROOT)}: merged {len(answer) - problems}, refused {problems}")
        return 1 if problems else 0

    errors = []
    specs = {"en": en_doc}
    outputs = {}
    for lang in ui:
        if lang == "en":
            continue
        translations = load_json(ROOT / "i18n" / f"{lang}.json", {})
        missing = [p for p in strings if p not in translations]
        stale = [p for p in strings if p in translations and translations[p]["en"] != strings[p]]
        extra = [p for p in translations if p not in strings]
        damaged = [
            f"{p} ({', '.join(bad)})"
            for p in strings
            if p in translations
            for bad in [damage(strings[p], translations[p]["text"])]
            if bad
        ]
        for kind, items in (("missing", missing), ("stale", stale), ("unused", extra), ("damaged", damaged)):
            if items:
                errors.append(f"i18n/{lang}.json: {len(items)} {kind}: {', '.join(items[:5])}")
        if missing or stale or damaged:
            continue
        text = translated_yaml(en_text, strings, translations)
        doc = YAML(typ="safe").load(text)
        # Everything but the prose is the English description's.
        stripped_en, stripped = json.loads(json.dumps(en_doc)), json.loads(json.dumps(doc))
        for p in strings:
            for d in (stripped_en, stripped):
                parent, key = resolve(d, p)
                parent[int(key) if isinstance(parent, list) else key] = None
        if stripped != stripped_en:
            errors.append(f"{lang}/openapi.yaml: differs from the English beyond the prose")
        outputs[f"{lang}/openapi.yaml"] = text
        specs[lang] = doc
    if errors:
        print("\n".join(errors))
        return 1
    outputs.update(pages(ui, specs))

    stale_files = []
    for rel, content in outputs.items():
        path = ROOT / rel
        if path.exists() and path.read_text(encoding="utf-8") == content:
            continue
        if mode == "--check":
            stale_files.append(rel)
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
            print(f"wrote {rel}")
    if stale_files:
        print("not up to date (run tools/build.py): " + ", ".join(stale_files))
        return 1
    if mode == "--check":
        print(f"{len(outputs)} files up to date, {len(ui)} languages, {len(strings)} strings each")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
