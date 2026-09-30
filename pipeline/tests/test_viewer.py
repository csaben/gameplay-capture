"""Viewer: Range parsing, per-frame timeline, and the HTTP API over a synthetic session."""

from __future__ import annotations

import json
import threading
import urllib.error
import urllib.request

import pytest

from gamecap_pipeline.viewer.server import RangeError, Viewer, make_server, parse_range
from gamecap_pipeline.viewer.sources import make_source
from gamecap_pipeline.viewer.timeline import compute_timeline

MS = 1_000_000


# ---------------------------------------------------------------- Range
@pytest.mark.parametrize("hdr,size,want", [
    (None, 100, None),
    ("bytes=0-9", 100, (0, 9)),
    ("bytes=10-", 100, (10, 99)),
    ("bytes=90-1000", 100, (90, 99)),
    ("bytes=-10", 100, (90, 99)),
    ("bytes=-1000", 100, (0, 99)),
    ("bytes=5-5, 10-20", 100, (5, 5)),  # only the first range is served
    ("items=0-5", 100, None),
])
def test_parse_range(hdr, size, want):
    assert parse_range(hdr, size) == want


@pytest.mark.parametrize("hdr,size", [("bytes=100-", 100), ("bytes=5-2", 100), ("bytes=x-3", 100),
                                      ("bytes=-0", 100), ("bytes=0-1", 0), ("bytes=abc", 100)])
def test_parse_range_unsatisfiable(hdr, size):
    with pytest.raises(RangeError):
        parse_range(hdr, size)


# ---------------------------------------------------------------- timeline
def cols(events):
    return {"t_ns": [e[0] for e in events], "device": [e[1] for e in events], "kind": [e[2] for e in events],
            "code": [e[3] for e in events], "value": [e[4] for e in events]}


def frames(n, t0=1000 * MS, tick=50 * MS):
    return {"frame_idx": list(range(n)), "tick_ns": [t0 + k * tick for k in range(n)],
            "capture_ns": [t0 + k * tick - MS for k in range(n)], "repeated": [k == 3 for k in range(n)]}


NO_FOCUS = {"t_ns": [], "focused": [], "game_id": []}


def test_timeline_windows_state_and_sums():
    t0 = 1000 * MS
    ev = [
        (t0 - 5 * MS, "keyboard", "key_down", 0x11, 1.0),  # before frame 0 -> folded into frame 0
        (t0 + 60 * MS, "mouse", "mouse_move", 0, 3.0),  # frame 1
        (t0 + 70 * MS, "mouse", "mouse_move", 0, 4.0),
        (t0 + 70 * MS, "mouse", "mouse_move", 1, -2.0),
        (t0 + 99 * MS, "mouse", "mouse_button", 0, 1.0),  # frame 1 (end exclusive at 100)
        (t0 + 100 * MS, "keyboard", "key_up", 0x11, 0.0),  # frame 2
        (t0 + 100 * MS, "mouse", "wheel", 0, -1.0),
        (t0 + 160 * MS, "mouse", "mouse_button", 0, 0.0),  # frame 3
        (t0 + 160 * MS, "keyboard", "key_down", 0xE048, 1.0),  # Up (extended)
        (t0 + 161 * MS, "keyboard", "key_down", 0xE02A, 1.0),  # fake shift: ignored
    ]
    tl = compute_timeline(frames(5), cols(ev), NO_FOCUS, {"rate_hz": 20, "latency_offset_ns": 0})
    assert tl["n"] == 5 and tl["rate_hz"] == 20 and not tl["has_gamepad"] and "axes" not in tl
    assert tl["keys"] == [[0x11], [0x11], [], [0xE048], [0xE048]]
    assert tl["pressed"][0] == ["W"] and tl["pressed"][1] == ["LMB"] and tl["pressed"][3] == ["Up"]
    assert tl["mb"] == [[], [0], [0], [], []]
    assert tl["dx"][1] == 7.0 and tl["dy"][1] == -2.0 and tl["dx"][0] == 0
    assert tl["wheel"][2] == -1.0
    assert tl["ev"] == [1, 4, 2, 3, 0]
    assert tl["repeated"] == [0, 0, 0, 1, 0]
    assert tl["focused"] == [1] * 5
    assert tl["key_names"] == {str(0x11): "W", str(0xE048): "Up"}


def test_timeline_latency_offset_shifts_windows():
    t0 = 1000 * MS
    ev = [(t0 + 40 * MS, "keyboard", "key_down", 0x1E, 1.0)]  # A at +40 ms
    base = compute_timeline(frames(3), cols(ev), NO_FOCUS, {"rate_hz": 20, "latency_offset_ns": 0})
    assert base["pressed"][0] == ["A"]
    # offset -35 ms: frame 0 window = [t0-35, t0+15), frame 1 = [t0+15, t0+65) -> A lands in frame 1
    shifted = compute_timeline(frames(3), cols(ev), NO_FOCUS, {"rate_hz": 20, "latency_offset_ns": -35 * MS})
    assert shifted["pressed"][0] == [] and shifted["pressed"][1] == ["A"]


