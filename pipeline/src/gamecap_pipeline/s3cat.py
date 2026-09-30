"""Stream one S3 object to stdout: `python -m gamecap_pipeline.s3cat s3://bucket/key`.

Used as a WebDataset `pipe:` source so shards stream straight from Garage/R2
without the aws CLI.  Connection settings as in `store.py` (env vars).
"""

from __future__ import annotations

import sys
from urllib.parse import urlparse

from .store import s3_client


def main(argv: list[str] | None = None) -> int:
    argv = argv if argv is not None else sys.argv[1:]
    if len(argv) != 1 or not argv[0].startswith("s3://"):
        print("usage: python -m gamecap_pipeline.s3cat s3://bucket/key", file=sys.stderr)
        return 2
    u = urlparse(argv[0])
    body = s3_client().get_object(Bucket=u.netloc, Key=u.path.lstrip("/"))["Body"]
    out = sys.stdout.buffer
    for chunk in iter(lambda: body.read(1 << 20), b""):
        out.write(chunk)
    out.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
