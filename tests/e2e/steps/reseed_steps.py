"""Steps for features/reseed.feature — the discovery and verification pipeline.

The `Given 下载器里有一个已完成做种的种子` / `IYUU 在 N 个站点上…` / passkey setup
steps live in common_steps.py because more than one feature needs them.
"""

from __future__ import annotations

import time

from pytest_bdd import given, parsers, scenarios, then, when

import fakes
import worldkit

scenarios("../features/reseed.feature")

SLUG_SID = {"alpha": fakes.SID_A, "beta": fakes.SID_B, "gamma": fakes.SID_C}


def _run(app, timeout=120):
    app.start_run()
    app.progress = app.wait_run_done(timeout)
    return app.progress


def _stats(app):
    return app.progress.get("stats", {})


@given("其中一个目标种子已经在下载器里")
def one_target_present(app):
    slug, digest = next(iter(app.offered.items()))
    app.present_target = digest
    app.fake.torrents[digest] = {
        "hash": digest,
        "name": f"already.there.{slug}",
        "save_path": "/data/source",
        "content_path": None,
        "total_size": 8 * worldkit.MIB,
        "progress": 1.0,
        "state": "stalledUP",
        "category": "",
        "tags": "",
        "added_on": int(time.time()),
        "seeding_time": 1,
        "uploaded": 1,
        "ratio": 0.1,
        "ticks": 0,
        "check_ticks": 1,
        "behaviour": "ok",
    }
    # Only the *other* site's release should need adding.
    app.offered = {k: v for k, v in app.offered.items() if k != slug}


@given("目标种子加入后校验结果是数据不完整")
def target_incomplete(app):
    for digest in app.offered.values():
        app.fake.verify_behaviour[digest] = "incomplete"


@given("目标种子加入后下载器报告文件缺失")
def target_missing(app):
    for digest in app.offered.values():
        app.fake.verify_behaviour[digest] = "missing"


@when("触发一次辅种任务")
def run_discovery(app):
    _run(app)


@then("任务应正常完成")
def run_completed(app):
    assert app.progress["phase"] == "done", app.progress


@then(parsers.parse("任务统计应为新增 {added:d}、失败 {failed:d}"))
def stats_added_failed(app, added, failed):
    stats = _stats(app)
    assert stats["added"] == added, stats
    failing = stats["fetch_failed"] + stats["add_failed"]
    assert failing == failed, stats


@then(parsers.parse("任务统计应为新增 {added:d}、已存在 {existed:d}"))
def stats_added_existed(app, added, existed):
    stats = _stats(app)
    assert stats["added"] == added, stats
    assert stats["existed"] == existed, stats


@then(parsers.parse("任务统计里的获取失败数应为 {count:d}"))
def stats_fetch_failed(app, count):
    assert _stats(app)["fetch_failed"] == count, _stats(app)


@then(parsers.parse("任务统计里的查询数应为 {count:d}"))
def stats_queried(app, count):
    assert _stats(app)["queried"] == count, _stats(app)


@given("已为全部三个站点配置了 passkey")
def credentials_for_all_sites(app):
    """Target every site so the run gets past setup and actually scans.

    Without this the run fails during engine construction ("还没有添加任何站点")
    and never reaches the source-filter phase this scenario is about.
    """
    for slug, sid in SLUG_SID.items():
        app.add_site_credentials(sid, app.fake.site_passkeys[slug])


@then("任务应跳过它而不是去查询 IYUU")
def skipped_without_query(app):
    stats = _stats(app)
    assert stats["scanned"] == 1, stats
    assert stats["skipped"] == 1, stats
    assert stats["queried"] == 0, stats
    # Nothing should have been offered to IYUU at all: the filter runs before the
    # network phase, so this is also a statement about quota.
    assert not [r for r in app.fake.request_log if "/reseed/index/index" in r], (
        app.fake.request_log
    )


@then(parsers.parse("两条辅种记录最终都应进入 seeding 状态"))
def attempts_seed(app):
    items = app.wait_attempt_state(2, "seeding")
    assert len(items) >= 2, items
    assert all(i["state"] == "seeding" for i in items)


@then(parsers.parse("该辅种记录应进入 {state} 状态"))
def attempt_in_state(app, state):
    app.wait_attempt_state(1, state)


@then(parsers.parse("下载器里应确实多了这两个目标种子"))
def downloader_has_targets(app):
    present = {t["hash"] for t in app.torrents()["items"]}
    for slug, digest in app.offered.items():
        assert digest in present, f"{slug}'s {digest} missing from downloader: {present}"


@then(parsers.parse("每个目标站点都应收到 {count:d} 次下载请求"))
def each_site_hit(app, count):
    for slug in app.offered:
        hits = worldkit.site_requests(app.fake, slug)
        assert len(hits) == count, f"{slug}: {hits}"


@then("下载请求应带上该站点的 passkey")
def requests_carry_passkey(app):
    for slug in app.offered:
        expected = f"passkey={app.fake.site_passkeys[slug]}"
        for line in worldkit.site_requests(app.fake, slug):
            assert expected in line, f"{slug}: {line}"


@then("新增的种子 hash 应与 IYUU 声明的 info_hash 完全一致")
def hashes_match(app):
    present = {t["hash"] for t in app.torrents()["items"]}
    for slug, digest in app.offered.items():
        assert digest in present, f"{slug}: expected {digest}, present {present}"


@then(parsers.parse("下载器收到的添加请求总数应为 {count:d}"))
def add_count(app, count):
    accepted = [e for e in app.fake.add_log if not e.get("duplicate")]
    assert len(accepted) == count, app.fake.add_log


@then("下载器不应收到任何添加请求")
def no_adds(app):
    accepted = [e for e in app.fake.add_log if not e.get("duplicate")]
    assert accepted == [], app.fake.add_log


@then("记录里应说明是保存路径或资源不匹配")
def failure_message_explains(app):
    items = app.attempts("verify_failed")
    assert items, app.attempts()
    message = items[0].get("message") or ""
    assert "保存路径" in message or "不匹配" in message, message


@then("任务日志里应说明该站点的种子获取失败")
def log_reports_fetch_failure(app):
    # A fetch failure never produces an attempt row — there is no torrent to
    # track — so the operator's only evidence is the run log. It has to name the
    # failure, otherwise "0 added" is indistinguishable from "nothing found".
    log = " ".join((app.get("/api/runs/progress") or {}).get("log") or [])
    assert "获取种子失败" in log, log
