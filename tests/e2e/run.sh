#!/usr/bin/env bash
# Run the end-to-end / BDD suite.
#
#   ./run.sh              # everything
#   ./run.sh -k catalogue # a subset
#
# Everything runs against a local fake of IYUU, qBittorrent and the trackers on
# loopback. No live downloader, IYUU account or tracker is contacted, so this is
# safe to run against a machine that is actually seeding.
#
# Requirements: a debug build of the binary (`cargo build`), and `uv` to provide
# pytest + pytest-bdd. `uv` fetches them into a throwaway environment, so the
# repo needs no Python dependencies.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"

if [[ ! -x "$root/target/debug/iyuu-reseed" ]]; then
  echo "building iyuu-reseed…" >&2
  (cd "$root" && cargo build --locked)
fi

if ! command -v uv >/dev/null 2>&1; then
  echo "uv is required to run the e2e suite (https://docs.astral.sh/uv/)" >&2
  exit 1
fi

exec uv run --quiet \
  --with pytest --with pytest-bdd \
  python -m pytest "$here" "$@"
