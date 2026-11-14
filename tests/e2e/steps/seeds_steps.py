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
        "downloaded": 0,
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


@given("下载器里还有一个位于主种子目录里的单集种子")
def add_nested_torrent(app):
    # 季包目录里的单集：路径是主种子的子路径，但它是另一个资源（不同名称/大小）。
    digest = f"{len(app.fake.torrents) + 7777:040x}"
    app.fake.torrents[digest] = {
        "hash": digest,
        "name": f"[中文名].{SOURCE_NAME}.E01",
        "save_path": f"/data/source/{SOURCE_NAME}",
        "content_path": f"/data/source/{SOURCE_NAME}/E01.mkv",
        "total_size": 2 * worldkit.MIB,
        "progress": 1.0,
        "state": "stalledUP",
        "category": "movies",
        "tags": "",
        "added_on": int(time.time()),
        "seeding_time": 10,
        "uploaded": 0,
        "downloaded": 0,
        "ratio": 0.0,
        "ticks": 0,
        "check_ticks": 1,
        "behaviour": "ok",
    }


@given("下载器里还有一份同数据、不同标题、没有标签的副本")
def add_untagged_copy(app):
    # 同一份数据、站点改过标题、没有任何标签和记录：识别只能靠内容路径。
    digest = f"{len(app.fake.torrents) + 4242:040x}"
    app.fake.torrents[digest] = {
        "hash": digest,
        "name": f"[中文名].{SOURCE_NAME}",
        "save_path": "/data/source",
        "content_path": f"/data/source/{SOURCE_NAME}",
        "total_size": 8 * worldkit.MIB,
        "progress": 1.0,
        "state": "stalledUP",
        "category": "movies",
        "tags": "",
        "added_on": int(time.time()) + 120,
        "seeding_time": 10,
        "uploaded": 0,
        "downloaded": 0,
        "ratio": 0.0,
        "ticks": 0,
        "check_ticks": 1,
        "behaviour": "ok",
    }


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


@then(parsers.parse("该组应列出 {count:d} 条辅种子"))
def aux_count(app, count):
    assert len(_group(app)["aux"]) == count, _group(app)


@then("该组的辅种子应全部校验通过并在做种")
def aux_all_seeding(app):
    for aux in _group(app)["aux"]:
        assert aux["state"] == "seeding", aux
        assert aux["in_downloader"] is True, aux
        assert aux["phase"] == "complete", aux


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
    found = [a for a in _group(app)["aux"] if a["duplicate"]]
    assert len(found) == count, _group(app)


@then(parsers.parse("该组的辅种子总数应为 {count:d}"))
def aux_total(app, count):
    assert len(_group(app)["aux"]) == count, _group(app)


@then(parsers.parse("搜索应命中这 {count:d} 组"))
def search_hit(app, count):
    assert _view(app)["total"] == count, _view(app)


@then(parsers.parse("顶部统计的辅种总数应为 {count:d}"))
def header_total(app, count):
    assert _view(app)["counts"]["aux_total"] == count, _view(app)["counts"]


@then("该组不应再列出那条记录")
def record_gone(app):
    assert all(a["attempt_id"] != app.deleted_attempt for a in _group(app)["aux"]), _group(app)


@then(parsers.parse("该组仍应列出 {count:d} 条下载器里的辅种子"))
def live_aux_left(app, count):
    # 删的是记录，不是种子：下载器里的副本还在，页面就不该假装它消失了。
    live = [a for a in _group(app)["aux"] if a["in_downloader"]]
    assert len(live) == count, _group(app)


# ---------------------------------------------------------------- 自动清理


@when("预览无效辅种清理")
def preview_cleanup(app):
    status, payload = app.request("POST", "/api/seeds/cleanup?dry_run=true")
    assert status == 200, payload
    app.cleanup = payload


@when("开启自动清理")
def enable_cleanup(app):
    status, payload = app.request(
        "PUT",
        "/api/config",
        {"cleanup": {"enabled": True, "delete_files": True}},
    )
    assert status == 200, payload


@then(parsers.parse("预览应列出 {count:d} 组要清理的资源"))
def preview_groups(app, count):
    # 预览里候选 = 通过证据检查、会被清理的组。
    assert app.cleanup["candidates"] == count, app.cleanup
    assert app.cleanup["actions"], app.cleanup


@then("预览不应删除任何种子")
def preview_no_delete(app):
    assert app.fake.deleted == [], app.fake.deleted


@then("下载器里不应再有主种子名字的种子")
def no_aux_torrents_left(app):
    left = [t for t in app.fake.torrents.values() if t["name"] == SOURCE_NAME]
    assert left == [], app.fake.torrents


@then("最近一次删除应连同文件")
def delete_included_files(app):
    assert app.fake.deleted, "没有发生删除"
    assert all(d["delete_files"] for d in app.fake.deleted), app.fake.deleted


@then(parsers.parse("本次任务查询的源种子应为 {count:d} 个"))
def queried_sources(app, count):
    stats = (app.progress or {}).get("stats", {})
    assert stats.get("queried") == count, stats


@then("最近一次删除不应删除文件")
def delete_kept_files(app):
    assert app.fake.deleted, "没有发生删除"
    assert all(not d["delete_files"] for d in app.fake.deleted), app.fake.deleted


# ---------------------------------------------------------------- 下载流量


@given("下载器里有两个同数据的无标签副本，其中一份下载过数据")
def two_untagged_copies(app):
    """同一个资源的两份副本，都没有标签；只有一份真的下载过数据。

    这份有下载流量的加入得更晚，所以「按加入时间取最早」会选错，只能靠流量。
    """
    now = int(time.time())

    def put(digest: str, downloaded: int, added_on: int) -> None:
        app.fake.torrents[digest] = {
            "hash": digest,
            "name": SOURCE_NAME,
            "save_path": "/data/source",
            "content_path": f"/data/source/{SOURCE_NAME}",
            "total_size": 8 * worldkit.MIB,
            "progress": 1.0,
            "state": "stalledUP",
            "category": "movies",
            "tags": "",
            "added_on": added_on,
            "seeding_time": 10,
            "uploaded": 0,
            "downloaded": downloaded,
            "ratio": 0.0,
            "ticks": 0,
            "check_ticks": 1,
            "behaviour": "ok",
        }

    put(f"{len(app.fake.torrents) + 1001:040x}", 0, now - 3600)
    app.traffic_hash = f"{len(app.fake.torrents) + 1002:040x}"
    put(app.traffic_hash, 8 * worldkit.MIB, now)


@then("该组的主种子应是下载过数据的那一份")
def main_has_traffic(app):
    main = _group(app)["main"]
    assert main["hash"] == app.traffic_hash, (_group(app), app.traffic_hash)
    assert main["downloaded"] > 0, main


@then(parsers.parse("该组的主种子识别依据应为 {source}"))
def main_source_is(app, source):
    assert _group(app)["main_source"] == source, _group(app)
