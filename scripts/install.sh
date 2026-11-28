#!/usr/bin/env bash
set -euo pipefail

SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST_DIR="${EXT_MODELS_PLUGIN_DIR:-$HOME/.gemini/config/plugins/external-model-subagents}"

if [[ -d "$HOME/.cargo/bin" ]]; then
  export PATH="$HOME/.cargo/bin:$PATH"
fi

echo "[1/3] Building release binary (lto + strip + opt-level=z)..."
cargo build --release --manifest-path "$SRC_DIR/Cargo.toml"

mkdir -p "$SRC_DIR/bin"
rm -f "$SRC_DIR/bin/ext-models-mcp"
cp "$SRC_DIR/target/release/ext-models-mcp" "$SRC_DIR/bin/ext-models-mcp"
chmod 755 "$SRC_DIR/bin/ext-models-mcp"
if [[ "$(uname -s)" == "Darwin" ]] && command -v codesign >/dev/null 2>&1; then
  codesign --force --sign - "$SRC_DIR/bin/ext-models-mcp" >/dev/null 2>&1 || true
fi

echo "[2/3] Staging clean plugin bundle to: $DEST_DIR"
mkdir -p "$DEST_DIR/bin" "$DEST_DIR/skills/external-model-subagents" "$DEST_DIR/scripts"

# Remove legacy v1.x Node MCP servers and redundant subagent markdowns if upgrading
rm -rf "$DEST_DIR/agents" \
       "$DEST_DIR/scripts/claude-mcp-server.mjs" \
       "$DEST_DIR/scripts/trae-mcp-server.mjs" \
       "$DEST_DIR/scripts/zcode-mcp-server.mjs"

# Unlink old binary inode first so macOS arm64 vnode code-signing cache never SIGKILLs hot upgrades
rm -f "$DEST_DIR/bin/ext-models-mcp"
cp "$SRC_DIR/bin/ext-models-mcp" "$DEST_DIR/bin/ext-models-mcp"
chmod 755 "$DEST_DIR/bin/ext-models-mcp"
if [[ "$(uname -s)" == "Darwin" ]] && command -v codesign >/dev/null 2>&1; then
  codesign --force --sign - "$DEST_DIR/bin/ext-models-mcp" >/dev/null 2>&1 || true
fi
cp "$SRC_DIR/plugin.json" "$DEST_DIR/plugin.json"
cp "$SRC_DIR/mcp_config.json" "$DEST_DIR/mcp_config.json"
cp "$SRC_DIR/harnesses.example.json" "$DEST_DIR/harnesses.example.json"
cp "$SRC_DIR/http-harnesses.example.json" "$DEST_DIR/http-harnesses.example.json"
cp "$SRC_DIR/models.example.json" "$DEST_DIR/models.example.json"
cp "$SRC_DIR/README.md" "$DEST_DIR/README.md"
cp "$SRC_DIR/ARCHITECTURE.md" "$DEST_DIR/ARCHITECTURE.md"
cp "$SRC_DIR/CHANGELOG.md" "$DEST_DIR/CHANGELOG.md"
cp "$SRC_DIR/LICENSE" "$DEST_DIR/LICENSE"
cp "$SRC_DIR/skills/external-model-subagents/SKILL.md" "$DEST_DIR/skills/external-model-subagents/SKILL.md"
cp "$SRC_DIR/scripts/install.sh" "$DEST_DIR/scripts/install.sh"
chmod 755 "$DEST_DIR/scripts/install.sh"

echo "[3/3] Verifying binary health..."
BIN_SIZE="$(ls -lh "$DEST_DIR/bin/ext-models-mcp" | awk '{print $5}')"
echo "Installed binary: $DEST_DIR/bin/ext-models-mcp ($BIN_SIZE)"
"$DEST_DIR/bin/ext-models-mcp" --version
echo "Done."
