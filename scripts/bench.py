# /// script
# requires-python = ">=3.10"
# dependencies = ["nbstripout==0.9.1", "fire>=0.6"]
# ///
"""Benchmark nbripout against nbstripout.

Measures (median wall time):

* ``latency``: one notebook through stdin/stdout, i.e. one git clean-filter call,
* ``batch``: stripping a directory of notebooks in place,
* ``git add``: staging all notebooks of a fresh repository through the git filter
  (nbstripout as clean filter, nbripout as clean filter, nbripout as process filter).

Usage::

    uv run scripts/bench.py CORPUS_DIR [--binary target/release/nbripout]
"""

import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable
from pathlib import Path

import fire

ISOLATED_GIT = {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}


def median_seconds(action: Callable[[], None], repeat: int, setup: Callable[[], None] | None = None) -> float:
    samples = []
    for _ in range(repeat):
        if setup is not None:
            setup()
        start = time.perf_counter()
        action()
        samples.append(time.perf_counter() - start)
    return statistics.median(samples)


def run(cmd: list[str], cwd: Path | None = None, stdin: bytes | None = None) -> None:
    subprocess.run(cmd, cwd=cwd, input=stdin, stdout=subprocess.DEVNULL, check=True, env={**os.environ, **ISOLATED_GIT})


def git_add(tools: dict[str, str], notebooks: list[Path], process: bool, name: str, workdir: Path) -> float:
    repo = workdir / "repo"

    def setup() -> None:
        shutil.rmtree(repo, ignore_errors=True)
        repo.mkdir()
        run(["git", "init", "-q", "."], cwd=repo)
        key = "filter.nbstripout.process" if process else "filter.nbstripout.clean"
        command = f'"{tools[name]}" --filter-process' if process else f'"{tools[name]}"'
        run(["git", "config", key, command], cwd=repo)
        run(["git", "config", "filter.nbstripout.smudge", "cat"], cwd=repo)
        run(["git", "config", "filter.nbstripout.required", "true"], cwd=repo)
        (repo / ".gitattributes").write_text("*.ipynb filter=nbstripout\n")
        for i, nb in enumerate(notebooks):
            shutil.copy(nb, repo / f"nb{i:04d}.ipynb")

    return median_seconds(lambda: run(["git", "add", "-A"], cwd=repo), repeat=3, setup=setup)


def main(corpus: str, binary: str = "target/release/nbripout") -> None:
    """Benchmark both tools on the notebooks below CORPUS."""
    # Prefer the nbstripout installed next to this interpreter (the script's env).
    local = Path(sys.executable).parent / "nbstripout"
    nbstripout = str(local) if local.exists() else shutil.which("nbstripout")
    if nbstripout is None:
        sys.exit("nbstripout not found")
    tools = {"nbstripout": nbstripout, "nbripout": str(Path(binary).resolve())}
    notebooks = sorted(Path(corpus).rglob("*.ipynb"))
    total_mb = sum(nb.stat().st_size for nb in notebooks) / 1e6
    sample = sorted(notebooks, key=lambda nb: nb.stat().st_size)[len(notebooks) // 2]
    data = sample.read_bytes()
    print(f"corpus: {len(notebooks)} notebooks, {total_mb:.1f} MB; latency sample: {len(data) / 1e3:.0f} kB\n")

    rows: list[tuple[str, dict[str, float]]] = []
    rows.append(
        (
            "single notebook, stdin -> stdout",
            {t: median_seconds(lambda t=t: run([tools[t]], stdin=data), 20) for t in tools},
        )
    )

    with tempfile.TemporaryDirectory() as tmp:
        workdir = Path(tmp)
        batch_dir = workdir / "batch"

        def fresh_copy() -> None:
            shutil.rmtree(batch_dir, ignore_errors=True)
            batch_dir.mkdir()
            for i, nb in enumerate(notebooks):
                shutil.copy(nb, batch_dir / f"nb{i:04d}.ipynb")

        files = [f"nb{i:04d}.ipynb" for i in range(len(notebooks))]
        rows.append(
            (
                f"{len(notebooks)} notebooks in place",
                {t: median_seconds(lambda t=t: run([tools[t], *files], cwd=batch_dir), 3, fresh_copy) for t in tools},
            )
        )
        rows.append(
            (
                f"git add {len(notebooks)} notebooks",
                {t: git_add(tools, notebooks, False, t, workdir) for t in tools},
            )
        )
        process = git_add(tools, notebooks, True, "nbripout", workdir)

    print(f"{'benchmark':<36} {'nbstripout':>12} {'nbripout':>12} {'speedup':>9}")
    for label, times in rows:
        a, b = times["nbstripout"], times["nbripout"]
        print(f"{label:<36} {a * 1e3:>10.1f}ms {b * 1e3:>10.1f}ms {a / b:>8.0f}x")
    a = rows[-1][1]["nbstripout"]
    print(f"{'  ... nbripout --filter-process':<36} {'':>12} {process * 1e3:>10.1f}ms {a / process:>8.0f}x")


if __name__ == "__main__":
    fire.Fire(main)
