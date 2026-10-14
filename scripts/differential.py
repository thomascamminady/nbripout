# /// script
# requires-python = ">=3.10"
# dependencies = ["nbstripout==0.9.1", "fire>=0.6"]
# ///
"""Differential test: nbripout must behave byte-for-byte like nbstripout.

Generates random notebooks (nbformat 4.4, 4.5, 3 and 2, plus Zeppelin), optionally adds
real notebooks from corpus directories, and compares for a range of flag sets:

* the stripped output (stdout mode),
* which files count as changed (``--dry-run``),
* the files left behind by in-place stripping.

nbstripout runs in-process through its own ``process_*_notebook`` functions, nbripout
as a subprocess. Usage::

    uv run scripts/differential.py --binary target/release/nbripout --count 300 [CORPUS_DIR ...]
"""

import copy
import io
import json
import logging
import math
import os
import random
import shutil
import struct
import subprocess
import sys
import tempfile
import warnings
from argparse import Namespace
from collections.abc import Iterator
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import fire
from nbstripout._nbstripout import process_jupyter_notebook, process_zeppelin_notebook

DEFAULT_EXTRA_KEYS = [
    "metadata.signature",
    "metadata.widgets",
    "cell.metadata.collapsed",
    "cell.metadata.ExecuteTime",
    "cell.metadata.execution",
    "cell.metadata.heading_collapsed",
    "cell.metadata.hidden",
    "cell.metadata.scrolled",
]

# Flag sets compared between the tools. Multi-tag `--drop-tagged-cells` and
# `--drop-empty-cells` on v2/v3 are deliberate bug fixes and therefore excluded.
FLAG_SETS: list[list[str]] = [
    [],
    ["--keep-output"],
    ["--keep-count"],
    ["--keep-id"],
    ["--drop-empty-cells"],
    ["--drop-tagged-cells", "t1"],
    ["--strip-init-cells"],
    ["--extra-keys", "metadata.foo cell.metadata.bar cell.metadata.jupyter.source_hidden"],
    ["--extra-keys", "cell.attachments metadata.kernelspec.name"],
    ["--keep-metadata-keys", "cell.metadata.collapsed metadata.widgets"],
    ["--max-size", "40"],
    ["--max-size", "1k"],
    ["--keep-output-type=stream"],
    ["--keep-output", "--drop-output-type", "error", "stream:stderr"],
    ["--keep-output", "--keep-count", "--keep-id"],
]

ALPHABET = (
    "abcxyz ABC 0123 _-.:/\\\"'{}[],\t\n\n\n\r\r\n\x0b\x0c\x1c\x1d\x1e\x1f\x00\x01\x7f"
    "\x85\xa0\u00e9\u6f22\u5b57\u00df\u2028\u2029\u200b\ufeff\U0001f600\U0001f389"
)


@dataclass
class Case:
    name: str
    text: str
    mode: str = "jupyter"
    # nbformat assigns random ids to cells without (unique) ids, so `--keep-id`
    # output is not reproducible for such notebooks.
    random_ids: bool = False
    version: int = 4
    errors: set[str] = field(default_factory=set)


def random_text(rng: random.Random, max_len: int = 40) -> str:
    return "".join(rng.choice(ALPHABET) for _ in range(rng.randint(0, max_len)))


def random_float(rng: random.Random) -> float:
    choice = rng.random()
    if choice < 0.5:
        while True:
            (value,) = struct.unpack("<d", rng.randbytes(8))
            if math.isfinite(value):
                return value
    if choice < 0.7:
        return rng.choice([0.0, -0.0, 1e-5, 1e16, 1e15, 0.1, 100.0, 1e22, 5e-324, 2.5e-7])
    if choice < 0.8:
        # Exactly halfway between two shortest representations: Python rounds
        # these half to even, Rust's formatter would round up.
        return float(rng.randrange(2**49, 2**52)) + rng.choice([0.25, 0.75])
    return rng.uniform(-1e6, 1e6)


