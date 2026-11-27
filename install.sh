#!/bin/sh
# openbutlerd installer for Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/m4rs1l/openbutler-daemon/main/install.sh | sh -s -- [options]
#
# Options:
#   --version <x.y.z>   install a specific release (default: the latest)
#   --prefix <dir>      install under this directory (default: ~/.openbutler)
#   --no-modify-path    leave the shell profiles alone
#   --quiet             no progress bar, no decoration
#
# The archive carries the daemon, the bundled sidecar and the Node runtime it
# runs on; all three land beside the binary, which is where the daemon looks for
# them.

set -eu

APP="openbutlerd"
REPO="${OPENBUTLER_DAEMON_REPO:-m4rs1l/openbutler-daemon}"
ROOT="${OPENBUTLER_INSTALL_DIR:-$HOME/.openbutler}"
BIN="$ROOT/bin"
MARKER="$ROOT/.installed-openbutlerd"

quiet=""
modify_path="yes"
requested_version=""

say() { [ -n "$quiet" ] || printf '%s\n' "$*"; }
die() { printf 'Error: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
Usage: install.sh [--version <x.y.z>] [--prefix <dir>] [--no-modify-path] [--quiet]

Installs openbutlerd (with its sidecar and Node runtime) from this
repository's releases.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version)
      [ $# -ge 2 ] || die "--version needs a value, e.g. --version 0.0.1."
      requested_version="$2"
      shift 2
      ;;
    --prefix)
      [ $# -ge 2 ] || die "--prefix needs a directory."
      ROOT="$2"
      BIN="$ROOT/bin"
      MARKER="$ROOT/.installed-openbutlerd"
      shift 2
      ;;
    --no-modify-path)
      modify_path="no"
      shift
      ;;
    --quiet)
      quiet="yes"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1 (see --help)."
      ;;
  esac
done

[ "$(uname -s)" = "Linux" ] || die "this installer builds Linux archives; macOS arrives with the stable release."

case "$(uname -m)" in
  x86_64|amd64) asset_arch="linux-amd64" ;;
  aarch64|arm64) asset_arch="linux-arm64" ;;
  *) die "unsupported architecture: $(uname -m)." ;;
esac

command -v curl >/dev/null 2>&1 || die "curl is required."
command -v tar >/dev/null 2>&1 || die "tar is required."

api="https://api.github.com/repos/$REPO"
if [ -n "$requested_version" ]; then
  version="${requested_version#v}"
else
  version="$(curl -fsSL "$api/releases/latest" | sed -n 's/.*"tag_name": *"v\([^"]*\)".*/\1/p' | head -1)"
  [ -n "$version" ] || die "could not read the latest release of $REPO."
fi

asset="$APP-$version-$asset_arch.tar.gz"

if [ -f "$MARKER" ] && [ -x "$BIN/$APP" ] && [ -f "$BIN/sidecar/dist/entry.js" ]; then
  installed="$(sed -n 's/^version=//p' "$MARKER" | head -1)"
  if [ "$installed" = "$version" ]; then
    say "$APP $version is already installed at $BIN/$APP; nothing to do."
    exit 0
  fi
fi

bar() {
  filled=$(( $1 * 20 / 100 ))
  i=0
  while [ "$i" -lt "$filled" ]; do printf '='; i=$((i + 1)); done
  while [ "$i" -lt 20 ]; do printf '-'; i=$((i + 1)); done
}

# One line, updated in place, the way the legacy installer drew it.
progress() {
  [ -n "$quiet" ] && return 0
  [ -t 1 ] || return 0
  printf '\r[%s] %3d%% %s' "$(bar "$1")" "$1" "$2"
}

download() { # url, output
  if [ -n "$quiet" ] || [ ! -t 1 ]; then
    curl -fsSL -o "$2" "$1" || die "download failed: $1"
    return 0
  fi
  total="$(curl -fsSIL "$1" 2>/dev/null | tr -d '\r' | awk 'tolower($1) == "content-length:" { value = $2 } END { print value }')"
  curl -fsSL -o "$2" "$1" &
  pid=$!
  while kill -0 "$pid" 2>/dev/null; do
    size="$(wc -c < "$2" 2>/dev/null || printf '0')"
    percent=0
    if [ -n "$total" ] && [ "$total" -gt 0 ] 2>/dev/null; then
      percent=$((size * 100 / total))
      [ "$percent" -gt 99 ] && percent=99
    fi
    progress "$percent" "downloading $(basename "$1")"
    sleep 0.2
  done
  wait "$pid" || die "download failed: $1"
  progress 100 "downloading $(basename "$1")"
  [ -n "$quiet" ] || [ ! -t 1 ] || printf '\n'
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

if command -v pgrep >/dev/null 2>&1 && pgrep -f "^$BIN/$APP( |$)" >/dev/null 2>&1; then
  die "$APP is running from this prefix; stop it first, then run this installer again."
fi

base="https://github.com/$REPO/releases/download/v$version"
download "$base/$asset" "$tmp/$asset"
download "$base/SHA256SUMS" "$tmp/SHA256SUMS"

expected="$(awk -v name="$asset" '$2 == name { print $1 }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || die "SHA256SUMS does not list $asset."
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
else
  actual="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
fi
[ "$actual" = "$expected" ] || die "checksum mismatch for $asset (expected $expected, got $actual)."

progress 100 "installing"
[ -n "$quiet" ] || [ ! -t 1 ] || printf '\n'

mkdir -p "$BIN"
stage="$tmp/$APP-$version-$asset_arch"
tar -xzf "$tmp/$asset" -C "$tmp"
[ -d "$stage" ] || die "the archive did not contain $APP-$version-$asset_arch."

install -m 0755 "$stage/$APP" "$BIN/$APP"
rm -rf "$BIN/sidecar" "$BIN/node"
cp -R "$stage/sidecar" "$BIN/sidecar"
cp -R "$stage/node" "$BIN/node"

reported="$("$BIN/$APP" --version 2>/dev/null || true)"
case "$reported" in
  *" $version "*) ;;
  *) die "the installed binary does not report $version (it said: ${reported:-nothing})." ;;
esac

{
  printf 'version=%s\n' "$version"
  printf 'asset=%s\n' "$asset_arch"
  printf 'installed_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "$MARKER"

if [ "$modify_path" = "yes" ]; then
  line="export PATH=\"$BIN:\$PATH\"  # openbutler"
  found=""
  for profile in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.zshrc"; do
    [ -f "$profile" ] || continue
    found="yes"
    grep -Fq "# openbutler" "$profile" 2>/dev/null && continue
    printf '\n%s\n' "$line" >> "$profile"
    say "PATH updated in $profile."
  done
  if [ -z "$found" ]; then
    printf '%s\n' "$line" > "$HOME/.profile"
    say "PATH written to $HOME/.profile (no shell profile existed)."
  fi
fi

say "openbutlerd $version installed at $BIN/$APP"
if [ "$modify_path" = "yes" ]; then
  say "Restart the terminal (or source your profile) so the updated PATH applies."
fi
say "Next: install the CLI from m4rs1l/openbutler-cli, then create the account with: openbutler admin create <username>"
