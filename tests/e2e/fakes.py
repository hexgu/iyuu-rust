"""A local stand-in for every network dependency the engine talks to.

One process, three faces, routed by path prefix:

    /reseed/...        IYUU API (catalogue, partners, bind, reportExisting, index/index)
    /qb/api/v2/...     qBittorrent WebAPI v2 (auth, torrents, add, recheck, start, ...)
    /site/download.php PT site download endpoint (passkey-gated, real bencode)

Everything is in-memory and deterministic. The engine is exercised through its
real HTTP client, its real bencode hashing and its real state machine — only the
bytes on the other end of the socket are fake.

Why this exists: the alternative is pointing a test at a live qBittorrent, which
would add torrents to a real library. `/qb` here is the *only* downloader the
tests touch; the operator's real qB is never addressed by a mutating call.
"""

from __future__ import annotations

import hashlib
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlparse

import torrentlib

# Mirrors iyuu_core::iyuu::HASH_CHUNK_SIZE; a bigger batch is a client bug the
# fake should surface rather than silently accept.
MAX_HASHES_PER_QUERY = 200

# Site ids used throughout the suite. Deliberately not real IYUU sids so a
# mixed-up catalogue is obvious.
SID_A = 9001
SID_B = 9002
SID_C = 9003
SID_PARTNER = 9100


