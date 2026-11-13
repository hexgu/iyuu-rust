"""Steps for features/api.feature — settings, credentials, redaction."""

from __future__ import annotations

import json

from pytest_bdd import given, parsers, scenarios, then, when

import fakes

scenarios("../features/api.feature")


def _config(app):
    payload = app.get("/api/config")
    assert payload is not None
    return payload




@when("读取配置")
def read_config(app):
    app.config = _config(app)
    # The raw body matters for the redaction assertions, so capture it too.
    app.config_raw = json.dumps(app.config, ensure_ascii=False)


@then("配置里应显示 IYUU Token 已配置")
def token_configured(app):
    assert app.config["iyuu"]["has_token"] is True, app.config["iyuu"]


@then("配置里应显示会员类型为 free")
def tier_free(app):
    assert app.config["iyuu"]["tier"] == "free", app.config["iyuu"]
    assert app.config["iyuu"]["tier_chosen"] is True


@then("下载器类型应为 qbittorrent")
def downloader_kind(app):
    assert app.config["downloader"]["kind"] == "qbittorrent", app.config["downloader"]


@then("响应正文里不应包含 IYUU Token 原文")
def no_token_leak(app):
    assert app.fake.token not in app.config_raw, "IYUU token leaked into /api/config"


@then("响应正文里不应包含下载器 API Key 原文")
def no_api_key_leak(app):
    assert app.fake.dl_api_key not in app.config_raw, "downloader API key leaked"


@then("响应正文里不应包含任何站点 passkey 原文")
def no_passkey_leak(app):
    for slug, passkey in app.fake.site_passkeys.items():
        assert passkey not in app.config_raw, f"passkey for {slug} leaked"


@when(parsers.parse("把会员类型改成 {tier}"))
def set_tier(app, tier):
    app.patch_result = app.put("/api/config", {"iyuu": {"tier": tier}})


@given("一个用用户名密码认证的实例")
def password_auth_instance(world):
    """Configure with password auth and no API key at all.

    Must start from a fresh instance: the API treats a blank secret as
    "unchanged", so once a key is stored no patch can remove it. Distinct from
    `一个已配置好的实例` (which stores a key) for exactly that reason.
    """
    world.configure(
        downloader={
            "kind": "qbittorrent",
            "name": "e2e-qb",
            "base_url": f"{world.fake_base}/qb",
            "auth": "password",
            "username": world.fake.dl_username,
            "password": world.fake.dl_password,
        }
    )
    world.wait_catalogue()
    return world


@when("把下载器认证方式切到 api_key 而不提供密钥")
def switch_to_api_key_without_key(world):
    # Uses `world`, not `app`: the `app` fixture pre-stores an API key, and a
    # stored key can never be cleared by a patch (blank means "unchanged"). The
    # missing-key path only exists on an instance that never had one.
    stored = world.get("/api/config")["downloader"]
    assert stored["has_api_key"] is False, stored
    world.patch_result = world.put("/api/config", {"downloader": {"auth": "api_key"}})


@when("把下载器认证方式切到 api_key 并提交空白密钥")
def switch_to_api_key_with_blank(app):
    app.patch_result = app.put(
        "/api/config", {"downloader": {"auth": "api_key", "api_key": "   "}}
    )


@then(parsers.parse("接口应返回 {code:d}"))
def status_is(app, code):
    status, payload = app.patch_result
    assert status == code, f"expected {code}, got {status}: {payload}"


@then("接口应返回 400 并提示缺少密钥")
def missing_key_message(world):
    status, payload = world.patch_result
    assert status == 400, (status, payload)
    message = payload["error"]["message"]
    assert "密钥" in message or "API Key" in message, message


@then("配置里仍显示已有 API Key")
def key_still_present(app):
    status, payload = app.patch_result
    assert status == 200, (status, payload)
    assert payload["config"]["downloader"]["has_api_key"] is True, payload["config"]["downloader"]


@when("为站点 alpha 设置 passkey")
def set_alpha_credentials(app):
    app.add_site_credentials(fakes.SID_A, app.fake.site_passkeys["alpha"])


@then("该站点应显示为已配置凭据")
def site_has_credentials(app):
    sites = {s["id"]: s for s in app.get("/api/sites")["sites"]}
    assert sites[fakes.SID_A]["has_credentials"] is True, sites[fakes.SID_A]


@when("删除该站点凭据")
def delete_alpha_credentials(app):
    status, payload = app.delete(f"/api/sites/{fakes.SID_A}/credentials")
    assert status == 200, (status, payload)


@then("该站点应显示为未配置凭据")
def site_lacks_credentials(app):
    sites = {s["id"]: s for s in app.get("/api/sites")["sites"]}
    assert sites[fakes.SID_A]["has_credentials"] is False, sites[fakes.SID_A]


@when("删除一个从未配置过的站点凭据")
def delete_unknown_credentials(app):
    app.patch_result = app.delete(f"/api/sites/{fakes.SID_C}/credentials")


@when("为站点提交一份全空的凭据")
def submit_empty_credentials(app):
    app.patch_result = app.put(f"/api/sites/{fakes.SID_A}/credentials", {})


@when("调用下载器连通性测试")
def test_downloader(app):
    app.test_result = app.post("/api/test/downloader", {})
    status, payload = app.test_result
    assert status == 200, f"downloader test failed: {status} {payload}"


@then(parsers.parse("应报告版本号 {version}"))
def reports_version(app, version):
    _, payload = app.test_result
    assert payload["version"] == version, payload


@then("应报告种子总数")
def reports_torrent_count(app):
    _, payload = app.test_result
    assert "torrent_count" in payload, payload
    assert payload["torrent_count"] >= 0


@when("用空 Token 调用 IYUU 连通性测试")
def test_iyuu_empty(world):
    world.patch_result = world.post("/api/test/iyuu", {"token": "  "})


@then("接口应返回 400 并提示尚未配置 Token")
def without_token(world):
    status, payload = world.patch_result
    assert status == 400, (status, payload)
    assert "Token" in payload["error"]["message"], payload


@when("读取状态")
def read_status(app):
    app.status = app.health_flags()


@then("状态应报告已配置")
def status_configured(app):
    assert app.status["configured"] is True, app.status


@then("状态应报告 IYUU 与下载器都可用")
def status_both_up(app):
    assert app.status["iyuu_ok"] is True, app.status
    assert app.status["downloader_ok"] is True, app.status


@when("触发一次辅种任务并等待完成")
def run_and_wait(app):
    app.start_run()
    app.progress = app.wait_run_done(120)


@then("运行记录里应有这次任务")
def run_recorded(app):
    runs = app.runs()
    assert runs, "no runs recorded"
    assert runs[0]["id"] == app.progress["run_id"], (runs[0], app.progress)


@then("该记录的触发方式应为 manual")
def run_is_manual(app):
    runs = app.runs()
    assert runs[0]["trigger"] == "manual", runs[0]
