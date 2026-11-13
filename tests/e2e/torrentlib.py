"""Minimal bencode encoder + deterministic .torrent generator.

The e2e suite needs real torrent bytes: the engine hashes the `info` dictionary
exactly as it appears in the file, and refuses any download whose hash does not
match what IYUU claimed. Hand-rolled fake bytes would never pass that gate, so
we build genuine v1 torrents here and read the hash back out of them the same
way the Rust side does.
"""

from __future__ import annotations

import hashlib


def _bstr(raw: bytes) -> bytes:
    return b"%d:%s" % (len(raw), raw)


def _bint(value: int) -> bytes:
    return b"i%de" % value


def encode_dict(items: list[tuple[bytes, bytes]]) -> bytes:
    """Encode pre-encoded (key, value) pairs, sorting keys as bencode requires."""
    body = b"".join(_bstr(k) + v for k, v in sorted(items, key=lambda kv: kv[0]))
    return b"d" + body + b"e"


def make_torrent(
    name: str,
    size: int,
    piece_length: int = 262_144,
    announce: str = "http://tracker.invalid/announce",
) -> tuple[bytes, str]:
    """Build a valid single-file v1 torrent.

    `name` doubles as the uniqueness salt: two sites carrying "the same release"
    in reality produce different `info` dicts (different name/encoding/creation
    metadata) and therefore different hashes, which is exactly the situation a
    cross-seed has to handle.
    """
    pieces = max(1, (size + piece_length - 1) // piece_length)
    blob = bytes(((i * 37) + len(name)) % 256 for i in range(20 * pieces))

    info = encode_dict(
        [
            (b"length", _bint(size)),
            (b"name", _bstr(name.encode())),
            (b"piece length", _bint(piece_length)),
            (b"pieces", _bstr(blob)),
        ]
    )
    torrent = encode_dict(
        [
            (b"announce", _bstr(announce.encode())),
            (b"info", info),
        ]
    )
    return torrent, hashlib.sha1(info).hexdigest()


def info_hash(torrent: bytes) -> str:
    """Recompute the hash from the bytes, the way the Rust decoder does."""
    start = torrent.index(b"4:info") + len(b"4:info")
    depth = 0
    pos = start
    while pos < len(torrent):
        byte = torrent[pos : pos + 1]
        if byte == b"d" or byte == b"l":
            depth += 1
        elif byte == b"e":
            if depth == 0:
                break
            depth -= 1
        elif byte.isdigit():
            # skip a byte string: <len>:<bytes>
            colon = torrent.index(b":", pos)
            length = int(torrent[pos:colon])
            pos = colon + 1 + length
            continue
        elif byte == b"i":
            pos = torrent.index(b"e", pos) + 1
            continue
        pos += 1
    return hashlib.sha1(torrent[start:pos]).hexdigest()