def test_timeline_focus_and_gamepad():
    t0 = 1000 * MS
    ev = [
        (t0 + 10 * MS, "gamepad", "axis", 0, 0.5),
        (t0 + 20 * MS, "gamepad", "axis", 5, 1.0),
        (t0 + 60 * MS, "gamepad", "button", 0, 1.0),
        (t0 + 60 * MS, "gamepad", "axis", 0, -0.25),
        (t0 + 110 * MS, "gamepad", "button", 0, 0.0),
    ]
    focus = {"t_ns": [t0 + 120 * MS, t0], "focused": [False, True], "game_id": ["x", "g"]}  # unsorted on purpose
    tl = compute_timeline(frames(4), cols(ev), focus, {"rate_hz": 20})
    assert tl["has_gamepad"]
    assert tl["axes"][0][0] == 0.5 and tl["axes"][0][5] == 1.0
    assert tl["axes"][1][0] == -0.25
    assert tl["pad"] == [[], [0], [], []]
    assert tl["pressed"][1] == ["Pad A"]
    assert tl["focused"] == [1, 1, 0, 0]


def test_timeline_on_synthetic_segment(seg0):
    from gamecap_pipeline.viewer.timeline import timeline_from_bytes

    man = json.loads((seg0 / "manifest.json").read_text())
    tl = timeline_from_bytes((seg0 / "frames.parquet").read_bytes(), (seg0 / "inputs.parquet").read_bytes(),
                             (seg0 / "focus.parquet").read_bytes(), man)
    assert tl["n"] == man["frame_count"]
    assert tl["has_gamepad"] and len(tl["axes"]) == tl["n"]
    rate = man["rate_hz"]
    # South is held 22.0-22.2 s in the synthetic script; Start 28.0-28.4 s
    assert any(0 in tl["pad"][k] for k in range(21 * rate, 24 * rate))
    assert any(11 in tl["pad"][k] for k in range(27 * rate, 30 * rate))
    # the 48-51 s unfocused span shows up
    assert 0 in tl["focused"][47 * rate:52 * rate]


# ---------------------------------------------------------------- HTTP
@pytest.fixture(scope="module")
def server(synth_root, tmp_path_factory):
    src = make_source("synth", str(synth_root / "sessions"))
    viewer = Viewer([src], tmp_path_factory.mktemp("viewer-cache"))
    srv = make_server(viewer, "127.0.0.1", 0)
    th = threading.Thread(target=srv.serve_forever, daemon=True)
    th.start()
    yield f"http://127.0.0.1:{srv.server_address[1]}", synth_root
    srv.shutdown()
    srv.server_close()


def get(url, headers=None):
    req = urllib.request.Request(url, headers=headers or {})
    try:
        with urllib.request.urlopen(req) as r:
            return r.status, dict(r.headers), r.read()
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read()


def test_http_api(server):
    base, root = server
    st, _, body = get(base + "/api/sources")
    assert st == 200 and json.loads(body)[0]["name"] == "synth"
    st, _, body = get(base + "/api/sessions?source=synth")
    sessions = json.loads(body)
    assert [s["id"] for s in sessions] == ["sess-test"]
    assert sessions[0]["segments"] == 2 and sessions[0]["user"] == ""
    st, _, body = get(base + "/api/session?source=synth&id=sess-test")
    detail = json.loads(body)
    assert [s["name"] for s in detail["segment_list"]] == ["seg_000000", "seg_000001"]
    st, hdr, body = get(base + "/api/timeline?source=synth&id=sess-test&seg=seg_000001")
    assert st == 200 and json.loads(body)["n"] == detail["segment_list"][1]["manifest"]["frame_count"]
    assert get(base + "/api/session?source=synth&id=nope")[0] == 404
    assert get(base + "/api/timeline?source=synth&id=sess-test&seg=../seg_000000")[0] == 404
    assert get(base + "/api/sessions?source=missing")[0] == 404
    st, hdr, body = get(base + "/")
    assert st == 200 and b"gamecap" in body and hdr["Content-Type"].startswith("text/html")
    assert get(base + "/static/app.js")[0] == 200
    assert get(base + "/static/../server.py")[0] == 404


def test_http_video_ranges(server):
    base, root = server
    video = (root / "sessions" / "sess-test" / "seg_000000" / "video.mp4").read_bytes()
    url = base + "/video?source=synth&id=sess-test&seg=seg_000000"
    st, hdr, body = get(url)
    assert st == 200 and body == video and hdr["Accept-Ranges"] == "bytes"
    st, hdr, body = get(url, {"Range": "bytes=100-1099"})
    assert st == 206 and body == video[100:1100]
    assert hdr["Content-Range"] == f"bytes 100-1099/{len(video)}"
    st, hdr, body = get(url, {"Range": "bytes=-16"})
    assert st == 206 and body == video[-16:]
    st, hdr, _ = get(url, {"Range": f"bytes={len(video)}-"})
    assert st == 416 and hdr["Content-Range"] == f"bytes */{len(video)}"
    assert get(url + "&mode=bogus")[0] == 400


@pytest.mark.skipif(not __import__("shutil").which("ffmpeg"), reason="ffmpeg not on PATH")
def test_http_video_remux(server):
    base, _ = server
    url = base + "/video?source=synth&id=sess-test&seg=seg_000000&mode=remux"
    st, hdr, body = get(url, {"Range": "bytes=0-7"})
    assert st == 206 and len(body) == 8 and body[4:8] == b"ftyp"
    total = int(hdr["Content-Range"].split("/")[1])
    st, _, full = get(url)
    assert st == 200 and len(full) == total
    # faststart: moov before mdat
    assert full.find(b"moov") < full.find(b"mdat")