def random_json(rng: random.Random, depth: int = 0) -> Any:
    kinds = ["str", "int", "float", "bool", "null"]
    if depth < 3:
        kinds += ["list", "dict"]
    kind = rng.choice(kinds)
    if kind == "str":
        return random_text(rng)
    if kind == "int":
        return rng.choice([0, -1, 7, 2**70, -(2**65), rng.randint(-1000, 1000)])
    if kind == "float":
        return rng.choice([random_float(rng), math.nan, math.inf, -math.inf])
    if kind == "bool":
        return rng.random() < 0.5
    if kind == "null":
        return None
    if kind == "list":
        return [random_json(rng, depth + 1) for _ in range(rng.randint(0, 3))]
    return {random_text(rng, 8): random_json(rng, depth + 1) for _ in range(rng.randint(0, 3))}


def maybe_lines(rng: random.Random, text: str) -> str | list[str]:
    """On-disk notebooks store multi-line strings either joined or split."""
    return text.splitlines(True) if rng.random() < 0.5 else text


def random_bundle(rng: random.Random) -> dict[str, Any]:
    bundle: dict[str, Any] = {}
    for mime in ["text/plain", "text/html", "image/png", "image/svg+xml", "application/javascript"]:
        if rng.random() < 0.4:
            bundle[mime] = maybe_lines(rng, random_text(rng, 80))
    if rng.random() < 0.3:
        bundle["application/json"] = random_json(rng)
    if rng.random() < 0.2:
        bundle["application/vnd.custom+json"] = [random_text(rng), random_text(rng)]
    return bundle


def random_output(rng: random.Random, count: int | None) -> dict[str, Any]:
    kind = rng.choice(["stream", "execute_result", "display_data", "error"])
    if kind == "stream":
        return {
            "output_type": "stream",
            "name": rng.choice(["stdout", "stderr"]),
            "text": maybe_lines(rng, random_text(rng, 60)),
        }
    if kind == "execute_result":
        return {
            "output_type": "execute_result",
            "execution_count": count,
            "data": random_bundle(rng),
            "metadata": {},
        }
    if kind == "display_data":
        return {"output_type": "display_data", "data": random_bundle(rng), "metadata": random_json(rng, 2) or {}}
    return {
        "output_type": "error",
        "ename": random_text(rng, 10),
        "evalue": random_text(rng, 10),
        "traceback": [random_text(rng) for _ in range(rng.randint(0, 3))],
    }


def random_cell_metadata(rng: random.Random) -> dict[str, Any]:
    metadata: dict[str, Any] = {}
    for key in ["collapsed", "scrolled", "ExecuteTime", "execution", "hidden", "heading_collapsed", "bar"]:
        if rng.random() < 0.2:
            metadata[key] = random_json(rng, 2)
    if rng.random() < 0.4:
        metadata["tags"] = rng.sample(["t1", "t2", "keep_output", "other"], rng.randint(0, 2))
    if rng.random() < 0.15 and "keep_output" not in metadata.get("tags", []):
        metadata["keep_output"] = rng.choice([True, False, 1, 0, "yes", None])
    elif rng.random() < 0.1:
        metadata["keep_output"] = True
    if rng.random() < 0.1:
        metadata["init_cell"] = rng.choice([True, False, 1])
    if rng.random() < 0.2:
        metadata["jupyter"] = {"source_hidden": True, "outputs_hidden": False}
    if rng.random() < 0.1:
        metadata["trusted"] = True
    if rng.random() < 0.2:
        metadata[random_text(rng, 6)] = random_json(rng)
    return metadata


def random_source(rng: random.Random) -> str:
    return rng.choice(["", "  \n\t", " \x1c", random_text(rng, 60), "x = 1\nprint(x)\n"])


