"""`gamecap-pipeline` command line.

  synth             generate spec-exact synthetic segment folders
  process           steps 1-8: ready segments -> WebDataset shards
  rebuild-affected  rewrite/remove shards that contain deleted segments
  replay            render a clip (from a shard) or a raw segment with actions overlaid
  validate          validate segment folder(s) and print alignment stats
  load              loader smoke test (needs the `train` extra)
  action-spec       print the action vector layout as json

Settings can also come from a TOML file (`--config pipeline.toml`) whose keys
are the long option names with '_' (e.g. `input = "s3://gamecap/raw"`,
`dataset_version = "v1"`, `max_dropped_ratio = 0.005`); CLI flags win.
"""

from __future__ import annotations

import argparse
import json
import logging
import sys
import tomllib
from pathlib import Path


def _load_config(argv: list[str]) -> dict:
    pre = argparse.ArgumentParser(add_help=False)
    pre.add_argument("--config")
    a, _ = pre.parse_known_args(argv)
    if a.config:
        return tomllib.loads(Path(a.config).read_text())
    return {}


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(prog="gamecap-pipeline", description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--config", help="TOML file with default option values")
    ap.add_argument("-v", "--verbose", action="store_true")
    sub = ap.add_subparsers(dest="cmd", required=True)

    s = sub.add_parser("synth", help="generate synthetic segments")
    s.add_argument("--out", required=True, type=Path)
    s.add_argument("--session-id", default=None)
    s.add_argument("--segments", type=int, default=1)
    s.add_argument("--first-segment-idx", type=int, default=0)
    s.add_argument("--seconds", type=int, default=60)
    s.add_argument("--latency-offset-ms", type=float, default=0.0)
    s.add_argument("--drop-ticks", default="1100,1101", help="comma list of tick indices to drop ('' for none)")
    s.add_argument("--layout", choices=["sessions", "raw"], default="sessions")
    s.add_argument("--user-id", default="synth-user")
    s.add_argument("--encoder", default=None, help="hevc_nvenc or libx265 (default: auto)")
    s.add_argument("--seed", type=int, default=0)
    s.add_argument("--no-unfocused", action="store_true", help="omit the 48-51 s unfocused span")

    p = sub.add_parser("process", help="process ready segments into shards")
    p.add_argument("--input", help="segments root: local dir or s3://bucket/prefix (e.g. s3://gamecap/raw)")
    p.add_argument("--output", help="output store root: local dir or s3://bucket (shards go under shards/<version>/)")
    p.add_argument("--dataset-version", default=None)
    p.add_argument("--workdir", type=Path, default=None)
    p.add_argument("--endpoint-url", default=None, help="S3 endpoint (else GAMECAP_S3_ENDPOINT / AWS_ENDPOINT_URL)")
    p.add_argument("--workers", type=int, default=None)
    p.add_argument("--limit", type=int, default=None)
    p.add_argument("--shard-size-mb", type=float, default=None, help="target shard size (default 1000)")
    p.add_argument("--keep-local", action="store_true", help="keep local shard copies after upload")
    p.add_argument("--retry-rejected", action="store_true")
    p.add_argument("--deletions", action="append", default=None,
                   help="deletion list: text file, local dir, or s3:// prefix (repeatable)")
    p.add_argument("--max-dropped-ratio", type=float, default=None)
    p.add_argument("--no-hash-check", action="store_true")
    p.add_argument("--clip-len", type=int, default=None)
    p.add_argument("--stride", type=int, default=None, help="min frames between clip starts (default clip-len)")
    p.add_argument("--reencode-mid-gop", action="store_true")
    p.add_argument("--reencode-codec", default=None)
    p.add_argument("--drop-idle", action="store_true", help="drop clips containing idle frames")
    p.add_argument("--idle-seconds", type=float, default=None)
    p.add_argument("--time-column", choices=["capture_ns", "tick_ns"], default=None)

    r = sub.add_parser("rebuild-affected", help="rewrite/remove shards containing deleted segments")
    r.add_argument("--output", help="output store root (as for process)")
    r.add_argument("--dataset-version", default=None)
    r.add_argument("--deletions", action="append", default=None)
    r.add_argument("--workdir", type=Path, default=None)
    r.add_argument("--endpoint-url", default=None)
    r.add_argument("--dry-run", action="store_true")

    rp = sub.add_parser("replay", help="render actions over a clip or segment")
    src = rp.add_mutually_exclusive_group(required=True)
    src.add_argument("--shard", help="local shard .tar")
    src.add_argument("--segment", help="raw segment folder")
    rp.add_argument("--key", default=None)
    rp.add_argument("--index", type=int, default=0)
    rp.add_argument("--start", type=int, default=0)
    rp.add_argument("--count", type=int, default=None)
    rp.add_argument("-o", "--out", required=True)

    v = sub.add_parser("validate", help="validate segment folders")
    v.add_argument("segments", nargs="+")
    v.add_argument("--max-dropped-ratio", type=float, default=0.005)

    ld = sub.add_parser("load", help="loader smoke test (train extra)")
    ld.add_argument("rest", nargs=argparse.REMAINDER)

    sub.add_parser("action-spec", help="print the action vector layout")
    return ap


def _pick(cli, conf: dict, name: str, default):
    return cli if cli is not None else conf.get(name, default)


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    conf = _load_config(argv)
    a = build_parser().parse_args(argv)
    logging.basicConfig(level=logging.DEBUG if a.verbose else logging.INFO,
                        format="%(asctime)s %(levelname)s %(name)s: %(message)s")

    if a.cmd == "synth":
        import uuid

        from .synth import SynthConfig, synth
        drops = tuple(int(x) for x in a.drop_ticks.split(",") if x.strip())
        cfg = SynthConfig(out=a.out, session_id=a.session_id or str(uuid.uuid4()), segments=a.segments,
                          first_segment_idx=a.first_segment_idx, seconds=a.seconds,
                          latency_offset_ns=int(a.latency_offset_ms * 1e6), drop_ticks=drops,
                          layout=a.layout, user_id=a.user_id, encoder=a.encoder, seed=a.seed,
                          unfocused=None if a.no_unfocused else (48.0, 51.0))
        for p in synth(cfg):
            print(p)
        return 0

    if a.cmd == "process":
        from .align import AlignConfig
        from .clips import ClipConfig
        from .process import ProcessConfig, run
        from .segment import ValidationConfig
        inp = _pick(a.input, conf, "input", None)
        out = _pick(a.output, conf, "output", None)
        if not inp or not out:
            print("process: --input and --output are required (or set in --config)", file=sys.stderr)
            return 2
        cfg = ProcessConfig(
            input_url=inp, output_url=out,
            dataset_version=_pick(a.dataset_version, conf, "dataset_version", "v0"),
            workdir=Path(_pick(a.workdir, conf, "workdir", ".gamecap-work")),
            endpoint_url=_pick(a.endpoint_url, conf, "endpoint_url", None),
            workers=_pick(a.workers, conf, "workers", 4),
            limit=_pick(a.limit, conf, "limit", None),
            shard_maxsize=int(_pick(a.shard_size_mb, conf, "shard_size_mb", 1000) * 1e6),
            keep_local=a.keep_local or conf.get("keep_local", False),
            retry_rejected=a.retry_rejected,
            deletions=_pick(a.deletions, conf, "deletions", []),
            validation=ValidationConfig(
                max_dropped_ratio=_pick(a.max_dropped_ratio, conf, "max_dropped_ratio", 0.005),
                check_hashes=not a.no_hash_check),
            align=AlignConfig(
                time_column=_pick(a.time_column, conf, "time_column", "capture_ns"),
                idle_threshold_ns=int(_pick(a.idle_seconds, conf, "idle_seconds", 10.0) * 1e9)),
            clips=ClipConfig(
                clip_len=_pick(a.clip_len, conf, "clip_len", 64),
                stride=_pick(a.stride, conf, "stride", None),
                reencode_mid_gop=a.reencode_mid_gop or conf.get("reencode_mid_gop", False),
                reencode_codec=_pick(a.reencode_codec, conf, "reencode_codec", "libx265"),
                drop_idle=a.drop_idle or conf.get("drop_idle", False)),
        )
        summary = run(cfg)
        print(json.dumps(summary, indent=1))
        return 0 if summary["errors"] == 0 else 1

    if a.cmd == "rebuild-affected":
        from .deletions import Deletions
        from .shards import rebuild_affected
        from .store import open_store
        out = _pick(a.output, conf, "output", None)
        ep = _pick(a.endpoint_url, conf, "endpoint_url", None)
        dels = Deletions.load(_pick(a.deletions, conf, "deletions", []), ep)
        res = rebuild_affected(open_store(out, ep), _pick(a.dataset_version, conf, "dataset_version", "v0"),
                               dels, Path(_pick(a.workdir, conf, "workdir", ".gamecap-work")) / "rebuild",
                               dry_run=a.dry_run)
        print(json.dumps(res, indent=1))
        return 0

    if a.cmd == "replay":
        from .replay import replay_segment, replay_shard_sample
        if a.shard:
            res = replay_shard_sample(a.shard, a.out, key=a.key, index=a.index)
        else:
            res = replay_segment(a.segment, a.out, start=a.start, count=a.count)
        print(json.dumps(res))
        return 0

    if a.cmd == "validate":
        from .align import align_segment
        from .segment import ValidationConfig, ValidationError, validate_and_load
        rc = 0
        for sp in a.segments:
            try:
                seg = validate_and_load(Path(sp), ValidationConfig(max_dropped_ratio=a.max_dropped_ratio))
                print(json.dumps({"segment": sp, "ok": True, "warnings": seg.warnings,
                                  "stats": align_segment(seg).stats}))
            except ValidationError as e:
                rc = 1
                print(json.dumps({"segment": sp, "ok": False, "reasons": e.reasons}))
        return rc

    if a.cmd == "load":
        from .loader import main as load_main
        return load_main(a.rest)

    if a.cmd == "action-spec":
        from .action_spec import default_spec
        spec = default_spec()
        print(json.dumps(spec.to_json_obj() | {"sha": spec.sha}, indent=1))
        return 0
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
