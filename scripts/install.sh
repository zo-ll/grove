#!/usr/bin/env bash
#
# Build and install grove.
#
# Two binaries, because grove is two programs: `groved` owns the ptys and
# outlives the terminal, and `grove` is the TUI that attaches to it. Installing
# one without the other gives you a UI that cannot find its daemon, which is
# the confusing half of the pair, so this installs both or neither.
#
#   scripts/install.sh                 build in release and install to ~/.local/bin
#   scripts/install.sh --prefix DIR    install somewhere else
#   scripts/install.sh --debug         build without optimisations, for a faster loop
#   scripts/install.sh --sample-config write a starter config.lua if none exists
#   scripts/install.sh --uninstall     remove what this script installed
#
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local/bin}"
PROFILE=release
SAMPLE=0
UNINSTALL=0
BINARIES=(grove groved)

usage() {
    # The header is the help text, so the two cannot drift: everything from
    # the summary down to the last option line.
    sed -n '3,/^set -euo/p' "$0" | sed 's|^# \{0,1\}||; /^set -euo/d'
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

root="$(cd "$(dirname "$0")/.." && pwd)"

if [ "$UNINSTALL" -eq 1 ]; then
    for binary in "${BINARIES[@]}"; do
        target="$PREFIX/$binary"
        if [ -e "$target" ]; then
            rm -f "$target"
            echo "removed $target"
        fi
    done
    # The config is the user's, not ours: it survives an uninstall, because
    # deleting someone's settings is not what "uninstall the program" means.
    echo "left your config alone: ${XDG_CONFIG_HOME:-$HOME/.config}/grove/config.lua"
    exit 0
fi

command -v cargo >/dev/null 2>&1 || {
    echo "install.sh: cargo is not on PATH — grove builds from source" >&2
    exit 1
}

# The MSRV is checked here rather than left to a compile error three minutes
# in: "unexpected token" from a let-chain is a worse way to learn your
# toolchain is old.
msrv="$(sed -n 's/^rust-version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
have="$(rustc --version | cut -d' ' -f2)"
if [ -n "$msrv" ]; then
    oldest="$(printf '%s\n%s\n' "$msrv" "$have" | sort -V | head -1)"
    if [ "$oldest" != "$msrv" ]; then
        echo "install.sh: grove needs rustc $msrv or newer, found $have" >&2
        exit 1
    fi
fi

echo "building grove ($PROFILE) …"
if [ "$PROFILE" = release ]; then
    cargo build --manifest-path "$root/Cargo.toml" --release --workspace
else
    cargo build --manifest-path "$root/Cargo.toml" --workspace
fi

built="${CARGO_TARGET_DIR:-$root/target}/$PROFILE"
mkdir -p "$PREFIX"
for binary in "${BINARIES[@]}"; do
    [ -x "$built/$binary" ] || {
        echo "install.sh: $built/$binary was not built" >&2
        exit 1
    }
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
  -- Every setting here is optional; these are the defaults, written out so
  -- there is something to edit.
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
-- knowing what a forge is — it runs per worktree, and it is bounded, so keep
-- it quick or it will render empty.
-- grove.column("pr", function(wt)
--   return grove.sh("gh pr view " .. wt.branch .. " --json state -q .state")
-- end)
LUA
        echo "wrote $config_dir/config.lua"
    fi
fi

case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *) echo; echo "note: $PREFIX is not on your PATH" ;;
esac

cat <<EOF

grove is two programs. Start the daemon on a directory that holds your clones:

    groved ~/code &

then open the TUI on the same directory:

    grove ~/code

The daemon keeps running when the TUI exits — that is the point of it. Press
^g ? inside grove for the keys, and ^g q to leave the TUI without stopping
your terminals.
EOF
