"""Deletion list: segments that must never appear in (or must be removed from) shards.

Sources (combined):
  * a local text file, one entry per line (`#` comments allowed)
  * an S3/local prefix: every object under it is read as such a text file,
    e.g. s3://gamecap/deletions/  (the ingest-api's `deletions` table can
    later be exported there, or queried directly by adding a source here)

An entry matches a segment when it equals
  * the segment id            `<session_id>/seg_000042`
  * the session id            `<session_id>`
  * a '/'-bounded prefix of the segment's storage key relative to the raw
    root, e.g. `<user_id>` or `<user_id>/<session_id>` for the R2 layout
    `raw/<user_id>/<session_id>/seg_<n>/` (this is how DELETE /me/data
    removes everything of one user).
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

from .store import open_store


@dataclass
class Deletions:
    entries: set[str] = field(default_factory=set)

    @classmethod
    def load(cls, sources: list[str] | None, endpoint_url: str | None = None) -> "Deletions":
        d = cls()
        for src in sources or []:
            if src.startswith("s3://") or Path(src).is_dir():
                st = open_store(src, endpoint_url)
                for o in st.list(""):
                    d._add_text(st.get_bytes(o.key).decode())
            elif Path(src).exists():
                d._add_text(Path(src).read_text())
            else:
                raise FileNotFoundError(f"deletions source not found: {src}")
        return d

    def _add_text(self, text: str) -> None:
        for line in text.splitlines():
            line = line.split("#", 1)[0].strip().strip("/")
            if line:
                self.entries.add(line)

    def matches(self, segment_id: str, source_key: str = "") -> bool:
        if not self.entries:
            return False
        if segment_id in self.entries:
            return True
        session = segment_id.rsplit("/", 1)[0]
        if session in self.entries:
            return True
        parts = source_key.strip("/").split("/")
        for i in range(1, len(parts) + 1):
            if "/".join(parts[:i]) in self.entries:
                return True
        return False

    def __len__(self) -> int:
        return len(self.entries)
