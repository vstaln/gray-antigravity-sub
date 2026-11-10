#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/antigravity-sub.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/antigravity-sub" ;;
    *) rel="x86_64-apple-darwin/antigravity-sub" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/antigravity-sub" ;;
    *) rel="x86_64-unknown-linux-musl/antigravity-sub" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/antigravity-sub.exe" "$here/target/release/antigravity-sub" "$here/target/debug/antigravity-sub.exe" "$here/target/debug/antigravity-sub"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "antigravity-sub binary not found in $here" >&2
exit 1
