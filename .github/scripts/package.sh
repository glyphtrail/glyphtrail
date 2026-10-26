#!/usr/bin/env bash
# Package a built `glyphtrail` binary for a release: an archive (.tar.gz, or
# .zip on Windows) with the binary, README and license, plus a one-click MCP
# Bundle (.mcpb) for Claude Desktop (#303). Both land in dist/.
#
# The .mcpb is a zip of the platform binary plus a manifest derived from
# packaging/mcpb. It is arch-specific because it ships a native binary, so we
# build one per target and stamp the version / binary name / platform into the
# manifest.
#
# Inputs (environment): TARGET (Rust target triple), TAG (e.g. v0.3.0).
# Needs jq and zip (or 7z on Windows).
set -euo pipefail

: "${TARGET:?TARGET must be set}"
: "${TAG:?TAG must be set}"

case "$TARGET" in
  *windows*) bin="glyphtrail.exe"; platform="win32" ;;
  *darwin*)  bin="glyphtrail";     platform="darwin" ;;
  *)         bin="glyphtrail";     platform="linux" ;;
esac
built="target/$TARGET/release/$bin"
mkdir -p dist

staging="glyphtrail-$TAG-$TARGET"
mkdir -p "$staging"
cp "$built" README.md LICENSE-EUPL-1.2 "$staging/"
if [ "$platform" = "win32" ]; then
  7z a "dist/$staging.zip" "$staging" >/dev/null
else
  tar czf "dist/$staging.tar.gz" "$staging"
fi

work="$(mktemp -d)"
cp "$built" "$work/"
# shellcheck disable=SC2016 # ${__dirname} is an MCPB placeholder, not a shell variable
jq --arg v "${TAG#v}" --arg bin "$bin" --arg plat "$platform" \
  '.version = $v
   | .server.entry_point = $bin
   | .server.mcp_config.command = "${__dirname}/" + $bin
   | .compatibility.platforms = [$plat]' \
  packaging/mcpb/manifest.json > "$work/manifest.json"
out="$PWD/dist/glyphtrail-$TAG-$TARGET.mcpb"
if [ "$platform" = "win32" ]; then
  ( cd "$work" && 7z a -tzip "$out" . >/dev/null )
else
  ( cd "$work" && zip -r -q "$out" . )
fi
