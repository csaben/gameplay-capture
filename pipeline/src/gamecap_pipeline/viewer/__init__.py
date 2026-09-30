"""`gamecap-pipeline viewer`: browse and play recorded sessions with their inputs.

Sources are local session folders or S3 prefixes (Garage); see server.py for
the HTTP API and static/ for the web UI (vanilla JS, no build step).
"""

from __future__ import annotations

import logging
import tomllib
from pathlib import Path

from .server import Viewer, check_ffmpeg, default_cache_dir, make_server
from .sources import Source, make_source

log = logging.getLogger("gamecap.viewer")


def sources_from_gamecap_config(path: Path, list_ttl: float = 30.0) -> list[Source]:
    """A gamecap config.toml (or just its [upload] snippet): Garage raw/ for all
    users, plus the local sessions dir when it exists."""
    conf = tomllib.loads(Path(path).read_text(encoding="utf-8"))
    out: list[Source] = []
    up = conf.get("upload") or {}
    if up.get("target", "s3") == "s3" and up.get("bucket"):
        out.append(make_source("garage", f"s3://{up['bucket']}/raw/", endpoint=up.get("endpoint"),
                               region=up.get("region"), access_key=up.get("access_key"),
                               secret_key=up.get("secret_key"), list_ttl=list_ttl))
    root = conf.get("sessions_root") or (Path(conf["data_dir"]) / "sessions" if conf.get("data_dir") else None)
    if root and Path(root).is_dir():
        out.append(make_source("local", str(root), list_ttl=list_ttl))
    return out


def run(sources: list[str], gamecap_config: str | None, endpoint: str | None, region: str | None,
        host: str, port: int, cache_dir: str | None, list_ttl: float) -> int:
    srcs: list[Source] = []
    if gamecap_config:
        srcs += sources_from_gamecap_config(Path(gamecap_config).expanduser(), list_ttl)
    for spec in sources or []:
        name, sep, url = spec.partition("=")
        if not sep:
            name, url = ("s3" if spec.startswith("s3://") else "local"), spec
        srcs = [s for s in srcs if s.name != name]  # explicit --source wins
        srcs.append(make_source(name, url, endpoint=endpoint, region=region, list_ttl=list_ttl))
    if not srcs:
        log.error("no sources: pass --source NAME=DIR|s3://bucket/prefix and/or --gamecap-config")
        return 2
    cache = Path(cache_dir) if cache_dir else default_cache_dir()
    if not check_ffmpeg():
        log.warning("ffmpeg not on PATH: only mode=raw video works (no remux / h264)")
    viewer = Viewer(srcs, cache)
    srv = make_server(viewer, host, port)
    for s in srcs:
        log.info("source %s: %s", s.name, s.store.url)
    log.info("cache dir %s", cache)
    log.info("viewer on http://%s:%d/", host, port)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        srv.server_close()
    return 0


__all__ = ["Viewer", "make_server", "make_source", "run", "sources_from_gamecap_config"]
