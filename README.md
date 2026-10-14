# nbRipout

Strip outputs from Jupyter (and Zeppelin) notebooks before they reach git. A safer,
faster, drop-in replacement for [nbstripout](https://github.com/kynan/nbstripout),
written in Rust.

- **Drop-in.** Same flags, same git configuration (filter `nbstripout`, diff driver
  `ipynb`), same `filter.nbstripout.extrakeys` / `keepmetadatakeys` settings, and
  byte-identical output. Existing `.gitattributes` keep working and teammates can
  mix both tools without diff noise.
- **Safer.** Notebooks are replaced atomically and are never left truncated.
- **Faster.** 10–90× faster, mostly by not starting a Python interpreter per notebook.

## Install

```sh
cargo install --git https://github.com/thomascamminady/nbripout   # Rust >= 1.81
```

## Usage

```sh
nbripout --install              # set up the git filter for this repository
nbripout --install --global     # ... or for all repositories
nbripout --status               # show what is configured

nbripout notebook.ipynb         # strip in place
nbripout --verify *.ipynb       # exit 1 if anything would be stripped (CI)
cat in.ipynb | nbripout > out.ipynb
```

Every nbstripout option is supported: `--keep-output`, `--keep-count`, `--keep-id`,
`--extra-keys`, `--keep-metadata-keys`, `--drop-empty-cells`, `--drop-tagged-cells`,
`--strip-init-cells`, `--keep-output-type`, `--drop-output-type`, `--max-size`,
`--mode zeppelin`, `--textconv`, `--dry-run`, `--verify`, `--attributes`, `--global`,
`--system`, `--uninstall`, `--is-installed`. Cell-level control works as before:
`"keep_output": true` metadata, the `keep_output` tag and `init_cell`. See
`nbripout --help`.

### pre-commit

```yaml
repos:
  - repo: https://github.com/thomascamminady/nbripout
    rev: <tag or commit>
    hooks:
      - id: nbripout
```

### Migrating from nbstripout

Run `nbripout --install` in each repository (or `--install --global` once). It
rewrites the `nbstripout` filter commands in `.git/config` to point at nbripout and
leaves the attributes alone if they exist. Nothing in the repository changes.

## Safer

nbstripout rewrites notebooks in place by truncating the file and then writing the
new content. Any error in between leaves an **empty file**. Two inputs that do this
with nbstripout 0.9.1 (the notebooks end up 0 bytes):

- a code cell without an `outputs` key (hand-edited or generated notebooks),
- a string with an unpaired UTF-16 surrogate escape such as `"\ud800"`.

nbripout instead:

- writes to a temporary file in the same directory, `fsync`s it and renames it over
  the original, so readers see the old or the new notebook, never a partial one
  (even on Ctrl-C, a crash or a full disk);
- follows symlinks (the link stays a link) and keeps file permissions;
- leaves files alone when stripping changes nothing;
- keeps going after a broken notebook, reports every failure and exits with 1;
- refuses input it cannot round-trip (lone surrogates, invalid UTF-8, nesting deeper
  than 1000 levels) with a line/column error message instead of guessing.

## Faster

`uv run scripts/bench.py CORPUS` on an Apple M1 Max with 208 real
notebooks (69.7 MB, from *Python Data Science Handbook*, *Hands-On ML 3* and
*fastbook*):

| benchmark                             | nbstripout | nbripout | speedup |
|---------------------------------------|-----------:|---------:|--------:|
| one 77 kB notebook, stdin → stdout    |     144 ms |    13 ms |     11× |
| 208 notebooks in place                |    1.00 s  |    60 ms |     17× |
| `git add` 208 notebooks, `clean` filter |   31.9 s  |   3.9 s  |      8× |
| `git add` 208 notebooks, `process` filter |    —   |   360 ms |     89× |

Single-file runs are dominated by process start-up and the `git config` lookup
for `extrakeys` (≈ 9 ms on macOS) that both tools perform. Two things remove that
overhead:

- **git's long-running filter process.** `nbripout --install` configures
  `filter.nbstripout.process`, so git (≥ 2.11) starts *one* nbripout per command
  and streams every notebook through it. The per-file `clean` command stays
  configured as a fallback for older git and other git implementations.
  Use `--install --no-process` for nbstripout's exact configuration.
- **Parallel batches.** Files given on the command line are processed on all
  cores (`-j N` to limit).

## Compatibility

nbripout reimplements what nbstripout does through nbformat, including the parts
that are easy to miss: Python's `json` number and escape formatting (`1e-05`,
`NaN`, `ensure_ascii` for v3 and Zeppelin), `str.splitlines()` line boundaries,
nbformat's selective line splitting (`text/*`, SVG and JavaScript only), transient
key removal, cell id repair for nbformat ≥ 4.5, and which edits count as a change.

`scripts/differential.py` checks this by running nbstripout 0.9.1 in-process and
nbripout as a subprocess on randomized notebooks (v2, v3, v4.4, v4.5, Zeppelin; random
metadata, outputs, unicode, floats, ids, on-disk layouts) and on real notebooks, for
15 flag combinations, comparing output bytes, `--dry-run` decisions and in-place
results. Current state: 0 differences on 3,000 random notebooks and 208 real ones
(× 15 flag sets).

### Deliberate differences

Where nbstripout crashes, corrupts data or is non-deterministic, nbripout differs:

| situation | nbstripout 0.9.1 | nbripout |
|---|---|---|
| `--drop-tagged-cells "a b"` | only drops cells tagged `b` (closure bug) | drops cells with any listed tag |
| `--drop-empty-cells` on nbformat v3 | drops every code cell (checks `source`, v3 uses `input`) | checks `input` for v3 code cells |
| write error after truncation | empty file | file untouched |
| `--keep-id` with missing/duplicate ids | random new ids on every run | deterministic ids (`"3"`, `"3-1"`, ...) |
| `*.zpln` through the git filter | crashes (filter runs in Jupyter mode) | detects Zeppelin notebooks by content |
| broken notebook among many files | stops at the first one | processes the rest, then exits 1 |
| UTF-8 BOM | "No valid notebook detected" | accepted |
| `--install --global` attributes file | honors `$XDG_CONFIG_DIR`, which git ignores | `git var GIT_ATTR_GLOBAL` (git's own answer) |
| `--install` in a linked worktree | writes the worktree's private `info/attributes` (ignored by git) | `git rev-parse --git-path info/attributes` |
| `--version` | `0.9.1` | `nbripout <version>` |

nbstripout's `--python` install option does not apply and is not supported;
nbformat v1 notebooks (2011) are rejected with an upgrade hint.

## Development

```sh
cargo test                                             # unit + integration tests (needs git)
uv run scripts/differential.py --count 1000 [DIRS...]  # compare with nbstripout
uv run scripts/bench.py CORPUS_DIR                     # benchmark
```

Layout: `json.rs` (Python-compatible JSON), `py.rs` (Python string semantics),
`notebook.rs` (nbformat read/write normalization), `strip.rs` (stripping rules),
`fsutil.rs` (atomic writes), `git.rs` (install/status/config),
`filter_process.rs` (git's pkt-line filter protocol), `main.rs` (CLI).