def random_v4(rng: random.Random, minor: int) -> tuple[dict[str, Any], bool]:
    cells = []
    random_ids = False
    ids: list[str] = []
    for i in range(rng.randint(0, 6)):
        cell_type = rng.choice(["code", "code", "markdown", "raw"])
        cell: dict[str, Any] = {"cell_type": cell_type, "metadata": random_cell_metadata(rng)}
        cell["source"] = maybe_lines(rng, random_source(rng))
        if cell_type == "code":
            count = rng.choice([None, rng.randint(1, 50)])
            cell["execution_count"] = count
            cell["outputs"] = [random_output(rng, count) for _ in range(rng.randint(0, 3))]
        elif rng.random() < 0.2:
            cell["attachments"] = {"img.png": random_bundle(rng)}
        if minor >= 5:
            roll = rng.random()
            if roll < 0.08:
                random_ids = True
            elif roll < 0.15 and ids:
                cell["id"] = rng.choice(ids)
                random_ids = True
            else:
                cell["id"] = rng.choice([str(i), f"cell-{rng.randint(0, 10**6)}", "abc"])
                if cell["id"] in ids:
                    random_ids = True
                ids.append(cell["id"])
        cells.append(cell)
    metadata: dict[str, Any] = {"kernelspec": {"name": "python3", "display_name": "Python 3"}}
    for key in ["widgets", "signature", "foo", "orig_nbformat"]:
        if rng.random() < 0.2:
            metadata[key] = random_json(rng)
    if rng.random() < 0.1:
        metadata["keep_output"] = rng.choice([True, False])
    if rng.random() < 0.3:
        metadata["extra"] = random_json(rng)
    return {"cells": cells, "metadata": metadata, "nbformat": 4, "nbformat_minor": minor}, random_ids


def random_v3(rng: random.Random) -> dict[str, Any]:
    cells = []
    for _ in range(rng.randint(0, 5)):
        if rng.random() < 0.6:
            count = rng.choice([None, rng.randint(1, 9)])
            outputs = []
            for _ in range(rng.randint(0, 2)):
                outputs.append(
                    rng.choice(
                        [
                            {"output_type": "stream", "stream": "stdout", "text": maybe_lines(rng, random_text(rng))},
                            {
                                "output_type": "pyout",
                                "prompt_number": count,
                                "text": maybe_lines(rng, random_text(rng)),
                            },
                            {"output_type": "display_data", "png": "iVBOR", "html": [random_text(rng)]},
                        ]
                    )
                )
            cell = {
                "cell_type": "code",
                "collapsed": rng.random() < 0.5,
                "input": maybe_lines(rng, random_source(rng)),
                "language": "python",
                "metadata": random_cell_metadata(rng),
                "outputs": outputs,
                "prompt_number": count,
            }
        else:
            cell = {
                "cell_type": rng.choice(["markdown", "heading"]),
                "metadata": random_cell_metadata(rng),
                "source": maybe_lines(rng, random_source(rng)),
            }
        cells.append(cell)
    return {
        "metadata": {"name": random_text(rng, 10), "signature": "sha256:abc"},
        "nbformat": 3,
        "nbformat_minor": 0,
        "worksheets": [{"cells": cells, "metadata": {}}],
    }


def random_v2(rng: random.Random) -> dict[str, Any]:
    """nbformat v2: like v3, but images are held as bytes and lines are split
    without line endings."""
    nb = random_v3(rng)
    nb["nbformat"] = 2
    del nb["nbformat_minor"]
    for cell in nb["worksheets"][0]["cells"]:
        for output in cell.get("outputs", []):
            if rng.random() < 0.5:
                output[rng.choice(["png", "jpeg"])] = "".join(
                    rng.choice("AZaz09+/=\n'\"\\\x01") for _ in range(rng.randint(0, 30))
                )
    return nb


def random_zeppelin(rng: random.Random) -> dict[str, Any]:
    paragraphs = []
    for _ in range(rng.randint(0, 4)):
        paragraph: dict[str, Any] = {"text": random_text(rng), "config": random_json(rng, 2)}
        if rng.random() < 0.7:
            paragraph["results"] = rng.choice(
                [{}, {"code": "SUCCESS", "msg": [{"type": "TEXT", "data": random_text(rng)}]}]
            )
        paragraphs.append(paragraph)
    return {"paragraphs": paragraphs, "name": random_text(rng, 8), "id": "2ABC", "config": {}}


def serialize(rng: random.Random, nb: dict[str, Any]) -> str:
    """Write a notebook the way some tool might have: indentation, key order and
    escaping vary, so the canonicalization is exercised too."""
    indent = rng.choice([None, 1, 1, 2, 4])
    return json.dumps(nb, indent=indent, sort_keys=rng.random() < 0.5, ensure_ascii=rng.random() < 0.3)


