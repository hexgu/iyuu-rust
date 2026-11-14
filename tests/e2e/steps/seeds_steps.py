"""Steps for features/seeds.feature — the 种子管理 (main + aux) view."""

from __future__ import annotations

import time

from pytest_bdd import given, parsers, scenarios, then, when

import fakes
import worldkit

scenarios("../features/seeds.feature")

SOURCE_NAME = "Demo.Movie.2026.2160p.WEB-DL"


def _view(app) -> dict:
    assert getattr(app, "seeds", None) is not None, "先读取种子管理视图"
    return app.seeds


def _group(app) -> dict:
    items = _view(app)["items"]
    assert items, f"没有种子组：{_view(app)}"
    return items[0]


def _manual_torrent(app, *, name: str, size: int, tags: str = "") -> str:
    """把一个种子直接放进 fake 下载器。

    这里不需要真的 .torrent 字节：视图读的是 `list_torrents` 的结果，识别用的是
    名称与大小。用一个固定的假 hash 就足以构造「同数据的另一份副本」或「同名不同
    大小」这两种局面。
    """
    digest = f"{(len(app.fake.torrents) + 1):040x}"
    app.fake.torrents[digest] = {
        "hash": digest,
        "name": name,
        "save_path": "/data/source",
        "content_path": f"/data/source/{name}",
        "total_size": size,
        "progress": 1.0,
        "state": "stalledUP",
        "category": "movies",
        "tags": tags,
        "added_on": int(time.time()) + 60,
        "seeding_time": 10,
        "uploaded": 0,
        "ratio": 0.0,
        "ticks": 0,
        "check_ticks": 1,
        "behaviour": "ok",
    }
    return digest


@when("在种子管理场景里跑一次辅种任务")
def run_reseed(app):
    app.start_run()
    app.progress = app.wait_run_done(120)


@given("目标种子校验时会报告数据不完整")
def target_verifies_incomplete(app):
    for digest in app.offered.values():
        app.fake.verify_behaviour[digest] = "incomplete"


@when("读取种子管理视图")
def read_view(app):
    status, payload = app.request("GET", "/api/seeds")
    assert status == 200, payload
    app.seeds = payload


@when(parsers.parse('用站点名搜索种子管理视图'))
def search_by_site(app):
    status, payload = app.request("GET", "/api/seeds?q=alpha")
    assert status == 200, payload
    app.seeds = payload


@when("只读取有失败的组")
def read_failed_only(app):
    status, payload = app.request("GET", "/api/seeds?filter=has_failure")
    assert status == 200, payload
    app.seeds = payload


@when("删除下载器里的主种子")
def drop_source(app):
    app.fake.torrents.pop(app.source_hash, None)


@when("下载器里出现同一份数据的另一个副本")
def add_duplicate(app):
    _manual_torrent(app, name=SOURCE_NAME, size=8 * worldkit.MIB)


@when("下载器里出现一个同名但大小不同的种子")
def add_other_size(app):
    # 带辅种标签才会单独成组 —— 否则它只是一条不相干的下载器种子。
    _manual_torrent(app, name=SOURCE_NAME, size=16 * worldkit.MIB, tags="iyuu-reseed")


@when("删除该组的第一条辅种子记录")
def delete_first_aux(app):
    aux = _group(app)["aux"]
    assert aux, "这一组没有辅种子记录"
    status, payload = app.request("DELETE", f"/api/attempts/{aux[0]['attempt_id']}")
    assert status == 200, payload
    # 删的是一条记录，不是下载器里的种子：种子仍在，组靠下载器快照重新算出来。
    app.deleted_attempt = aux[0]["attempt_id"]
    app.request("GET", "/api/seeds")
    _, payload = app.request("GET", "/api/seeds")
    app.seeds = payload


@then("应只有 1 个种子组")
def exactly_one_group(app):
    assert _view(app)["total"] == 1, _view(app)


@then(parsers.parse("应只有 {count:d} 个种子组"))
def exactly_count_groups(app, count):
    assert _view(app)["total"] == count, _view(app)


@then(parsers.parse("应共有 {count:d} 个种子组"))
def total_groups(app, count):
    assert _view(app)["total"] == count, _view(app)


@then("该组的主种子应是下载器里的源种子")
def main_is_source(app):
    main = _group(app)["main"]
    assert main["hash"] == app.source_hash, (main, app.source_hash)
    assert main["present"] is True, main


@then(parsers.parse("该组应有 {count:d} 条辅种子记录"))
def aux_count(app, count):
    assert len(_group(app)["aux"]) == count, _group(app)


@then("该组的辅种子应全部校验通过并在做种")
def aux_all_seeding(app):
    for aux in _group(app)["aux"]:
        assert aux["state"] == "seeding", aux
        assert aux["in_downloader"] is True, aux
        assert aux["live"] is not None and aux["live"]["phase"] == "complete", aux


@then(parsers.parse("该组状态应为 {state}"))
def group_state(app, state):
    assert _group(app)["state"] == state, _group(app)


@then(parsers.parse("该组的失败辅种数应为 {count:d}"))
def failed_count(app, count):
    assert _group(app)["counts"]["failed"] == count, _group(app)


@then("主种子应标记为不在下载器")
def main_missing(app):
    main = _group(app)["main"]
    assert main["present"] is False, main
    assert main["hash"] == app.source_hash, main


@then("该组的名称应来自记录快照")
def name_from_snapshot(app):
    # 主种子已经不在下载器里了，名称只能来自辅种记录里保存的 source_name。
    assert _group(app)["name"] == SOURCE_NAME, _group(app)


@then(parsers.parse("该组应报告 {count:d} 个同数据副本"))
def duplicates(app, count):
    assert len(_group(app)["duplicates"]) == count, _group(app)


@then(parsers.parse("搜索应命中这 {count:d} 组"))
def search_hit(app, count):
    assert _view(app)["total"] == count, _view(app)


@then(parsers.parse("顶部统计的辅种总数应为 {count:d}"))
def header_total(app, count):
    assert _view(app)["counts"]["aux_total"] == count, _view(app)["counts"]


@then("该组应只剩 1 条辅种子记录")
def aux_left(app):
    assert len(_group(app)["aux"]) == 1, _group(app)
    assert all(a["attempt_id"] != app.deleted_attempt for a in _group(app)["aux"])
