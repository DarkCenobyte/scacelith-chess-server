# Scacelith server API reference (gh-pages)

This branch is the website of the HTTPS API reference of the Scacelith server, in the ten languages
of the game, published by GitHub Pages at
<https://darkcenobyte.github.io/scacelith-chess-server/>. The server itself, its documentation
(`docs/API.md` is the reference this site describes) and the realtime protocol are on the
[master branch](https://github.com/DarkCenobyte/scacelith-chess-server/tree/master).

| Path | Content |
| --- | --- |
| `index.html` | The language picker (the visitor's language first) |
| `en/openapi.yaml` | The OpenAPI 3.1 description, in English: the source of every language |
| `<lang>/openapi.yaml` | The same description translated (`fr`, `de`, `es`, `uk`, `ru`, `ar`, `ja`, `zh-Hans`, `zh-Hant`), generated |
| `<lang>/index.html` | The reference page of a language (Redoc), generated |
| `i18n/<lang>.json` | The translations: for each title, summary, description and tag name of the English description (by JSON pointer), the English text it translates and its translation |
| `i18n/ui.json` | Per language: its name, direction, and the strings of the page itself (Redoc's labels and the few strings `assets/site.js` replaces) |
| `assets/` | The page's script and style, and Redoc 2.5.4 (`assets/redoc/`, MIT licence) |
| `tools/build.py` | Generates the files above from `en/openapi.yaml` and `i18n/`, and checks them |

Only the prose is translated. The examples (with the server's English error messages), field
names, error codes, settings and everything in backticks are the same in every language, and the
build refuses a translation whose code spans, links or Markdown structure differ from the English.

## Updating the reference

The English description follows the server: when the API changes on master, change
`en/openapi.yaml` with it (the code and `docs/API.md` are the reference). Then, with Python 3.9+
and `ruamel.yaml` (`pip install ruamel.yaml`):

```sh
python3 tools/build.py --check            # lists the translations that are now missing or stale
python3 tools/build.py --todo fr          # i18n/fr.todo.json: {pointer: English text} to translate
# translate the values of the todo file into a file of the same shape, then:
python3 tools/build.py --merge fr fr.json # checks each translation and records it
python3 tools/build.py                    # writes <lang>/openapi.yaml, <lang>/index.html, index.html
python3 tools/build.py --check            # everything up to date
npx --yes @redocly/cli@2.0.8 lint en/openapi.yaml fr/openapi.yaml   # and the other languages
```

Do not edit the generated files by hand. The `.todo.json` files are not committed.

## Publishing

GitHub Pages serves this branch as it is (Settings > Pages: "Deploy from a branch", `gh-pages`,
`/ (root)`); `.nojekyll` turns Jekyll off. The pages load nothing from other sites: Redoc is in
`assets/redoc/` (from the `redoc` 2.5.4 npm package, `bundles/redoc.standalone.js`, SHA-256
`dcaf76612bc4a3fbcc923a8966dee2f6146a5f32e5ce1b6f02dd60cbbf89500b`), and a Content Security
Policy keeps it that way.

The translations were written with an AI model (Claude) from the English and checked by the build;
corrections from native speakers are welcome, through `i18n/<lang>.json`.

The documentation is part of the Scacelith server, free software under the GNU General Public
License, version 3 or later ([LICENSE](https://github.com/DarkCenobyte/scacelith-chess-server/blob/master/LICENSE)).
