"""HTTP server for the session viewer (stdlib only).

Routes
  GET /                               index.html (+ /static/<file>)
  GET /api/sources                    [{name, kind, url}]
  GET /api/sessions?source=S[&refresh=1]
  GET /api/session?source=S&id=ID     summary + segments (with manifests)
  GET /api/timeline?source=S&id=ID&seg=seg_000000
  GET /video?source=S&id=ID&seg=SEG[&mode=raw|remux|h264]   (HTTP Range)

Video modes: ``raw`` streams video.mp4 as stored (fragmented MP4, HEVC);
``remux`` copies it into a faststart MP4 (no re-encode); ``h264`` transcodes it
once for browsers without HEVC. remux/h264 results are cached in ``cache_dir``.
"""

from __future__ import annotations

import hashlib
import json
import logging
import mimetypes
import shutil
import subprocess
import tempfile
import threading
import time
from collections import OrderedDict
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from importlib import resources
from pathlib import Path
from urllib.parse import parse_qs, urlparse

from .sources import Source, segment_info, summarize
from .timeline import timeline_from_bytes

log = logging.getLogger("gamecap.viewer")

H264_ENCODERS = [
    ("h264_nvenc", ["-preset", "p4", "-cq", "23"]),
    ("libx264", ["-preset", "veryfast", "-crf", "23"]),
    ("h264_qsv", ["-global_quality", "23"]),
    ("h264_amf", ["-quality", "balanced", "-rc", "cqp", "-qp_i", "23", "-qp_p", "23"]),
    ("h264_mf", ["-b:v", "3M"]),
    ("libopenh264", ["-b:v", "3M"]),
    ("mpeg4", ["-q:v", "4"]),
]


class RangeError(ValueError):
    pass


def parse_range(header: str | None, size: int) -> tuple[int, int] | None:
    """Parse a single-range ``Range`` header -> (start, end) inclusive, or None for the whole body.

    Raises RangeError when the range can't be satisfied (-> 416)."""
    if not header:
        return None
    h = header.strip()
    if not h.startswith("bytes="):
        return None  # unknown unit: ignore, send everything
    spec = h[6:].split(",")[0].strip()
    if "-" not in spec:
        raise RangeError(header)
    a, b = spec.split("-", 1)
    a, b = a.strip(), b.strip()
    try:
        if a == "":  # suffix: last N bytes
            n = int(b)
            if n <= 0:
                raise RangeError(header)
            if size == 0:
                raise RangeError(header)
            return max(0, size - n), size - 1
        start = int(a)
        end = int(b) if b else size - 1
    except ValueError as e:
        raise RangeError(header) from e
    if start >= size or start < 0 or end < start:
        raise RangeError(header)
    return start, min(end, size - 1)


