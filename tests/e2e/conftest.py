"""Harness: run the real `iyuu-reseed` binary against a fully fake world.

The binary under test is built from this workspace. It gets its own temp data
dir, its own SQLite file, and a fake IYUU / qBittorrent / PT-site on loopback,
so nothing here can reach — let alone modify — a live downloader or a live
account. The operator's real qB is never addressed.

Everything the engine does is real: its HTTP client, its bencode hashing, its
signature computation, its state machine. Only the bytes arriving from the
network are synthetic.
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

import pytest

import fakes

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))


REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BINARY = os.path.join(REPO_ROOT, "target", "debug", "iyuu-reseed")


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class World:
    """A fake internet plus a server process, restartable against the same data."""

    def __init__(self, tmpdir: str):
        self.tmpdir = tmpdir
        self.data_dir = os.path.join(tmpdir, "data")
        self.fake = fakes.State()
        self.fake.catalogue()
        self.httpd, fake_port = fakes.serve(self.fake)
        self.fake_base = f"http://127.0.0.1:{fake_port}"
        self.port = free_port()
        self.base = f"http://127.0.0.1:{self.port}"
        self.proc: subprocess.Popen | None = None
        self.log_path = os.path.join(tmpdir, "server.log")
        self._log = None

    # -- process ---------------------------------------------------------

    def start(self) -> "World":
        env = dict(os.environ)
        # The one deployment knob that redirects every IYUU call at the fake.
        env["IYUU_API_BASE"] = self.fake_base
        env["IYUU_LOG"] = "info"
        self._log = open(self.log_path, "ab")
        self.proc = subprocess.Popen(
            [
                BINARY,
                "--data-dir",
                self.data_dir,
                "--web-dir",
                os.path.join(self.tmpdir, "no-web"),
                "--listen",
                f"127.0.0.1:{self.port}",
            ],
            env=env,
            stdout=self._log,
            stderr=subprocess.STDOUT,
        )
        self._wait_ready()
        return self

    def restart(self) -> "World":
        """Restart the process against the same data dir — the persistence probe."""
        self.stop_process()
        return self.start()

    def _wait_ready(self, timeout: float = 30.0) -> None:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.proc and self.proc.poll() is not None:
                raise RuntimeError(
                    f"server exited early (code {self.proc.returncode}):\n{self.tail_log()}"
                )
            try:
                self.get("/api/health")
                return
            except Exception:
                time.sleep(0.1)
        raise RuntimeError(f"server never became ready:\n{self.tail_log()}")

    def stop_process(self) -> None:
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)
        self.proc = None
        if self._log:
            self._log.close()
            self._log = None

    def stop(self) -> None:
        self.stop_process()
        if self.httpd:
            self.httpd.shutdown()

    def tail_log(self, lines: int = 80) -> str:
        try:
            with open(self.log_path, "r", errors="replace") as f:
                return "".join(f.readlines()[-lines:])
        except FileNotFoundError:
            return ""

    # -- HTTP ------------------------------------------------------------

    def request(self, method: str, path: str, body=None):
        data = None
        headers = {}
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(self.base + path, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=60) as resp:
                raw = resp.read()
                return resp.status, json.loads(raw) if raw else None
        except urllib.error.HTTPError as e:
            raw = e.read()
            try:
                return e.code, json.loads(raw) if raw else None
            except json.JSONDecodeError:
                return e.code, {"raw": raw.decode(errors="replace")}

    def get(self, path: str):
        return self.request("GET", path)[1]

    def post(self, path: str, body=None):
        return self.request("POST", path, body if body is not None else {})

    def put(self, path: str, body=None):
        return self.request("PUT", path, body if body is not None else {})

    def delete(self, path: str):
        return self.request("DELETE", path)

    # -- setup helpers ---------------------------------------------------

    def configure(self, **overrides) -> None:
        """Point the app at the fake qB and save the fake IYUU token.

        `downloader=` replaces the whole downloader block rather than merging into
        the default one: the API treats a blank secret as "unchanged", so merging
        would leave the default API key in place and make it impossible to set up
        a password-auth client.
        """
        downloader = overrides.pop(
            "downloader",
            {
                "kind": "qbittorrent",
                "name": "e2e-qb",
                "base_url": f"{self.fake_base}/qb",
                "auth": "api_key",
                "api_key": self.fake.dl_api_key,
            },
        )
        body = {
            "iyuu": {"token": self.fake.token, "tier": "free"},
            "downloader": downloader,
            "schedule": {"enabled": False, "cron": "0 4 * * *"},
        }
        body.update(overrides)
        status, payload = self.put("/api/config", body)
        assert status == 200, f"config PUT failed: {status} {payload}"

    def configure_without_catalogue(self) -> None:
        """Configure everything, but arrange for the catalogue to stay empty.

        Saving a token makes the server try `bootstrap_site_catalogue` in the
        background, so simply "not refreshing" is racy — it wins about as often
        as it loses. Taking IYUU offline for the bootstrap is deterministic: the
        fetch fails, the catalogue stays uninitialised, and the preflight under
        test (「还没有 IYUU 站点目录」) is the one that answers.

        IYUU is brought back afterwards so the *running* server can still reach a
        downloader; only the catalogue is missing, which is exactly the state the
        scenario is about.
        """
        self.fake.token_offline = True
        try:
            self.configure()
            # The bootstrap sleeps ~3s before its first outbound call, then fails.
            # Waiting for the run log to record that failure is what makes "the
            # catalogue is still empty" a settled fact rather than a race against
            # a bootstrap that has not started yet.
            deadline = time.time() + 20
            while time.time() < deadline:
                log = " ".join((self.get("/api/runs/progress") or {}).get("log") or [])
                if "首次拉取站点目录失败" in log:
                    break
                time.sleep(0.25)
            else:
                raise AssertionError(f"bootstrap never failed:\n{self.tail_log(20)}")
        finally:
            self.fake.token_offline = False
        payload = self.get("/api/sites") or {}
        assert payload.get("initialised") is False, payload

    def wait_catalogue(self, timeout: float = 25.0) -> dict:
        deadline = time.time() + timeout
        while time.time() < deadline:
            payload = self.get("/api/sites") or {}
            if payload.get("initialised"):
                return payload
            time.sleep(0.1)
        raise AssertionError(f"catalogue never bootstrapped: {self.get('/api/sites')}")

    def add_site_credentials(self, sid: int, passkey: str) -> None:
        status, payload = self.put(f"/api/sites/{sid}/credentials", {"passkey": passkey})
        assert status == 200, f"credential PUT failed: {status} {payload}"

    # -- runs ------------------------------------------------------------

    def start_run(self, expect=202):
        status, payload = self.post("/api/runs", {"trigger": "manual"})
        if expect is not None:
            assert status == expect, f"start_run -> {status} {payload}"
        return status, payload

    def wait_run_done(self, timeout: float = 120.0) -> dict:
        deadline = time.time() + timeout
        while time.time() < deadline:
            payload = self.get("/api/runs/progress") or {}
            if not payload.get("running") and payload.get("phase") in {
                "done",
                "failed",
                "cancelled",
            }:
                return payload
            time.sleep(0.15)
        raise AssertionError(
            f"run did not finish in {timeout}s: {self.get('/api/runs/progress')}\n{self.tail_log(30)}"
        )

    def attempts(self, state: str | None = None) -> list[dict]:
        path = "/api/attempts?limit=500"
        if state:
            path += f"&state={state}"
        return (self.get(path) or {}).get("items", [])

    def wait_until(self, probe, timeout: float = 60.0, what: str = "condition"):
        """Poll `probe()` until it is truthy, and return its value."""
        deadline = time.time() + timeout
        last = None
        while time.time() < deadline:
            last = probe()
            if last:
                return last
            time.sleep(0.15)
        raise AssertionError(f"{what} never happened in {timeout}s; last={last}")

    def wait_attempt_state(self, count: int, state: str, timeout: float = 90.0):
        def probe():
            items = self.attempts(state)
            return items if len(items) >= count else None

        return self.wait_until(probe, timeout, f"{count} attempt(s) in state {state!r}")

    def torrents(self) -> dict:
        return self.get("/api/downloader/torrents?limit=500") or {"items": [], "total": 0}

    def runs(self, limit: int = 20) -> list[dict]:
        """`GET /runs` returns a bare array of run records, newest first."""
        payload = self.get(f"/api/runs?limit={limit}")
        return payload or []

    def health_flags(self) -> dict:
        return self.get("/api/status") or {}


@pytest.fixture()
def world(tmp_path):
    w = World(str(tmp_path)).start()
    try:
        yield w
    finally:
        w.stop()


@pytest.fixture()
def app(world):
    """A configured, catalogue-initialised app."""
    world.configure()
    world.wait_catalogue()
    return world
