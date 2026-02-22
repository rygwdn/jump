# world-nav

Worktree navigation and path shortening for Fish and Zsh. Uses frecency-based ranking to jump between git repositories quickly.

## Features

- **`j`** — fuzzy-jump to any git repo under your configured source paths
- **`jc`** — same, but open in VS Code
- **Path shortening** — compact prompt-friendly path segments with prefix/infix/suffix components
- **Frecency tracking** — repos you visit often and recently rank higher
- **Shell integration** — hooks for Fish and Zsh that update prompt variables automatically

## Installation

### From a release binary

Download the appropriate binary from the [releases page](https://github.com/rygwdn/jump/releases):

| Platform | Binary |
|---|---|
| Linux x86_64 | `world-nav-linux-x86_64` |
| Linux x86_64 (Alpine/musl) | `world-nav-linux-x86_64-musl` |
| macOS Apple Silicon | `world-nav-macos-aarch64` |

Place it somewhere on your `$PATH` (e.g. `~/.local/bin/world-nav`).

### From source

```bash
./install.sh
```

Requires `rustup`. The script installs the correct toolchain, runs fmt/clippy/tests, then installs the binary to `~/.cargo/bin/world-nav`.

## Shell integration

Add to your shell config, then restart your shell.

**Fish** (`~/.config/fish/config.fish`):
```fish
if status is-interactive
    which world-nav &>/dev/null && world-nav shell-init --shell fish --require-version ~/.config/world-nav/Cargo.toml | source
end
```

**Zsh** (`~/.zshrc`):
```zsh
if [[ $- == *i* ]] && command -v world-nav &>/dev/null; then
    eval "$(world-nav shell-init --shell zsh --require-version ~/.config/world-nav/Cargo.toml)"
fi
```

> **Tip:** `--require-version` can be a semver string like `^0.5.2` or a path to a `Cargo.toml`. When pointed at a `Cargo.toml`, it extracts the version automatically and warns you if the installed binary is out of date.

This creates:
- `j [query]` — interactive fuzzy navigation
- `jc [query]` — navigate and open in VS Code
- Frecency hooks that fire on directory change and prompt render
- `$WORKTREE_PATH_PREFIX`, `$WORKTREE_PATH_SHORTENED`, `$WORKTREE_PATH_NORMAL`, `$WORKTREE_PATH_COLORED` — for use in custom prompts

Customize command names:
```bash
world-nav shell-init --shell fish --navigate jump --code code-jump
```

Disable individual components:
```bash
world-nav shell-init --shell fish --no-code --no-segments
```

## Configuration

Default config: `~/.config/world-nav/config.toml`
Override: `WORLD_NAV_CONFIG=/path/to/config.toml`

```toml
world_path = "~/world/trees"     # worktree root (optional)
src_paths = ["~/src"]            # directories scanned for git repos
depth_limit = 3                  # how deep to scan
frecency_db_path = "~/.local/share/world-nav/frecency.db"
```

View active config:
```bash
world-nav config
```

## Usage

```
world-nav nav [--list] [--scores] [--filter] [--multi] [--height HEIGHT] [QUERY...]
world-nav shortpath [--max-segments N] [--section SECTION] [--stdin] [PATH]
world-nav shell-init --shell <fish|zsh> [OPTIONS]
world-nav config
```

### `nav`

| Flag | Description |
|---|---|
| `--list` | Print all candidates, no interactive UI |
| `--scores` | Include frecency scores in output |
| `--filter` | Filter by query without interactive UI |
| `--multi` | Allow selecting multiple directories (TAB/Shift-TAB) |
| `--height` | UI height, e.g. `40%` or `20` (lines) |

### `shortpath`

Shorten a path into prefix/infix/suffix segments for use in prompts.

| Flag | Description |
|---|---|
| `-n, --max-segments N` | Number of trailing segments to leave unshortened (default: 1) |
| `-s, --section SECTION` | Output `prefix`, `shortened`, `normal`, `full`, `colored`, or `all` |
| `--stdin` | Read paths from stdin, one per line |

## Development

```bash
cargo fmt                        # format
cargo clippy -- -D warnings      # lint
cargo test                       # test
cargo build --release            # build
```

CI runs fmt + clippy + tests on every push and PR. Release binaries are built automatically on `v*` tags and as a rolling `dev` pre-release on every push to `main`.