def generate(count: int, seed: int) -> Iterator[Case]:
    rng = random.Random(seed)
    for i in range(count):
        roll = rng.random()
        if roll < 0.45:
            nb, random_ids = random_v4(rng, 5)
            yield Case(f"gen{i:04d}-v4.5.ipynb", serialize(rng, nb), random_ids=random_ids)
        elif roll < 0.7:
            nb, _ = random_v4(rng, 4)
            yield Case(f"gen{i:04d}-v4.4.ipynb", serialize(rng, nb))
        elif roll < 0.8:
            yield Case(f"gen{i:04d}-v3.ipynb", serialize(rng, random_v3(rng)), version=3)
        elif roll < 0.87:
            yield Case(f"gen{i:04d}-v2.ipynb", serialize(rng, random_v2(rng)), version=2)
        else:
            yield Case(f"gen{i:04d}.zpln", serialize(rng, random_zeppelin(rng)), mode="zeppelin")


def corpus(dirs: list[str]) -> Iterator[Case]:
    for directory in dirs:
        for path in sorted(Path(directory).rglob("*.ipynb")):
            try:
                text = path.read_text(encoding="utf-8")
            except (UnicodeDecodeError, OSError):
                continue
            name = "corpus-" + "-".join(path.relative_to(directory).parts)
            yield Case(name, text, random_ids=True)


def reference_args(flags: list[str], mode: str) -> tuple[Namespace, list[str]]:
    args = Namespace(
        dry_run=False,
        keep_output=None,
        keep_count=False,
        keep_id=False,
        extra_keys="",
        keep_metadata_keys="",
        drop_empty_cells=False,
        drop_tagged_cells="",
        strip_init_cells=False,
        drop_output_type=[],
        keep_output_type=[],
        max_size="0",
        mode=mode,
    )
    i = 0
    while i < len(flags):
        flag, _, inline = flags[i].partition("=")
        name = flag.lstrip("-").replace("-", "_")
        if name in {"keep_output", "keep_count", "keep_id", "drop_empty_cells", "strip_init_cells"}:
            setattr(args, name, True)
        elif name in {"drop_output_type", "keep_output_type"}:
            values = [inline] if inline else []
            while not inline and i + 1 < len(flags) and not flags[i + 1].startswith("--"):
                i += 1
                values.append(flags[i])
            setattr(args, name, values)
        else:
            if not inline:
                i += 1
            setattr(args, name, inline or flags[i])
        i += 1
    keep = args.keep_metadata_keys.split()
    extra_keys = [k for k in DEFAULT_EXTRA_KEYS + args.extra_keys.split() if k not in keep]
    return args, extra_keys


def reference(case: Case, flags: list[str]) -> tuple[bool, str] | None:
    """nbstripout's (changed, stdout output), or None if it fails."""
    args, extra_keys = reference_args(flags, case.mode)
    process = process_zeppelin_notebook if case.mode == "zeppelin" else process_jupyter_notebook
    out = io.StringIO()
    try:
        changed = process(io.StringIO(case.text), out, args, extra_keys)
    except Exception:  # noqa: BLE001 - any nbstripout failure counts as "error"
        return None
    return bool(changed), out.getvalue()


def split_outputs(data: str) -> list[str]:
    """Split concatenated `--textconv` outputs: only a top-level closing brace
    starts a line with `}` (nested ones are indented)."""
    parts = data.split("\n}\n")
    return [part + "\n}\n" for part in parts[:-1]]


def run_nbripout(binary: str, args: list[str], cwd: Path) -> tuple[int, str, str]:
    """Run nbripout; returns (exit code, stdout, stderr) without newline translation."""
    env = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
    result = subprocess.run([binary, "--unix-newlines", *args], cwd=cwd, env=env, capture_output=True, check=False)
    return result.returncode, result.stdout.decode("utf-8"), result.stderr.decode("utf-8", "replace")


