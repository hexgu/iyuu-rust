"""Steps for features/robustness.feature — surviving IYUU's real-world payloads.

Each scenario pins a specific failure the reference implementations hit:
numeric-string ids, one poisoned row in a 200-hash batch, an expired sid_sha1, a
rate-limit mid-run, an unactivated token, an unrenderable download template.

Shared setup (`download source`, `site offers`, passkeys) lives in
common_steps.py; only the failure injections are here.
"""

from __future__ import annotations

from pytest_bdd import given, parsers, scenarios, then, when

import fakes
import worldkit

scenarios("../features/robustness.feature")

SLUG_SID = {"alpha": fakes.SID_A, "beta": fakes.SID_B, "gamma": fakes.SID_C}
SOURCE_NAME = "Robustness.Movie.2026"


def _source(app):
    digest, _ = worldkit.add_torrent(app.fake, name=SOURCE_NAME)
    app.source_hash = digest
    app.offered = {}
    return digest


def _offer(app, *, slug="alpha", torrent_id=101, sid=None):
    digest = worldkit.offer_on_site(
        app.fake,
        slug=slug,
        torrent_id=torrent_id,
        name=f"{SOURCE_NAME}.{slug.upper()}",
        sid=sid,
        source_hash=app.source_hash,
    )
    app.offered = {slug: digest}
    return digest


def _run(app, timeout=120):
    app.start_run()
    app.progress = app.wait_run_done(timeout)
    return app.progress




@given("IYUU 在该资源上返回的 sid 与 torrent_id 是数字字符串")
def string_ids(app):
    _source(app)
    _offer(app)
    app.fake.string_ids = True
    app.add_site_credentials(fakes.SID_A, app.fake.site_passkeys["alpha"])


@given("IYUU 在同一批次里混入一行无法解析的候选")
def poisoned_row(app):
    _source(app)
    _offer(app)
    # `torrent_id: true` — a boolean where the schema promises an id. The real
    # service has produced exactly this shape for a broken row, and deserializing
    # a whole batch strictly is what used to discard 200 good answers.
    app.fake.extra_bad_row[app.source_hash] = [
        {"sid": True, "torrent_id": True, "info_hash": "zzzz"}
    ]
    app.add_site_credentials(fakes.SID_A, app.fake.site_passkeys["alpha"])


@given("下一次辅种查询会因 sid_sha1 过期被拒绝")
def expire_sid(app):
    # Needs a source torrent and a candidate first: the "已为该站点配置了 passkey"
    # step that follows depends on `app.offered` existing.
    _source(app)
    _offer(app)
    app.add_site_credentials(fakes.SID_A, app.fake.site_passkeys["alpha"])
    app.fake.sid_sha1 = "stale-token"
    app.fake.expire_sid_sha1_once = True


@given("IYUU 会在查询时返回限流")
def rate_limited(app):
    _source(app)
    _offer(app)
    app.add_site_credentials(fakes.SID_A, app.fake.site_passkeys["alpha"])
    app.fake.quota_reject_after = 0


@given("IYUU 认为该 Token 尚未绑定合作站点")
def unactivated(app):
    app.fake.activated = False


@given("目标站点的下载模板含无法替换的占位符")
def unrenderable_template(app):
    _source(app)
    _offer(app)
    for row in app.fake.sites:
        if row["site"] == "alpha":
            row["download_page"] = "download.php?id={}&authkey={authkey}"
    app.fake.overlay_sites = [dict(s) for s in app.fake.sites]
    status, payload = app.post("/api/sites/refresh", {})
    assert status == 200, f"refresh failed: {status} {payload}"
    app.add_site_credentials(fakes.SID_A, app.fake.site_passkeys["alpha"])


@given("该站点的详情页里含有下载链接")
def details_page_has_link(app):
    # The fake's details endpoint renders a real download anchor; the scenario
    # asserts the outcome (a successful cross-seed) rather than the HTML.
    app.fake.details_available = True


@given("一个没有任何站点目录的实例")
def no_catalogue(world):
    world.configure_without_catalogue()
    return world


@given("一个站点目录已初始化但没有配置任何站点凭据的实例")
def catalogue_without_credentials(app):
    assert app.get("/api/config")["credentials"] == {}


