from __future__ import annotations

import shutil
from pathlib import Path

import pytest

from gamecap_pipeline.synth import SynthConfig, synth

OFFSET_NS = -35_000_000  # non-zero, not a multiple of the tick


@pytest.fixture(scope="session")
def synth_root(tmp_path_factory) -> Path:
    """Two consecutive 60 s synthetic segments (read-only; copy before mutating)."""
    root = tmp_path_factory.mktemp("synth")
    synth(SynthConfig(out=root, session_id="sess-test", segments=2, latency_offset_ns=OFFSET_NS))
    return root


@pytest.fixture(scope="session")
def seg0(synth_root) -> Path:
    return synth_root / "sessions" / "sess-test" / "seg_000000"


@pytest.fixture
def seg_copy(seg0, tmp_path) -> Path:
    dst = tmp_path / "sessions" / "sess-test" / "seg_000000"
    shutil.copytree(seg0, dst)
    return dst


@pytest.fixture(scope="session")
def processed(synth_root, tmp_path_factory):
    """Run the full pipeline once over the synthetic session (local store)."""
    from gamecap_pipeline.process import ProcessConfig, run

    out = tmp_path_factory.mktemp("out")
    work = tmp_path_factory.mktemp("work")
    cfg = ProcessConfig(input_url=str(synth_root / "sessions"), output_url=str(out),
                        dataset_version="vtest", workdir=work, workers=2)
    summary = run(cfg)
    return {"summary": summary, "out": out, "work": work, "cfg": cfg,
            "shard_dir": out / "shards" / "vtest"}
