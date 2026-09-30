"""Session catalog over one or more stores (local sessions dir, or S3 raw/ prefix).

A segment is any ``.../<session>/seg_NNNNNN/manifest.json`` under the store root
(``.partial`` / ``.broken`` folders don't match). The path part before the
session is the user id (``raw/<user>/<session>/...``); for a local sessions dir
there is none. Listings are cached for ``list_ttl`` seconds, manifests forever
(segments are immutable once finished).
"""

from __future__ import annotations

import json
import re
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterator

from ..store import LocalStore, S3Store, Store

SEG_RE = re.compile(r"^seg_\d{6}$")
SID_TIME_RE = re.compile(r"^(\d{8}T\d{6}Z)")


@dataclass
class SegmentRef:
    name: str  # seg_000000
    prefix: str  # store key prefix of the segment folder, ending in '/'
    files: dict[str, int] = field(default_factory=dict)  # file name -> size


@dataclass
class SessionRef:
    id: str  # "<user>/<session>" or "<session>"
    user: str
    session: str
    segments: dict[str, SegmentRef] = field(default_factory=dict)


def session_start(session: str) -> str | None:
    m = SID_TIME_RE.match(session)
    if not m:
        return None
    try:
        return datetime.strptime(m.group(1), "%Y%m%dT%H%M%SZ").replace(tzinfo=timezone.utc).isoformat()
    except ValueError:
        return None


class Source:
    def __init__(self, name: str, store: Store, list_ttl: float = 30.0):
        self.name = name
        self.store = store
        self.list_ttl = list_ttl
        self._lock = threading.Lock()
        self._listed_at = 0.0
        self._sessions: dict[str, SessionRef] = {}
        self._manifests: dict[tuple[str, int], dict] = {}

    @property
    def kind(self) -> str:
        return "s3" if isinstance(self.store, S3Store) else "local"

    def describe(self) -> dict:
        return {"name": self.name, "kind": self.kind, "url": self.store.url}

    # ---- listing -------------------------------------------------------
    def sessions(self, refresh: bool = False) -> dict[str, SessionRef]:
        with self._lock:
            if refresh or time.monotonic() - self._listed_at > self.list_ttl:
                self._sessions = self._scan()
                self._listed_at = time.monotonic()
            return self._sessions

    def _scan(self) -> dict[str, SessionRef]:
        found: dict[str, SessionRef] = {}
        for obj in self.store.list(""):
            parts = obj.key.split("/")
            if len(parts) < 3 or not SEG_RE.match(parts[-2]):
                continue
            session = parts[-3]
            user = parts[-4] if len(parts) >= 4 else ""
            sid = f"{user}/{session}" if user else session
            ref = found.setdefault(sid, SessionRef(sid, user, session))
            seg = ref.segments.setdefault(parts[-2], SegmentRef(parts[-2], "/".join(parts[:-1]) + "/"))
            seg.files[parts[-1]] = obj.size
        # Only segments that have a manifest (i.e. are finished) count.
        for ref in found.values():
            ref.segments = {k: v for k, v in sorted(ref.segments.items()) if "manifest.json" in v.files}
        return {k: v for k, v in found.items() if v.segments}

    def session(self, sid: str) -> SessionRef:
        ref = self.sessions().get(sid) or self.sessions(refresh=True).get(sid)
        if ref is None:
            raise KeyError(f"unknown session {sid!r} in source {self.name!r}")
        return ref

    def segment(self, sid: str, seg: str) -> SegmentRef:
        ref = self.session(sid)
        if seg not in ref.segments:
            raise KeyError(f"unknown segment {seg!r} in {sid!r}")
        return ref.segments[seg]

    # ---- objects -------------------------------------------------------
    def manifest(self, seg: SegmentRef) -> dict:
        key = (seg.prefix, seg.files.get("manifest.json", 0))
        m = self._manifests.get(key)
        if m is None:
            m = json.loads(self.store.get_bytes(seg.prefix + "manifest.json"))
            self._manifests[key] = m
        return m

    def manifests(self, segs: list[SegmentRef]) -> list[dict]:
        missing = [s for s in segs if (s.prefix, s.files.get("manifest.json", 0)) not in self._manifests]
        if len(missing) > 1:
            with ThreadPoolExecutor(8) as ex:
                list(ex.map(self.manifest, missing))
        return [self.manifest(s) for s in segs]

    def get_bytes(self, seg: SegmentRef, name: str) -> bytes | None:
        if name not in seg.files:
            return None
        return self.store.get_bytes(seg.prefix + name)

    def local_path(self, seg: SegmentRef, name: str) -> Path | None:
        return self.store.local_path(seg.prefix + name)

    def iter_range(self, seg: SegmentRef, name: str, start: int, end: int, chunk: int = 256 * 1024) -> Iterator[bytes]:
        """Bytes [start, end] (inclusive) of one object."""
        key = seg.prefix + name
        lp = self.store.local_path(key)
        if lp is not None:
            with open(lp, "rb") as f:
                f.seek(start)
                left = end - start + 1
                while left > 0:
                    b = f.read(min(chunk, left))
                    if not b:
                        break
                    left -= len(b)
                    yield b
            return
        assert isinstance(self.store, S3Store)
        resp = self.store.client.get_object(Bucket=self.store.bucket, Key=self.store._k(key),
                                            Range=f"bytes={start}-{end}")
        yield from resp["Body"].iter_chunks(chunk)

    def download(self, seg: SegmentRef, name: str, dest: Path) -> None:
        self.store.download(seg.prefix + name, dest)


def summarize(src: Source, ref: SessionRef) -> dict:
    segs = list(ref.segments.values())
    mans = src.manifests(segs)
    m0 = mans[0]
    rate = m0.get("rate_hz") or 20
    frames = sum(int(m.get("frame_count", 0)) for m in mans)
    dropped = sum(int(m.get("dropped_frames", 0)) for m in mans)
    return {
        "id": ref.id,
        "user": ref.user,
        "session": ref.session,
        "game_id": m0.get("game_id", ""),
        "start": session_start(ref.session),
        "segments": len(segs),
        "frames": frames,
        "dropped": dropped,
        "duration_s": round((frames + dropped) / rate, 2),
        "bytes": sum(sum(s.files.values()) for s in segs),
        "rate_hz": rate,
        "width": m0.get("width"),
        "height": m0.get("height"),
    }


def segment_info(src: Source, seg: SegmentRef) -> dict:
    m = src.manifest(seg)
    return {"name": seg.name, "files": seg.files, "manifest": m}


def make_source(name: str, url: str, endpoint: str | None = None, region: str | None = None,
                access_key: str | None = None, secret_key: str | None = None, list_ttl: float = 30.0) -> Source:
    if url.startswith("s3://"):
        from urllib.parse import urlparse

        from ..store import s3_client

        u = urlparse(url)
        client = s3_client(endpoint, region, access_key, secret_key)
        store: Store = S3Store(u.netloc, u.path.lstrip("/"), client=client)
    else:
        if url.startswith("file://"):
            from urllib.parse import urlparse

            url = urlparse(url).path
        store = LocalStore(url)
    return Source(name, store, list_ttl)