def compare(binary: str, cases: list[Case], flags: list[str], workdir: Path) -> list[str]:
    failures: list[str] = []
    for mode in ["jupyter", "zeppelin"]:
        selected = [
            c
            for c in cases
            if c.mode == mode
            and not (c.random_ids and "--keep-id" in flags)
            and not (c.version < 4 and "--drop-empty-cells" in flags)
        ]
        expected = {c.name: reference(c, flags) for c in selected}
        ok_cases = [c for c in selected if expected[c.name] is not None]
        if not ok_cases:
            continue
        mode_flags = ["-m", mode] if mode == "zeppelin" else []
        shutil.rmtree(workdir, ignore_errors=True)
        workdir.mkdir()
        for c in selected:
            (workdir / c.name).write_text(c.text, encoding="utf-8", newline="")
        names = [c.name for c in ok_cases]
        label = f"{mode} {' '.join(flags) or '(defaults)'}"

        # 1. Stripped output.
        code, stdout, stderr = run_nbripout(binary, [*mode_flags, *flags, "-t", "--", *names], workdir)
        outputs = split_outputs(stdout)
        if code != 0 or len(outputs) != len(names):
            failures.append(f"{label}: textconv failed ({code}): {stderr[:500]}")
        else:
            for c, actual in zip(ok_cases, outputs, strict=True):
                exp = expected[c.name]
                assert exp is not None
                if actual != exp[1]:
                    failures.append(f"{label}: output differs for {c.name}")
                    dump = workdir.parent / "mismatch"
                    dump.mkdir(exist_ok=True)
                    (dump / f"{c.name}.input").write_text(c.text, encoding="utf-8", newline="")
                    (dump / f"{c.name}.expected").write_text(exp[1], encoding="utf-8", newline="")
                    (dump / f"{c.name}.actual").write_text(actual, encoding="utf-8", newline="")

        # 2. Change detection.
        _, stdout, _ = run_nbripout(binary, [*mode_flags, *flags, "--dry-run", "--", *names], workdir)
        reported = {line.removeprefix("Dry run: would have stripped ") for line in stdout.splitlines()}
        wanted = {c.name for c in ok_cases if (exp := expected[c.name]) is not None and exp[0]}
        if reported != wanted:
            failures.append(f"{label}: dry-run differs: extra={reported - wanted} missing={wanted - reported}")

        # 3. In-place stripping, including files nbstripout fails on.
        code, _, _ = run_nbripout(binary, [*mode_flags, *flags, "--", *[c.name for c in selected]], workdir)
        for c in selected:
            after = (workdir / c.name).read_text(encoding="utf-8")
            exp = expected[c.name]
            if exp is None:
                if after != c.text:
                    failures.append(f"{label}: {c.name} modified although stripping fails")
                continue
            want = exp[1] if exp[0] else c.text
            if after != want:
                failures.append(f"{label}: in-place result differs for {c.name}")
        if any(exp is None for exp in expected.values()) == (code == 0):
            failures.append(f"{label}: unexpected exit code {code}")
    return failures


def main(*corpus_dirs: str, binary: str = "target/release/nbripout", count: int = 300, seed: int = 0) -> None:
    """Compare nbripout against nbstripout on generated and corpus notebooks."""
    logging.disable(logging.CRITICAL)  # nbformat logs schema violations
    warnings.simplefilter("ignore")  # ... and warns about repaired cell ids
    binary = str(Path(binary).resolve())
    cases = list(generate(count, seed)) + list(corpus(list(corpus_dirs)))
    print(f"{len(cases)} notebooks x {len(FLAG_SETS)} flag sets", file=sys.stderr)
    failures: list[str] = []
    with tempfile.TemporaryDirectory() as tmp:
        for flags in FLAG_SETS:
            failures += compare(binary, copy.deepcopy(cases), flags, Path(tmp) / "work")
        if (Path(tmp) / "mismatch").exists():
            shutil.copytree(Path(tmp) / "mismatch", "differential-mismatch", dirs_exist_ok=True)
    for failure in failures:
        print(failure)
    print(f"{len(failures)} failures", file=sys.stderr)
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    fire.Fire(main)
