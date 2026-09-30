"""S3 path (Garage/R2 stand-in): moto server, raw/<user>/<session>/seg_n layout, pipe: loader URLs."""

from __future__ import annotations

import json

import pytest

pytestmark = pytest.mark.s3


@pytest.fixture
def s3(monkeypatch):
    server_mod = pytest.importorskip("moto.server")
    srv = server_mod.ThreadedMotoServer(ip_address="127.0.0.1", port=0, verbose=False)
    srv.start()
    host, port = srv.get_host_and_port()
    ep = f"http://{host}:{port}"
    for k, v in {"GAMECAP_S3_ENDPOINT": ep, "AWS_ACCESS_KEY_ID": "test", "AWS_SECRET_ACCESS_KEY": "test",
                 "AWS_REGION": "us-east-1"}.items():
        monkeypatch.setenv(k, v)
    from gamecap_pipeline.store import s3_client
    c = s3_client()
    c.create_bucket(Bucket="gamecap")
    yield c
    srv.stop()


def test_s3_end_to_end(s3, synth_root, tmp_path):
    from gamecap_pipeline.deletions import Deletions
    from gamecap_pipeline.loader import raw_dataset, shard_urls
    from gamecap_pipeline.process import ProcessConfig, run
    from gamecap_pipeline.shards import rebuild_affected
    from gamecap_pipeline.store import open_store

    raw = open_store("s3://gamecap/raw")
    seg_dir = synth_root / "sessions" / "sess-test"
    for seg in ("seg_000000", "seg_000001"):
        for f in ("video.mp4", "frames.parquet", "inputs.parquet", "focus.parquet", "manifest.json"):
            raw.put_file(seg_dir / seg / f, f"user-42/sess-test/{seg}/{f}")
    # an in-flight upload without manifest must be ignored
    raw.put_file(seg_dir / "seg_000001" / "video.mp4", "user-42/sess-test/seg_000009/video.mp4")

    s = run(ProcessConfig(input_url="s3://gamecap/raw", output_url="s3://gamecap", dataset_version="v1",
                          workdir=tmp_path / "w", workers=2))
    assert s["found"] == 2 and s["processed"] == 2 and s["errors"] == 0
    keys = [o["Key"] for o in s3.list_objects_v2(Bucket="gamecap", Prefix="shards/v1/")["Contents"]]
    assert "shards/v1/action_spec.json" in keys
    assert any(k.endswith(".tar") for k in keys) and any(k.endswith(".tar.sources.json") for k in keys)
    assert not list((tmp_path / "w" / "shards" / "v1").glob("*.tar")), "local shard copies removed after upload"

    urls = shard_urls("s3://gamecap/shards/v1")
    assert urls and all(u.startswith("pipe:") for u in urls)
    samples = list(raw_dataset(urls))
    assert len(samples) == s["clips"] and samples[0]["actions"].shape == (64, 292)
    assert samples[0]["meta"]["source_key"].startswith("user-42/sess-test/")

    # DELETE /me/data style: whole user prefix
    side = json.loads(s3.get_object(Bucket="gamecap", Key=[k for k in keys if k.endswith(".sources.json")][0])["Body"].read())
    assert side["sources"][0]["source_key"].startswith("user-42/")
    s3.put_object(Bucket="gamecap", Key="deletions/2026-09-29.txt", Body=b"user-42\n")
    dels = Deletions.load(["s3://gamecap/deletions"])
    res = rebuild_affected(open_store("s3://gamecap"), "v1", dels, tmp_path / "rb")
    assert res and all(r["action"] == "removed" for r in res)
    left = [o["Key"] for o in s3.list_objects_v2(Bucket="gamecap", Prefix="shards/v1/").get("Contents", [])]
    assert not any(k.endswith(".tar") for k in left)

    s2 = run(ProcessConfig(input_url="s3://gamecap/raw", output_url="s3://gamecap", dataset_version="v1",
                           workdir=tmp_path / "w", workers=1, deletions=["s3://gamecap/deletions"]))
    assert s2["skipped_deleted"] == 2 and s2["processed"] == 0