class Viewer:
    def __init__(self, sources: list[Source], cache_dir: Path, ffmpeg: str = "ffmpeg"):
        self.sources = {s.name: s for s in sources}
        self.cache_dir = Path(cache_dir)
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        self.ffmpeg = ffmpeg
        self._timelines: OrderedDict[str, bytes] = OrderedDict()
        self._tl_lock = threading.Lock()
        self._job_locks: dict[str, threading.Lock] = {}
        self._jobs_lock = threading.Lock()
        self._h264_encoder: str | None = None
        self.has_ffmpeg = check_ffmpeg(ffmpeg)

    # ---- API ------------------------------------------------------------
    def source(self, name: str) -> Source:
        if name not in self.sources:
            raise KeyError(f"unknown source {name!r}")
        return self.sources[name]

    def api_sources(self) -> list[dict]:
        return [s.describe() | {"ffmpeg": self.has_ffmpeg} for s in self.sources.values()]

    def api_sessions(self, source: str, refresh: bool = False) -> list[dict]:
        src = self.source(source)
        out = [summarize(src, ref) for ref in src.sessions(refresh=refresh).values()]
        out.sort(key=lambda s: (s["start"] or "", s["session"]), reverse=True)
        return out

    def api_session(self, source: str, sid: str) -> dict:
        src = self.source(source)
        ref = src.session(sid)
        return summarize(src, ref) | {"segment_list": [segment_info(src, s) for s in ref.segments.values()]}

    def timeline_json(self, source: str, sid: str, seg: str) -> bytes:
        src = self.source(source)
        sref = src.segment(sid, seg)
        key = f"{source}\0{sref.prefix}\0{sref.files.get('inputs.parquet')}"
        with self._tl_lock:
            if key in self._timelines:
                self._timelines.move_to_end(key)
                return self._timelines[key]
        man = src.manifest(sref)
        tl = timeline_from_bytes(src.get_bytes(sref, "frames.parquet"), src.get_bytes(sref, "inputs.parquet"),
                                 src.get_bytes(sref, "focus.parquet"), man)
        body = json.dumps(tl, separators=(",", ":")).encode()
        with self._tl_lock:
            self._timelines[key] = body
            while len(self._timelines) > 256:
                self._timelines.popitem(last=False)
        return body

    # ---- video ------------------------------------------------------------
    def _job_lock(self, key: str) -> threading.Lock:
        with self._jobs_lock:
            return self._job_locks.setdefault(key, threading.Lock())

    def _local_video(self, src: Source, sref) -> Path:
        lp = src.local_path(sref, "video.mp4")
        if lp is not None:
            return lp
        h = hashlib.sha1(f"{src.store.url}|{sref.prefix}|{sref.files['video.mp4']}".encode()).hexdigest()[:20]
        dest = self.cache_dir / f"src-{h}.mp4"
        with self._job_lock(str(dest)):
            if not dest.exists():
                tmp = dest.with_suffix(".tmp")
                src.download(sref, "video.mp4", tmp)
                tmp.replace(dest)
        return dest

    def converted(self, source: str, sid: str, seg: str, mode: str) -> Path:
        src = self.source(source)
        sref = src.segment(sid, seg)
        h = hashlib.sha1(f"{src.store.url}|{sref.prefix}|{sref.files['video.mp4']}|{mode}".encode()).hexdigest()[:20]
        dest = self.cache_dir / f"{mode}-{h}.mp4"
        with self._job_lock(str(dest)):
            if dest.exists():
                return dest
            inp = self._local_video(src, sref)
            tmp = dest.with_name(dest.stem + ".tmp.mp4")
            t0 = time.monotonic()
            if mode == "remux":
                self._ffmpeg(["-i", str(inp), "-map", "0:v:0", "-c", "copy", "-tag:v", "hvc1",
                              "-movflags", "+faststart", str(tmp)])
            elif mode == "h264":
                self._transcode_h264(inp, tmp)
            else:
                raise ValueError(mode)
            tmp.replace(dest)
            log.info("%s %s/%s -> %s in %.1fs", mode, sid, seg, dest.name, time.monotonic() - t0)
        return dest

    def _ffmpeg(self, args: list[str]) -> None:
        cmd = [self.ffmpeg, "-hide_banner", "-v", "error", "-y", *args]
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode != 0:
            raise RuntimeError(f"ffmpeg failed ({r.returncode}): {r.stderr.strip()[-500:]}")

    def _transcode_h264(self, inp: Path, out: Path) -> None:
        cands = H264_ENCODERS
        if self._h264_encoder:
            cands = [c for c in H264_ENCODERS if c[0] == self._h264_encoder]
        errors = []
        for name, opts in cands:
            try:
                self._ffmpeg(["-i", str(inp), "-map", "0:v:0", "-an", "-c:v", name, *opts, "-pix_fmt", "yuv420p",
                              "-fps_mode", "passthrough", "-movflags", "+faststart", str(out)])
                if self._h264_encoder != name:
                    log.info("h264 transcodes use %s", name)
                self._h264_encoder = name
                return
            except RuntimeError as e:
                errors.append(f"{name}: {str(e)[-160:]}")
        raise RuntimeError("no usable H.264 encoder: " + " | ".join(errors))


def _static_root():
    return resources.files(__package__).joinpath("static")


