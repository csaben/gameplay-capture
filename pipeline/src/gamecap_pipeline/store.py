"""Object store abstraction: a local directory or an S3-compatible bucket (Garage, R2).

Stores are addressed by URL:
  * ``/some/dir`` or ``file:///some/dir``  -> LocalStore
  * ``s3://bucket`` or ``s3://bucket/prefix`` -> S3Store

S3 connection settings come from the environment (or explicit kwargs):
  GAMECAP_S3_ENDPOINT (falls back to AWS_ENDPOINT_URL), AWS_ACCESS_KEY_ID,
  AWS_SECRET_ACCESS_KEY, AWS_REGION / AWS_DEFAULT_REGION (default "garage";
  use "auto" for R2).  Garage needs path-style addressing, which is the default
  here (GAMECAP_S3_ADDRESSING=virtual to change).

Keys are always '/'-separated and relative to the store root.
"""

from __future__ import annotations

import os
import shutil
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator
from urllib.parse import urlparse


@dataclass
class ObjectInfo:
    key: str
    size: int


class Store:
    url: str

    def list(self, prefix: str = "") -> Iterator[ObjectInfo]:
        raise NotImplementedError

    def get_bytes(self, key: str) -> bytes:
        raise NotImplementedError

    def download(self, key: str, dest: Path) -> None:
        raise NotImplementedError

    def put_bytes(self, key: str, data: bytes) -> None:
        raise NotImplementedError

    def put_file(self, src: Path, key: str) -> None:
        raise NotImplementedError

    def exists(self, key: str) -> bool:
        raise NotImplementedError

    def delete(self, key: str) -> None:
        raise NotImplementedError

    def local_path(self, key: str) -> Path | None:
        """Direct filesystem path if the store is local (lets us skip copies)."""
        return None

    def uri(self, key: str) -> str:
        """URL of one object (local path for LocalStore, s3://... for S3)."""
        raise NotImplementedError


class LocalStore(Store):
    def __init__(self, root: str | Path):
        self.root = Path(root).resolve()
        self.url = str(self.root)

    def _p(self, key: str) -> Path:
        return self.root / key

    def list(self, prefix: str = "") -> Iterator[ObjectInfo]:
        base = self.root
        if not base.exists():
            return
        # Walk only the directory part of the prefix to keep listings cheap.
        pdir = prefix.rsplit("/", 1)[0] if "/" in prefix else ""
        start = base / pdir
        if not start.exists():
            return
        for dirpath, _dirs, files in os.walk(start):
            for f in files:
                p = Path(dirpath) / f
                key = p.relative_to(base).as_posix()
                if key.startswith(prefix):
                    yield ObjectInfo(key, p.stat().st_size)

    def get_bytes(self, key: str) -> bytes:
        return self._p(key).read_bytes()

    def download(self, key: str, dest: Path) -> None:
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(self._p(key), dest)

    def put_bytes(self, key: str, data: bytes) -> None:
        p = self._p(key)
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_name(p.name + ".tmp")
        tmp.write_bytes(data)
        tmp.replace(p)

    def put_file(self, src: Path, key: str) -> None:
        p = self._p(key)
        if Path(src).resolve() == p.resolve():
            return
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_name(p.name + ".tmp")
        shutil.copyfile(src, tmp)
        tmp.replace(p)

    def exists(self, key: str) -> bool:
        return self._p(key).exists()

    def delete(self, key: str) -> None:
        self._p(key).unlink(missing_ok=True)

    def local_path(self, key: str) -> Path | None:
        return self._p(key)

    def uri(self, key: str) -> str:
        return str(self._p(key))


def s3_client(endpoint_url: str | None = None, region: str | None = None,
              access_key: str | None = None, secret_key: str | None = None):
    import boto3
    from botocore.config import Config

    endpoint_url = endpoint_url or os.environ.get("GAMECAP_S3_ENDPOINT") or os.environ.get("AWS_ENDPOINT_URL")
    region = region or os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION") or "garage"
    addressing = os.environ.get("GAMECAP_S3_ADDRESSING", "path")
    cfg = Config(
        s3={"addressing_style": addressing},
        retries={"max_attempts": 8, "mode": "adaptive"},
        # R2 and Garage both reject some of the newer default checksum headers.
        request_checksum_calculation="when_required",
        response_checksum_validation="when_required",
    )
    creds = {"aws_access_key_id": access_key, "aws_secret_access_key": secret_key} if access_key else {}
    return boto3.client("s3", endpoint_url=endpoint_url, region_name=region, config=cfg, **creds)


class S3Store(Store):
    def __init__(self, bucket: str, prefix: str = "", endpoint_url: str | None = None, client=None):
        self.bucket = bucket
        self.prefix = prefix.strip("/")
        self.client = client or s3_client(endpoint_url)
        self.url = f"s3://{bucket}/{self.prefix}".rstrip("/")

    def _k(self, key: str) -> str:
        return f"{self.prefix}/{key}" if self.prefix else key

    def _rel(self, full: str) -> str:
        return full[len(self.prefix) + 1:] if self.prefix else full

    def list(self, prefix: str = "") -> Iterator[ObjectInfo]:
        paginator = self.client.get_paginator("list_objects_v2")
        for page in paginator.paginate(Bucket=self.bucket, Prefix=self._k(prefix)):
            for obj in page.get("Contents", []):
                yield ObjectInfo(self._rel(obj["Key"]), int(obj["Size"]))

    def get_bytes(self, key: str) -> bytes:
        return self.client.get_object(Bucket=self.bucket, Key=self._k(key))["Body"].read()

    def download(self, key: str, dest: Path) -> None:
        dest.parent.mkdir(parents=True, exist_ok=True)
        self.client.download_file(self.bucket, self._k(key), str(dest))

    def put_bytes(self, key: str, data: bytes) -> None:
        self.client.put_object(Bucket=self.bucket, Key=self._k(key), Body=data)

    def put_file(self, src: Path, key: str) -> None:
        from boto3.s3.transfer import TransferConfig

        cfg = TransferConfig(multipart_threshold=64 * 1024 * 1024, multipart_chunksize=64 * 1024 * 1024)
        self.client.upload_file(str(src), self.bucket, self._k(key), Config=cfg)

    def exists(self, key: str) -> bool:
        from botocore.exceptions import ClientError

        try:
            self.client.head_object(Bucket=self.bucket, Key=self._k(key))
            return True
        except ClientError as e:
            if e.response.get("Error", {}).get("Code") in ("404", "NoSuchKey", "NotFound"):
                return False
            raise

    def delete(self, key: str) -> None:
        self.client.delete_object(Bucket=self.bucket, Key=self._k(key))

    def uri(self, key: str) -> str:
        return f"s3://{self.bucket}/{self._k(key)}"


def open_store(url: str, endpoint_url: str | None = None) -> Store:
    if url.startswith("s3://"):
        u = urlparse(url)
        return S3Store(u.netloc, u.path.lstrip("/"), endpoint_url=endpoint_url)
    if url.startswith("file://"):
        url = urlparse(url).path
    return LocalStore(url)
