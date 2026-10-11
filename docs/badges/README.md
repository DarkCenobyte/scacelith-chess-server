# README badges

The badges of the [README](../../README.md) whose text never changes are the files of this folder
rather than images from img.shields.io. GitHub shows outside images through its image proxy, which
gives up after about four seconds; shields.io often answers later than that, and each such badge
then shows as a broken image. The CI, Release build and CodeQL badges are GitHub's own workflow
badges, and the coverage badge is `coverage.svg` on the `badges` branch, written by the Coverage
workflow ([DEVELOPMENT.md](../DEVELOPMENT.md#test-coverage)). Only the badges whose value changes
by itself (latest release, downloads, Rust toolchain, online protocol, website and official server
status, game client version) still come from shields.io, with a one-hour cache where the value
changes rarely.

Each file is the shields.io badge of the table below, saved by `tools/readme_badge.py`, which also
writes the logo into the file (shields.io links it as a `data:` URI). To change one, change its URL
here and save it again from the repository's root:

```sh
python3 -I tools/readme_badge.py '<URL>' docs/badges/<file>
```

| File | shields.io URL |
|---|---|
| `license.svg` | `https://img.shields.io/badge/license-GPL--3.0-blue` |
| `linux.svg` | `https://img.shields.io/badge/Linux-x86--64%20%28static%2C%20musl%29-FCC624?logo=linux&logoColor=black` |
| `sqlite.svg` | `https://img.shields.io/badge/database-SQLite-003B57?logo=sqlite&logoColor=white` |
| `api-reference.svg` | `https://img.shields.io/badge/API%20reference-OpenAPI%203.1-6BA539?logo=openapiinitiative&logoColor=white` |
| `claude-code.svg` | `https://img.shields.io/badge/built%20with-Claude%20Code-D97757?logo=claude&logoColor=white` |