@when("触发一次辅种任务")
def run(app):
    _run(app)


@when("调用检查激活接口")
def check_activation(app):
    app.activation = app.request("GET", "/api/partners/activated")


@when("用正确的 passkey 绑定合作站点")
def bind_ok(app):
    status, payload = app.post(
        "/api/partners/bind",
        {
            "site": "alpha",
            "sid": fakes.SID_PARTNER,
            "uid": "28353",
            "passkey": app.fake.site_passkeys["alpha"],
        },
    )
    app.bind_result = (status, payload)
    assert status == 200, f"bind failed: {status} {payload}"


@when("用错误的 passkey 绑定合作站点")
def bind_bad(app):
    status, payload = app.post(
        "/api/partners/bind",
        {"site": "alpha", "sid": fakes.SID_PARTNER, "uid": "28353", "passkey": "nope"},
    )
    app.bind_result = (status, payload)


@then("任务应正常完成")
def done(app):
    assert app.progress["phase"] == "done", app.progress


@then(parsers.parse("应成功新增 {count:d} 个辅种"))
def added(app, count):
    assert app.progress["stats"]["added"] == count, app.progress["stats"]


@then("任务消息里应提示有候选行被跳过")
def warning_about_skipped_rows(app):
    messages = " ".join(str(r.get("message") or "") for r in app.runs())
    log = " ".join(app.progress.get("log") or [])
    assert "跳过" in messages or "跳过" in log, f"runs={messages!r} log={log!r}"


@then(parsers.parse("仿真 IYUU 应收到至少 {count:d} 次汇报站点请求"))
def re_reported(app, count):
    actual = worldkit.report_existing_requests(app.fake)
    assert actual >= count, f"expected >= {count}, got {actual}: {app.fake.request_log}"


@then("任务不应崩溃")
def run_survived(app):
    # Rate limiting must degrade to partial results, not to a failed run that
    # throws the candidates away.
    assert app.get("/api/health")["ok"] is True


@then("任务消息里应说明限流")
def message_mentions_limiting(app):
    text = " ".join(str(r.get("message") or "") for r in app.runs())
    log = " ".join(app.progress.get("log") or [])
    combined = text + log
    assert any(word in combined for word in ("限流", "额度", "频率")), combined


@then("接口应报告未激活")
def reports_inactive(app):
    status, payload = app.activation
    assert status == 200, (status, payload)
    assert payload["activated"] is False, payload


@then("提示应引导用户去绑定合作站点")
def hint_points_at_binding(app):
    _, payload = app.activation
    assert "绑定" in payload["message"], payload


@then("绑定应成功")
def bind_succeeded(app):
    status, payload = app.bind_result
    assert status == 200, (status, payload)


@then("再次检查激活状态应为已激活")
def now_activated(app):
    status, payload = app.request("GET", "/api/partners/activated")
    assert status == 200, (status, payload)
    assert payload["activated"] is True, payload


@then("绑定接口应返回错误")
def bind_failed(app):
    status, payload = app.bind_result
    assert status >= 400, (status, payload)


@when("直接请求启动一次辅种任务")
def start_run_raw(world):
    """Start a run without touching the `app` fixture.

    The `app` fixture configures the instance and waits for the catalogue, which
    would defeat this scenario. `world` is the unconfigured-catalogue state under
    test, so the request is issued directly against it.
    """
    world.patch_result = world.post("/api/runs", {"trigger": "manual"})


@then("接口应拒绝启动并提示先刷新站点目录")
def refuses_no_catalogue(world):
    status, payload = world.post("/api/runs", {"trigger": "manual"})
    assert status == 400, (status, payload)
    assert "站点目录" in payload["error"]["message"], payload


@then("任务应失败并提示先添加站点凭据")
def fails_without_credentials(app):
    app.start_run()
    progress = app.wait_run_done(60)
    assert progress["phase"] == "failed", progress
    messages = " ".join(str(r.get("message") or "") for r in app.runs())
    log = " ".join(progress.get("log") or [])
    assert "站点凭据" in messages or "站点凭据" in log, f"{messages!r} / {log!r}"
