"""Steps for features/catalogue.feature — catalogue fetched once, then cached."""

from __future__ import annotations

from pytest_bdd import given, parsers, scenarios, then, when

import fakes
import worldkit

scenarios("../features/catalogue.feature")


@given("一个已配置好 IYUU Token 与下载器的实例")
def configured_instance(app):
    # The `app` fixture already saved the token, which is what triggers the
    # one-time catalogue bootstrap; waiting is the previous step's job.
    return app


@given("一个站点目录已初始化的实例")
def initialised_instance(app):
    app.wait_catalogue()
    return app


@when("等待站点目录初始化完成")
def wait_catalogue(app):
    app.catalogue_snapshot = app.wait_catalogue()


@then(parsers.parse("站点目录应包含 {count:d} 个站点"))
def catalogue_has(app, count):
    payload = app.get("/api/sites")
    assert len(payload["sites"]) == count, payload
    app.catalogue_snapshot = payload


@then("目录应标记为已初始化")
def marked_initialised(app):
    assert app.get("/api/sites")["initialised"] is True


@then(parsers.parse("仿真 IYUU 应只收到 {count:d} 次站点目录请求"))
def catalogue_requested_once(app, count):
    actual = worldkit.catalogue_requests(app.fake)
    assert actual == count, f"expected {count}, got {actual}: {app.fake.request_log}"


@then(parsers.parse("仿真 IYUU 收到的站点目录请求次数应仍为 {count:d}"))
def catalogue_still(app, count):
    catalogue_requested_once(app, count)


@then(parsers.parse("仿真 IYUU 收到的站点目录请求次数应为 {count:d}"))
def catalogue_exactly(app, count):
    catalogue_requested_once(app, count)


@when(parsers.parse("连续读取 {times:d} 次站点列表"))
def read_sites_repeatedly(app, times):
    app.repeated_reads = [app.get("/api/sites") for _ in range(times)]


@then(parsers.parse("每次读取都应返回同样的 {count:d} 个站点"))
def every_read_matches(app, count):
    for payload in app.repeated_reads:
        assert len(payload["sites"]) == count, payload
        assert payload["initialised"] is True
    ids = {tuple(sorted(s["id"] for s in p["sites"])) for p in app.repeated_reads}
    assert len(ids) == 1, f"catalogue changed between reads: {ids}"


@when("重启服务进程")
def restart(app):
    before = worldkit.catalogue_requests(app.fake)
    app.rebooted = app.restart()
    app.requests_before_restart = before


@then("站点目录应仍标记为已初始化")
def still_initialised(app):
    assert app.get("/api/sites")["initialised"] is True
    assert app.catalogue_snapshot if hasattr(app, "catalogue_snapshot") else True


@when("仿真 IYUU 的站点目录新增一个站点")
def add_site_upstream(app):
    extra = fakes.State.site_row(fakes.SID_C + 1, "delta", "Delta")
    app.fake.overlay_sites = [*app.fake.sites, extra]
    app.fake.site_passkeys["delta"] = "pk-delta"
    app.fake.site_torrents["delta"] = {}


@when("仿真 IYUU 的站点目录下掉一个站点")
def drop_site_upstream(app):
    app.dropped_sid = app.fake.sites[-1]["id"]
    app.dropped_slug = app.fake.sites[-1]["site"]
    app.fake.overlay_sites = app.fake.sites[:-1]


@when("调用刷新接口")
def refresh(app):
    status, payload = app.post("/api/sites/refresh", {})
    assert status == 200, f"refresh failed: {status} {payload}"
    app.refresh_payload = payload


@then("目录中不应包含已下线的站点")
def dropped_site_gone(app):
    payload = app.get("/api/sites")
    ids = {s["id"] for s in payload["sites"]}
    assert app.dropped_sid not in ids, f"{app.dropped_sid} still present: {ids}"
    slugs = {s["site"] for s in payload["sites"]}
    assert app.dropped_slug not in slugs
