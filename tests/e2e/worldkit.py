"""Scenario-level plumbing shared by the step definitions.

Everything here talks to the fake world in the vocabulary the feature files use
("a completed source torrent", "a target site with an unparseable template"), so
the step bodies stay about behaviour rather than about dict shapes.
"""

from __future__ import annotations

import time

import fakes
import torrentlib

MIB = 1024 * 1024


def add_torrent(
    state: fakes.State,
    *,
    name: str,
    size: int = 8 * MIB,
    save_path: str = "/data/source",
    progress: float = 1.0,
    state_name: str = "stalledUP",
    category: str = "",
    tags: str = "",
    check_ticks: int = 1,
    behaviour: str = "ok",
) -> tuple[str, bytes]:
    """Put a torrent in the fake downloader and return (hash, torrent bytes)."""
    blob, digest = torrentlib.make_torrent(name, size)
    state.torrents[digest] = {
        "hash": digest,
        "name": name,
        "save_path": save_path,
        "content_path": f"{save_path}/{name}",
        "total_size": size,
        "progress": progress,
        "state": state_name,
        "category": category,
        "tags": tags,
        "added_on": int(time.time()),
        "seeding_time": 3600,
        "uploaded": 1024,
        "ratio": 1.0,
        "ticks": 0,
        "check_ticks": check_ticks,
        "behaviour": behaviour,
    }
    return digest, blob


def offer_on_site(
    state: fakes.State,
    *,
    slug: str,
    torrent_id: int,
    name: str,
    size: int = 8 * MIB,
    sid: int | None = None,
    source_hash: str | None = None,
) -> str:
    """Publish a release on a fake tracker and expose it as an IYUU candidate.

    Returns the target info-hash. The candidate is registered under
    `source_hash` (the torrent already in the downloader), which is how IYUU
    answers: "for *this* hash, these sites have a release".
    """
    blob, digest = torrentlib.make_torrent(name, size)
    state.site_torrents.setdefault(slug, {})[torrent_id] = blob

    sid = sid if sid is not None else _sid_for(slug)
    if source_hash is not None:
        state.candidates.setdefault(source_hash, []).append(
            {"sid": sid, "torrent_id": torrent_id, "info_hash": digest}
        )
    return digest


def _sid_for(slug: str) -> int:
    return {"alpha": fakes.SID_A, "beta": fakes.SID_B, "gamma": fakes.SID_C}[slug]


def site_requests(state: fakes.State, slug: str) -> list[str]:
    """Tracker download requests recorded for one site."""
    return [
        line
        for line in state.request_log
        if f"/{slug}/" in line and "download.php" in line
    ]


def catalogue_requests(state: fakes.State) -> int:
    return sum(1 for line in state.request_log if "/reseed/sites/index" in line)


def report_existing_requests(state: fakes.State) -> int:
    return sum(1 for line in state.request_log if "/reseed/sites/reportExisting" in line)


def failed_adds(state: fakes.State) -> list[dict]:
    return [entry for entry in state.add_log if entry.get("rejected") or entry.get("duplicate")]
