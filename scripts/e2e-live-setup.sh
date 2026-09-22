#!/bin/sh
set -eu
umask 077

RESET_CONFIG=0
case "${1-}" in
  "") ;;
  --reset-config) RESET_CONFIG=1 ;;
  --help)
    echo "Usage: scripts/e2e-live-setup.sh [--reset-config]"
    echo "Creates target/e2e-live; existing config is preserved unless reset is explicit."
    exit 0
    ;;
  *) echo "Unknown option: $1" >&2; exit 2 ;;
esac

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
LIVE="$ROOT/target/e2e-live"
DATA="$LIVE/data"
WORKSPACE="$LIVE/workspace"
mkdir -p "$DATA" "$WORKSPACE"
chmod 700 "$LIVE" "$DATA" "$WORKSPACE"

if [ -e "$DATA/config.json" ] && [ "$RESET_CONFIG" -ne 1 ]; then
  echo "Preserved existing $DATA/config.json (use --reset-config to replace it)."
  exit 0
fi

for plugin in cli shell openai-codex rlm memory; do
  if [ ! -f "$ROOT/target/plugins/$plugin/plugin.toml" ]; then
    echo "Missing local package target/plugins/$plugin; sync plugins and retry." >&2
    exit 1
  fi
done

ROOT="$ROOT" LIVE="$LIVE" DATA="$DATA" WORKSPACE="$WORKSPACE" python3 - <<'PY'
import json
import os
from pathlib import Path

root = Path(os.environ["ROOT"])
live = Path(os.environ["LIVE"])
data = Path(os.environ["DATA"])
workspace = Path(os.environ["WORKSPACE"])
uid = os.getuid()
plugins = root / "target/plugins"

def file_url(path):
    return path.resolve().as_uri()

instances = {
    "cli": {
        "package": file_url(plugins / "cli"),
        "config": {"conversation_id": "live-e2e", "sender": "operator"},
        "components": {"main": {"stream": {"default": {
            "socket": str(data / "cli-main.sock"), "peer_uids": [uid]
        }}}},
    },
    "shell": {
        "package": file_url(plugins / "shell"),
        "config": {},
        "components": {"main": {"stream": {"default": {
            "socket": str(data / "shell-main.sock"), "peer_uids": [uid]
        }}}},
    },
    "codex": {
        "package": file_url(plugins / "openai-codex"),
        "aliases": ["openai-codex"],
        "config": {"credentials": {"subscription": "openai-codex:personal"},
                   "models": ["gpt-5.6-luna"], "timeout_ms": 300000},
        "components": {"main": {"http": {
            "origins": ["https://chatgpt.com", "https://auth.openai.com"],
            "methods": ["POST"], "max_request_bytes": 16777216,
            "max_response_bytes": 16777216, "max_timeout_ms": 300000
        }}},
    },
    "rlm": {"package": file_url(plugins / "rlm"), "config": {}},
    "memory": {"package": file_url(plugins / "memory"), "config": {}},
}
capabilities = {
    "shell/main": ["shell.execute"],
    "memory/main": ["memory.recall", "memory.get", "memory.remember",
                     "memory.supersede", "memory.forget"],
}
memory_scope = {name: {"scopes": ["project:pluribus-e2e"]}
                for name in capabilities["memory/main"]}
config = {
    "agent_id": "e2e-live",
    "identity": "You are a local test assistant. Answer concisely. Use the yield reply for the current CLI message. Use shell only for requested, read-only checks. Use memory only when asked, and invoke the requested memory tool rather than relying on conversation history.",
    "model": "gpt-5.6-luna",
    "maximum_blob_bytes": 16777216,
    "plugin_instances": instances,
    "model_instance": "codex/main",
    "capability_instances": ["shell/main", "memory/main"],
    "trusted_capabilities": capabilities,
    "trusted_constraints": {"shell/main": {"shell.execute": {}},
                            "memory/main": memory_scope},
}
(data / "config.json").write_text(json.dumps(config, indent=2) + "\n")
os.chmod(data / "config.json", 0o600)
(live / "workspace" / "README.txt").write_text(
    "Isolated workspace for the local live E2E instance.\n")
print(f"Configured {data}")
print(f"Runtime UID: {uid}; shell workspace: {workspace}")
PY