class State:
    """Mutable world the fake serves from. Tests mutate this before a request."""

    def __init__(self) -> None:
        self.lock = threading.RLock()

        # --- IYUU -------------------------------------------------------
        self.token = "e2e-token-0123456789"
        self.activated = True
        # Catalogue rows, as /reseed/sites/index returns them.
        self.sites: list[dict] = []
        # hash -> list of candidate dicts ({sid, torrent_id, info_hash})
        self.candidates: dict[str, list[dict]] = {}
        # Extra raw rows injected into a hash's entry, to exercise the
        # "one unparseable candidate must not poison the batch" path.
        self.poison: dict[str, list] = {}
        # site slug -> (torrent_id -> torrent bytes)
        self.site_torrents: dict[str, dict[int, bytes]] = {}
        self.site_passkeys: dict[str, str] = {}
        self.sid_sha1 = ""
        self.request_log: list[str] = []
        self.quota_reject_after: int | None = None
        self.expire_sid_sha1_once = False
        self.reseed_requests = 0
        # Hash-lookup behaviour knobs for the scar tissue scenarios:
        # a row whose ids arrive as numeric strings (the payload IYUU really
        # sends), and a hash injected with a torrent_id of `true` (unparseable).
        self.string_ids = True
        self.extra_bad_row: dict[str, list] = {}
        # Site catalogue can be replaced wholesale to test a stale catalogue.
        self.overlay_sites: list[dict] | None = None
        # Make the catalogue endpoint fail, so the one-time bootstrap cannot
        # succeed. Used to pin the "no catalogue yet" preflight deterministically.
        self.token_offline = False
        # Sites whose details page exists — the fallback path for an unrenderable
        # download template. Off by default so a bad passkey stays a hard failure
        # instead of being papered over by a scrape.
        self.details_available = False

        # --- downloader -------------------------------------------------
        self.dl_username = "admin"
        self.dl_password = "adminadmin"
        self.dl_api_key = "e2e-api-key"
        # hash -> torrent record
        self.torrents: dict[str, dict] = {}
        self.add_log: list[dict] = []
        # hash -> behaviour: ok | incomplete | missing | error | reject
        self.verify_behaviour: dict[str, str] = {}
        # Torrents that must never appear in a listing (a scheduler outage).
        self.dl_down = False

    # -- helpers ---------------------------------------------------------

    def catalogue(self) -> None:
        """Install the three-site catalogue and their torrent stores.

        Each site lives under its own path prefix on the fake's single port.
        That is what makes the sites distinguishable: `IyuuSite::host()` is just
        the base_url minus its scheme, so a shared host plus a shared template
        would make every candidate download from the same URL and the "right
        torrent from the right site" assertions meaningless.
        """
        self.sites = [
            self.site_row(SID_A, "alpha", "Alpha"),
            self.site_row(SID_B, "beta", "Beta"),
            self.site_row(SID_C, "gamma", "Gamma"),
        ]
        self.site_passkeys = {"alpha": "pk-alpha", "beta": "pk-beta", "gamma": "pk-gamma"}
        self.site_torrents = {s["site"]: {} for s in self.sites}

    @staticmethod
    def site_row(sid: int, slug: str, nickname: str) -> dict:
        # `base_url` is filled in by the server at request time because only it
        # knows the port it was bound to.
        return {
            "id": sid,
            "site": slug,
            "nickname": nickname,
            "base_url": None,
            "download_page": "download.php?id={}&passkey={passkey}",
            "details_page": "details.php?id={}",
            "reseed_check": "passkey",
            "is_https": 0,
            "cookie_required": 0,
        }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    state: State  # assigned at startup

    # ------------------------------------------------------------------ util

    def log_message(self, *_args) -> None:  # silence the default stderr spam
        pass

    def _body(self) -> bytes:
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def _send(self, code: int, payload: bytes, ctype: str, headers: dict | None = None):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(payload)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(payload)

    def _json(self, code: int, obj, headers: dict | None = None):
        self._send(code, json.dumps(obj).encode(), "application/json", headers)

    def _iyuu_ok(self, data):
        self._json(200, {"code": 0, "data": data, "msg": "ok"})

    def _iyuu_err(self, code: int, msg: str):
        self._json(200, {"code": code, "data": None, "msg": msg})

    def _host(self) -> str:
        return self.headers.get("Host") or "127.0.0.1"

    def _form(self, raw: bytes) -> dict[str, str]:
        return {k: v[0] for k, v in parse_qs(raw.decode(errors="replace")).items()}

    # --------------------------------------------------------------- routing

    def do_GET(self):
        self._route("GET")

    def do_POST(self):
        self._route("POST")

    def _route(self, method: str):
        path = urlparse(self.path).path
        raw = self._body() if method == "POST" else b""
        try:
            if path.startswith("/qb/"):
                self._qb(method, path[len("/qb"):], raw)
            elif path.startswith("/reseed/"):
                self._iyuu(method, path, raw)
            elif path.startswith("/site/"):
                self._site(method, path, raw)
            else:
                # A tracker path: /<slug>/download.php
                slug = path.lstrip("/").split("/", 1)[0]
                if slug in self.state.site_passkeys:
                    self._site(method, path, raw, slug)
                else:
                    self._json(404, {"error": f"no route for {path}"})
        except Exception as exc:  # a fake that 500s silently wastes a debugging hour
            self._json(500, {"error": f"{type(exc).__name__}: {exc}"})

    # ------------------------------------------------------------------ IYUU

    def _iyuu(self, method: str, path: str, raw: bytes):
        st = self.state
        with st.lock:
            st.request_log.append(f"{method} {path}")
            if path.startswith("/reseed/index/index"):
                st.reseed_requests += 1

            # Every endpoint but `users/bind` carries the token in a header.
            # `bind` puts it in the form body instead — that is the wire format
            # the reference clients use, so the fake has to accept both.
            token = self.headers.get("token") or self.headers.get("Token")
            if method == "POST" and path == "/reseed/users/bind":
                token = self._form(raw).get("token") or token
            if token != st.token:
                return self._iyuu_err(401, "token 无效或已过期")

            if st.quota_reject_after is not None and st.reseed_requests > st.quota_reject_after:
                return self._json(429, {"code": 429, "data": None, "msg": "访问频率过快"})

            # `/sites/recommend` lists what *could* be bound, and `users/bind`
            # is the call that performs the binding — neither can be gated behind
            # activation, or a token could never become activated.
            if not st.activated and path not in (
                "/reseed/sites/recommend",
                "/reseed/users/bind",
            ):
                self._json(403, {"code": 403, "data": None, "msg": "用户未绑定推荐站点"})
                return

            if path == "/reseed/sites/index":
                if st.token_offline:
                    # A 500 is what the bootstrap sees when IYUU is unreachable:
                    # retryable, not a credential problem, and it leaves the
                    # catalogue uninitialised.
                    return self._json(500, {"code": 500, "data": None, "msg": "服务暂时不可用"})
                sites = []
                for s in st.overlay_sites if st.overlay_sites is not None else st.sites:
                    row = dict(s)
                    if row["base_url"] is None:
                        # host:port/slug — the engine strips the scheme, keeps
                        # the rest as the host, so each site resolves to its own
                        # tracker path on the fake.
                        row["base_url"] = f"{self._host()}/{row['site']}"
                    sites.append(row)
                return self._iyuu_ok({"count": len(sites), "sites": sites})

            if path == "/reseed/sites/recommend":
                return self._iyuu_ok(
                    {
                        "count": 1,
                        "list": [
                            {
                                "id": SID_PARTNER,
                                "site": "alpha",
                                "nickname": "Alpha",
                                "bind_check": "passkey,uid",
                                "reseed_check": "passkey",
                            }
                        ],
                    }
                )

            if path == "/reseed/users/bind":
                form = self._form(raw)
                expected = hashlib.sha1(st.site_passkeys["alpha"].encode()).hexdigest()
                if form.get("passkey") != expected:
                    return self._iyuu_err(0, "站点 passkey 校验失败")
                st.activated = True
                return self._iyuu_ok([])

            if path == "/reseed/sites/reportExisting":
                # When a scenario has armed `expire_sid_sha1_once`, the token we
                # hand back is already doomed: the server rotates its value right
                # after the response, so the client's next index query is
                # rejected as expired and has to re-report. That is precisely the
                # real-world case (token TTL elapsed between report and query).
                token = f"sidsha1-{int(time.time() * 1000)}"
                st.sid_sha1 = token
                if st.expire_sid_sha1_once:
                    st.sid_sha1 = f"rotated-{token}"
                return self._iyuu_ok({"sid_sha1": token})

            if path == "/reseed/index/index":
                return self._reseed_index(raw)

            return self._json(404, {"code": 404, "msg": f"unknown IYUU path {path}"})

    def _reseed_index(self, raw: bytes):
        st = self.state
        form = self._form(raw)

        # `reportExisting` rotates the server-side value right after handing the
        # client a token, so the *first* index query is already rejected as
        # expired. The client re-reports (getting a fresh token) and retries; the
        # second query must succeed, which pins "refresh once, then carry on"
        # rather than "retry forever".
        if st.expire_sid_sha1_once:
            st.expire_sid_sha1_once = False
            return self._iyuu_err(0, "站点哈希值已过期，请重新汇报")

        if form.get("sid_sha1") != st.sid_sha1:
            return self._iyuu_err(0, "站点哈希值不正确")

        try:
            hashes = json.loads(form["hash"])
        except (KeyError, json.JSONDecodeError):
            return self._iyuu_err(0, "hash 参数格式错误")

        # The client signs the exact sorted-array bytes; a mismatch means it
        # would be rejected by the real service too.
        expected = hashlib.sha1(
            json.dumps(sorted(hashes), separators=(",", ":")).encode()
        ).hexdigest()
        if form.get("sha1") != expected:
            return self._iyuu_err(0, f"签名校验失败 expected={expected} got={form.get('sha1')}")

        if len(hashes) > MAX_HASHES_PER_QUERY:
            return self._iyuu_err(0, f"单次最多 {MAX_HASHES_PER_QUERY} 个 hash")

        data = {}
        for h in hashes:
            rows = st.candidates.get(h)
            if not rows and h not in st.poison and h not in st.extra_bad_row:
                continue
            entry: dict = {"torrent": []}
            if rows:
                for c in rows:
                    # The real service serialises PHP ints as JSON numbers but
                    # occasionally as numeric *strings*. Both must work; that is
                    # the regression this knob pins.
                    sid = str(c["sid"]) if st.string_ids and c.get("as_string", True) else c["sid"]
                    tid = (
                        str(c["torrent_id"])
                        if st.string_ids and c.get("as_string", True)
                        else c["torrent_id"]
                    )
                    entry["torrent"].append(
                        {"sid": sid, "torrent_id": tid, "info_hash": c["info_hash"]}
                    )
            entry["torrent"].extend(st.poison.get(h, []))
            entry["torrent"].extend(st.extra_bad_row.get(h, []))
            data[h] = entry
        return self._iyuu_ok(data)

    # -------------------------------------------------------------------- site

    def _site(self, method: str, path: str, raw: bytes, slug: str | None = None):
        st = self.state
        parsed = urlparse(self.path)
        qs = {k: v[0] for k, v in parse_qs(parsed.query).items()}
        with st.lock:
            st.request_log.append(f"{method} {path}?{parsed.query}")
            # Tracker URLs look like /<slug>/download.php?id=N&passkey=...
            # The slug comes from the path so each fake site is its own host,
            # matching how real sites differ by domain.
            site = slug or path.lstrip("/").split("/", 1)[0]
            try:
                tid = int(qs.get("id", ""))
            except ValueError:
                return self._send(400, b"bad id", "text/plain")

            if path.endswith("details.php"):
                # Details pages are gated by a login cookie, not a passkey —
                # there is no passkey in a details URL at all.
                if not st.details_available:
                    return self._send(404, b"<html>not found</html>", "text/html")
                return self._details_page(site, tid)

            want = st.site_passkeys.get(site)
            if qs.get("passkey") != want:
                return self._send(403, b"<html>bad passkey</html>", "text/html")

            blob = st.site_torrents.get(site, {}).get(tid)
            if blob is None:
                return self._send(404, b"<html>no such torrent</html>", "text/html")
            return self._send(200, blob, "application/x-bittorrent")

    def _details_page(self, site: str, torrent_id: int):
        """A NexusPHP-shaped details page carrying one download anchor.

        The engine falls back to scraping this when a site's download template
        cannot be rendered; the anchor has to look like the real ones
        (`download.php?...&action=download`) and carry the torrent id so the
        scraper can pick the right release out of the page.
        """
        passkey = self.state.site_passkeys.get(site, "")
        html = (
            "<!DOCTYPE html><html><head><title>details</title></head><body>"
            f'<a href="/{site}/download.php?id={torrent_id}&passkey={passkey}'
            '&action=download">下载</a>'
            "</body></html>"
        )
        return self._send(200, html.encode(), "text/html; charset=utf-8")

    # ---------------------------------------------------------------------- qB

    def _qb(self, method: str, path: str, raw: bytes):
        st = self.state
        with st.lock:
            st.request_log.append(f"{method} qb{path}")

            if st.dl_down:
                return self._send(503, b"down", "text/plain")

            if not self._qb_authed(path):
                if path == "/api/v2/auth/login":
                    form = self._form(raw)
                    if (
                        form.get("username") == st.dl_username
                        and form.get("password") == st.dl_password
                    ):
                        return self._send(
                            200,
                            b"Ok.",
                            "text/plain",
                            {"Set-Cookie": "SID=e2e-session; path=/"},
                        )
                    return self._send(200, b"Fails.", "text/plain")
                return self._send(403, b"Forbidden", "text/plain")

            if path == "/api/v2/app/version":
                return self._send(200, b"v5.2.0", "text/plain")
            if path == "/api/v2/app/webapiVersion":
                return self._send(200, b"2.14.1", "text/plain")
            if path == "/api/v2/auth/login":
                return self._send(200, b"Ok.", "text/plain")

            if path == "/api/v2/torrents/info":
                return self._qb_info(parse_qs(urlparse(self.path).query))

            if path == "/api/v2/torrents/add":
                return self._qb_add(raw)

            if path.startswith("/api/v2/torrents/"):
                return self._qb_action(path.rsplit("/", 1)[-1], self._form(raw))

            return self._send(404, b"unknown qb path", "text/plain")

    def _qb_authed(self, path: str) -> bool:
        st = self.state
        if path == "/api/v2/auth/login":
            return True
        authz = self.headers.get("Authorization") or ""
        if authz.startswith("Bearer "):
            return authz[len("Bearer ") :] == st.dl_api_key
        cookie = self.headers.get("Cookie") or ""
        return "SID=e2e-session" in cookie

    def _qb_info(self, qs):
        st = self.state
        rows = list(st.torrents.values())

        # `parse_qs` gives {name: [value, ...]}; each value is already the whole
        # parameter, not a per-character list.
        hash_values = qs.get("hashes", [])
        if hash_values:
            wanted = {
                h.strip().lower()
                for part in hash_values
                for h in part.split("|")
                if h.strip()
            }
            rows = [t for t in rows if t["hash"] in wanted]

        tag_values = qs.get("tag", [])
        if tag_values:
            wanted_tags = {t.strip() for part in tag_values for t in part.split(",")}
            rows = [t for t in rows if wanted_tags & set(t["tags"].split(","))]

        # Advance the verification simulation on every observation, which is
        # what a real client does when we poll it.
        for t in rows:
            self._advance(t)

        return self._json(200, rows)

    def _advance(self, t: dict) -> None:
        """Move a torrent through checking → final, one poll at a time."""
        if t["state"] != "checkingDL":
            return
        t["ticks"] += 1
        if t["ticks"] < t.get("check_ticks", 1):
            return

        mode = t.get("behaviour", "ok")
        if mode == "ok":
            t["state"] = "stoppedUP"
            t["progress"] = 1.0
        elif mode == "incomplete":
            # Verified, found the data incomplete → the wrong-save-path case.
            t["state"] = "stalledDL"
            t["progress"] = 0.0
        elif mode == "missing":
            t["state"] = "missingFiles"
            t["progress"] = 0.0
        elif mode == "error":
            t["state"] = "error"
            t["progress"] = 0.0
        elif mode == "slow":
            t["state"] = "stalledUP"
            t["progress"] = 1.0

    def _qb_add(self, raw: bytes):
        st = self.state
        ctype = self.headers.get("Content-Type") or ""
        if "multipart/form-data" not in ctype:
            return self._send(415, b"expected multipart", "text/plain")

        boundary = ctype.split("boundary=", 1)[-1].encode()
        fields, files = _parse_multipart(raw, boundary)
        if not files:
            return self._send(200, b"Fails.", "text/plain")

        blob = files[0]
        # Hash the way the client does, from the bytes it actually sent.
        try:
            h = torrentlib.info_hash(blob)
        except Exception:
            return self._send(200, b"Fails.", "text/plain")

        if h in st.torrents:
            st.add_log.append({"hash": h, "duplicate": True, "fields": fields})
            return self._send(200, b"Fails.", "text/plain")

        if st.verify_behaviour.get("__reject_all__") == "reject":
            st.add_log.append({"hash": h, "rejected": True, "fields": fields})
            return self._send(200, b"Fails.", "text/plain")

        # 真实客户端上报的是种子自己的名称和大小；用 savepath 末段当名字会让
        # 每一条新增的种子都长成同一个 1 MiB 的 "media"，把分组测试变成瞎测。
        try:
            meta = torrentlib.info_fields(blob)
        except Exception:
            meta = {"name": "e2e", "size": 1 << 20}
        save_path = fields.get("savepath", "")
        st.torrents[h] = {
            "hash": h,
            "name": meta["name"] or "e2e",
            "save_path": save_path,
            "content_path": f"{save_path.rstrip('/')}/{meta['name']}" if save_path else None,
            "total_size": meta["size"] or (1 << 20),
            "progress": 0.0,
            "state": "checkingDL",
            "category": fields.get("category", ""),
            "tags": fields.get("tags", ""),
            "added_on": int(time.time()),
            "seeding_time": 0,
            "uploaded": 0,
            "ratio": 0.0,
            "ticks": 0,
            "check_ticks": 1,
            "behaviour": st.verify_behaviour.get(h, "ok"),
        }
        st.add_log.append({"hash": h, "fields": fields})
        return self._send(200, b"Ok.", "text/plain")

    def _qb_action(self, action: str, form: dict[str, str]):
        st = self.state
        hashes = [
            part.strip().lower()
            for part in (form.get("hashes") or "").split("|")
            if part.strip()
        ]
        known = [k for k in hashes if k in st.torrents]

        if action in ("stop", "pause"):
            for k in known:
                st.torrents[k]["state"] = "stoppedDL"
        elif action in ("start", "resume"):
            for k in known:
                t = st.torrents[k]
                t["state"] = "stalledUP" if t["progress"] >= 0.999 else "stalledDL"
        elif action == "recheck":
            for k in known:
                t = st.torrents[k]
                t["state"] = "checkingDL"
                t["ticks"] = 0
        elif action == "addTags":
            for k in known:
                existing = [x for x in st.torrents[k]["tags"].split(",") if x]
                for tag in (form.get("tags") or "").split(","):
                    if tag.strip() and tag.strip() not in existing:
                        existing.append(tag.strip())
                st.torrents[k]["tags"] = ",".join(existing)
        elif action == "setCategory":
            for k in known:
                st.torrents[k]["category"] = form.get("category", "")
        elif action == "delete":
            for k in known:
                del st.torrents[k]
        elif action in ("setAutoManagement", "setShareLimits", "toggleSequentialDownload", "setForceStart"):
            pass
        else:
            return self._send(404, b"unknown action", "text/plain")
        return self._send(200, b"", "text/plain")


def _parse_multipart(raw: bytes, boundary: bytes) -> tuple[dict[str, str], list[bytes]]:
    """Split a multipart body into form fields and file payloads."""
    fields: dict[str, str] = {}
    files: list[bytes] = []
    sep = b"--" + boundary
    for chunk in raw.split(sep):
        if not chunk.strip() or chunk.strip() == b"--":
            continue
        if b"\r\n\r\n" not in chunk:
            continue
        head, body = chunk.split(b"\r\n\r\n", 1)
        body = body.rstrip(b"\r\n")
        text = head.decode(errors="replace")
        if 'filename=' in text:
            files.append(body)
        elif 'name="' in text:
            name = text.split('name="', 1)[1].split('"', 1)[0]
            fields[name] = body.decode(errors="replace")
    return fields, files


def serve(state: State, port: int = 0) -> tuple[ThreadingHTTPServer, int]:
    handler = type("BoundHandler", (Handler,), {"state": state})
    httpd = ThreadingHTTPServer(("127.0.0.1", port), handler)
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    return httpd, httpd.server_address[1]