class Handler(BaseHTTPRequestHandler):
    viewer: Viewer  # set on the subclass
    server_version = "gamecap-viewer/0.1"

    def log_message(self, fmt, *args):
        log.debug("%s " + fmt, self.address_string(), *args)

    # ---- helpers --------------------------------------------------------
    def _send(self, status: int, body: bytes, ctype: str, extra: dict | None = None):
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def _json(self, obj, status: int = 200):
        self._send(status, json.dumps(obj, separators=(",", ":")).encode(), "application/json",
                   {"Cache-Control": "no-store"})

    def _error(self, status: int, msg: str):
        self._json({"error": msg}, status)

    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        u = urlparse(self.path)
        q = {k: v[-1] for k, v in parse_qs(u.query).items()}
        try:
            if u.path in ("/", "/index.html"):
                return self._static("index.html")
            if u.path.startswith("/static/"):
                return self._static(u.path[len("/static/"):])
            if u.path == "/api/sources":
                return self._json(self.viewer.api_sources())
            if u.path == "/api/sessions":
                return self._json(self.viewer.api_sessions(q["source"], q.get("refresh") == "1"))
            if u.path == "/api/session":
                return self._json(self.viewer.api_session(q["source"], q["id"]))
            if u.path == "/api/timeline":
                body = self.viewer.timeline_json(q["source"], q["id"], q["seg"])
                return self._send(200, body, "application/json", {"Cache-Control": "max-age=3600"})
            if u.path == "/video":
                return self._video(q)
            return self._error(404, "not found")
        except KeyError as e:
            return self._error(404, f"missing or unknown: {e}")
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            return
        except Exception as e:  # noqa: BLE001
            log.exception("request failed: %s", self.path)
            try:
                return self._error(500, str(e))
            except OSError:
                return

    def _static(self, name: str):
        if "/" in name or "\\" in name or name.startswith("."):
            return self._error(404, "not found")
        f = _static_root().joinpath(name)
        if not f.is_file():
            return self._error(404, "not found")
        ctype = mimetypes.guess_type(name)[0] or "application/octet-stream"
        if ctype.startswith("text/") or ctype.endswith("javascript"):
            ctype += "; charset=utf-8"
        self._send(200, f.read_bytes(), ctype, {"Cache-Control": "no-cache"})

    def _video(self, q: dict):
        v = self.viewer
        src = v.source(q["source"])
        sref = src.segment(q["id"], q["seg"])
        mode = q.get("mode", "raw")
        if mode in ("remux", "h264"):
            path = v.converted(q["source"], q["id"], q["seg"], mode)
            size = path.stat().st_size

            def chunks(a, b):
                with open(path, "rb") as f:
                    f.seek(a)
                    left = b - a + 1
                    while left > 0:
                        d = f.read(min(256 * 1024, left))
                        if not d:
                            break
                        left -= len(d)
                        yield d
        elif mode == "raw":
            size = sref.files["video.mp4"]

            def chunks(a, b):
                return src.iter_range(sref, "video.mp4", a, b)
        else:
            return self._error(400, f"bad mode {mode!r}")

        try:
            rng = parse_range(self.headers.get("Range"), size)
        except RangeError:
            return self._send(416, b"", "text/plain", {"Content-Range": f"bytes */{size}"})
        start, end = rng if rng else (0, size - 1)
        self.send_response(HTTPStatus.PARTIAL_CONTENT if rng else HTTPStatus.OK)
        self.send_header("Content-Type", "video/mp4")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(end - start + 1 if size else 0))
        self.send_header("Cache-Control", "max-age=86400")
        if rng:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()
        if self.command == "HEAD" or size == 0:
            return
        for c in chunks(start, end):
            self.wfile.write(c)


def make_server(viewer: Viewer, host: str, port: int) -> ThreadingHTTPServer:
    handler = type("BoundHandler", (Handler,), {"viewer": viewer})
    srv = ThreadingHTTPServer((host, port), handler)
    srv.daemon_threads = True
    return srv


def default_cache_dir() -> Path:
    return Path(tempfile.gettempdir()) / "gamecap-viewer-cache"


def check_ffmpeg(ffmpeg: str = "ffmpeg") -> bool:
    return shutil.which(ffmpeg) is not None
