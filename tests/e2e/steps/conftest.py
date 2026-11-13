"""Shared step definitions, declared where pytest will actually find them.

pytest-bdd 9 implements every `@given`/`@when`/`@then` as a pytest *fixture*. A
fixture only reaches a test whose node sits under the directory that declared it,
and pytest only parses fixtures out of conftest modules and collected test
modules — importing a helper module does not register anything.

So the steps that several features share are declared right here, in the conftest
that is a parent of every feature's step module. That makes one declaration
visible to all features instead of forcing each module to redeclare it (which
pytest-bdd rejects as a duplicate step).

Nothing in this file has a `scenarios(...)` call: conftest is not a test module.
"""


from __future__ import annotations

import time

from pytest_bdd import given, parsers

import fakes
import worldkit

SOURCE_NAME = "Demo.Movie.2026.2160p.WEB-DL"

SLUG_SID = {"alpha": fakes.SID_A, "beta": fakes.SID_B, "gamma": fakes.SID_C}


@given("下载器里有一个已完成做种的种子")
def completed_source(app):
    digest, _ = worldkit.add_torrent(app.fake, name=SOURCE_NAME, category="movies")
    app.source_hash = digest
    app.offered = {}
    return digest


@given("下载器里有一个尚未下载完成的种子")
def incomplete_source(app):
    digest, _ = worldkit.add_torrent(
        app.fake, name=SOURCE_NAME, progress=0.42, state_name="downloading"
    )
    app.source_hash = digest
    app.offered = {}
    return digest


@given("该种子已带有本工具的辅种标记")
def tagged_as_crossseed(app):
    app.fake.torrents[app.source_hash]["tags"] = "iyuu-reseed"


@given(parsers.parse("IYUU 在 {count:d} 个站点上都能找到该资源"))
def iyuu_offers(app, count):
    if getattr(app, "offered", None) is None:
        app.offered = {}
    for index, slug in enumerate(["alpha", "beta", "gamma"][:count], start=1):
        digest = worldkit.offer_on_site(
            app.fake,
            slug=slug,
            torrent_id=100 + index,
            name=f"{SOURCE_NAME}.{slug.upper()}",
            source_hash=app.source_hash,
        )
        app.offered[slug] = digest


@given("已为这两个站点配置了 passkey")
@given("已为该站点配置了 passkey")
def credentials_for_offered_sites(app):
    for slug in app.offered:
        app.add_site_credentials(SLUG_SID[slug], app.fake.site_passkeys[slug])


@given("该站点的 passkey 是错的")
def wrong_passkey(app):
    slug = next(iter(app.offered))
    # Deliberately different from the fake's real passkey: the tracker answers
    # 403 with an HTML body, which is the exact shape that used to be saved as a
    # `.torrent` file.
    app.add_site_credentials(SLUG_SID[slug], "wrong-passkey-for-this-test")


@given("一个已配置好的实例")
def configured_instance(app):
    return app


@given("一个全新实例")
def fresh_instance(world):
    return world
