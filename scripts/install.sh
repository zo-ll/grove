#!/usr/bin/env bash
#
# Build and install grove, from a checkout or straight off the internet:
#
#     curl -fsSL https://raw.githubusercontent.com/zo-ll/grove/main/scripts/install.sh | bash
#
set -euo pipefail

REPO_URL="${GROVE_REPO:-https://github.com/zo-ll/grove.git}"
REF="${GROVE_REF:-main}"
# Kept rather than built in a temp directory: it makes the second install fast,
# and it means `--uninstall` and a later `git log` have something to point at.
SRC="${GROVE_SRC:-${XDG_CACHE_HOME:-$HOME/.cache}/grove/src}"
PREFIX="${PREFIX:-$HOME/.local/bin}"
PROFILE=release
SAMPLE=0
UNINSTALL=0
BINARIES=(grove groved)

usage() {
    # Written out rather than read back from this file: piped through `bash`
    # there is no file to read, and a --help that only works from a checkout
    # is the wrong half to have working.
    cat <<'USAGE'
grove install

    curl -fsSL .../scripts/install.sh | bash          install from the internet
    curl -fsSL .../scripts/install.sh | bash -s -- --sample-config
    scripts/install.sh                                install from a checkout

  --prefix DIR      where the binaries go (default ~/.local/bin)
  --debug           skip optimisation, for a faster loop
  --sample-config   write a starter config.lua if none exists
  --uninstall       remove the binaries, keep your config
  --help            this

  GROVE_REF=branch  build something other than main
  GROVE_SRC=DIR     where the source is kept (default ~/.cache/grove/src)
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)
            [ $# -ge 2 ] || { echo "install.sh: --prefix needs a directory" >&2; exit 2; }
            PREFIX="$2"
            shift 2
            ;;
        --debug) PROFILE=debug; shift ;;
        --sample-config) SAMPLE=1; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "install.sh: unknown option $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [ "$UNINSTALL" -eq 1 ]; then
    for binary in "${BINARIES[@]}"; do
        if [ -e "$PREFIX/$binary" ]; then
            rm -f "$PREFIX/$binary"
            echo "removed $PREFIX/$binary"
        fi
    done
    # The config is the user's, not ours. Deleting someone's settings is not
    # what "uninstall the program" means, and a source checkout they may have
    # edited is theirs too.
    echo "kept ${XDG_CONFIG_HOME:-$HOME/.config}/grove/config.lua"
    [ -d "$SRC" ] && echo "kept the source in $SRC"
    exit 0
fi

need() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "install.sh: $1 is not on PATH — grove builds from source and needs it" >&2
        exit 1
    }
}
need cargo
need git

# Run from inside a checkout, use it. Piped through `bash` there is no
# checkout and $0 is the shell, so the source is fetched instead — that branch
# is the whole reason this script can be curled.
root=""
if [ -f "${BASH_SOURCE[0]:-}" ]; then
    candidate="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    if [ -f "$candidate/Cargo.toml" ] && grep -q '^name = "grove"' "$candidate/crates/grove/Cargo.toml" 2>/dev/null; then
        root="$candidate"
    fi
fi

if [ -z "$root" ]; then
    if [ -d "$SRC/.git" ]; then
        echo "updating $SRC …"
        git -C "$SRC" fetch --quiet origin "$REF"
        git -C "$SRC" checkout --quiet FETCH_HEAD
    else
        echo "cloning grove into $SRC …"
        mkdir -p "$(dirname "$SRC")"
        git clone --quiet --depth 1 --branch "$REF" "$REPO_URL" "$SRC"
    fi
    root="$SRC"
fi
echo "building from $root ($(git -C "$root" log -1 --format=%h 2>/dev/null || echo 'no git'))"

# The MSRV is checked here rather than left to a compile error three minutes
# in: "unexpected token" from a let-chain is a worse way to learn your
# toolchain is old.
msrv="$(sed -n 's/^rust-version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
have="$(rustc --version | cut -d' ' -f2)"
if [ -n "$msrv" ] && [ "$(printf '%s\n%s\n' "$msrv" "$have" | sort -V | head -1)" != "$msrv" ]; then
    echo "install.sh: grove needs rustc $msrv or newer, found $have" >&2
    exit 1
fi

echo "building grove ($PROFILE) …"
if [ "$PROFILE" = release ]; then
    cargo build --manifest-path "$root/Cargo.toml" --release --workspace
else
    cargo build --manifest-path "$root/Cargo.toml" --workspace
fi

built="${CARGO_TARGET_DIR:-$root/target}/$PROFILE"
mkdir -p "$PREFIX"
# Both or neither: one without the other is a UI that cannot find its daemon,
# which is the confusing half of the pair to be missing.
for binary in "${BINARIES[@]}"; do
    [ -x "$built/$binary" ] || {
        echo "install.sh: $built/$binary was not built" >&2
        exit 1
    }
done
for binary in "${BINARIES[@]}"; do
    install -m 755 "$built/$binary" "$PREFIX/$binary"
    echo "installed $PREFIX/$binary"
done

config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/grove"
if [ "$SAMPLE" -eq 1 ]; then
    if [ -e "$config_dir/config.lua" ]; then
        echo "kept your existing $config_dir/config.lua"
    else
        mkdir -p "$config_dir"
        cat > "$config_dir/config.lua" <<'LUA'
local grove = require("grove")

grove.setup({
  -- Every setting is optional; these are the defaults, written out so there
  -- is something to edit.
  -- shell         = os.getenv("SHELL"),
  -- scratch_cwd   = "~",
  -- scrollback    = 10000,
  -- worktree_path = "~/grove/{repo}/{branch_slug}",

  theme = {
    corners = "rounded", -- rounded · square
    density = "airy",    -- airy · compact
  },
})

-- A key of your own. `^g` is how you address grove from a terminal pane.
grove.keymap("^g w", function()
  grove.palette("new ")
end)

-- A column of your own. This is how forge data reaches grove without grove
-- knowing what a forge is. It runs once per worktree and is bounded, so keep
-- it quick or it renders empty.
-- grove.column("pr", function(wt)
--   return grove.sh("gh pr view " .. wt.branch .. " --json state -q .state")
-- end)
LUA
        echo "wrote $config_dir/config.lua"
    fi
fi

case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *) printf '\nnote: %s is not on your PATH\n' "$PREFIX" ;;
esac

cat <<EOF

grove is two programs. Start the daemon on a directory that holds your clones,
then open the TUI on the same directory:

    groved ~/code &
    grove ~/code

The daemon keeps running when the TUI exits — that is the point of it. Inside
grove, ^g ? lists the keys for the screen you are on and ^g q leaves without
stopping your terminals.
EOF
