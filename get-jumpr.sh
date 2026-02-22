#!/bin/sh
# curl-pipe installer for jumpr
# Usage: curl -fsSL https://raw.githubusercontent.com/rygwdn/jump/main/get-jumpr.sh | sh
# Or with options:
#   curl -fsSL ... | sh -s -- --install-dir /usr/local/bin
#   curl -fsSL ... | sh -s -- --release v0.5.2
set -e

REPO="rygwdn/jump"
DEFAULT_RELEASE="dev"
DEFAULT_INSTALL_DIR="$HOME/.local/bin"

# Parse arguments
RELEASE="$DEFAULT_RELEASE"
INSTALL_DIR="$DEFAULT_INSTALL_DIR"

while [ $# -gt 0 ]; do
    case "$1" in
        --release)
            RELEASE="$2"
            shift 2
            ;;
        --install-dir)
            INSTALL_DIR="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1" >&2
            exit 1
            ;;
    esac
done

# Detect OS and architecture
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Linux*)
        case "$ARCH" in
            x86_64)
                # Prefer musl for better portability on Linux
                ARTIFACT="jumpr-linux-x86_64-musl"
                ;;
            *)
                echo "Unsupported Linux architecture: $ARCH" >&2
                echo "Supported: x86_64" >&2
                exit 1
                ;;
        esac
        ;;
    Darwin*)
        case "$ARCH" in
            arm64)
                ARTIFACT="jumpr-macos-aarch64"
                ;;
            x86_64)
                echo "No pre-built binary for macOS x86_64." >&2
                echo "Please build from source: cargo install --git https://github.com/$REPO" >&2
                exit 1
                ;;
            *)
                echo "Unsupported macOS architecture: $ARCH" >&2
                exit 1
                ;;
        esac
        ;;
    *)
        echo "Unsupported OS: $OS" >&2
        echo "Supported: Linux (x86_64), macOS (arm64)" >&2
        exit 1
        ;;
esac

DOWNLOAD_URL="https://github.com/$REPO/releases/download/$RELEASE/$ARTIFACT"

echo "Downloading jumpr ($ARTIFACT) from release '$RELEASE'..."

# Download
TMP_FILE="$(mktemp)"
trap 'rm -f "$TMP_FILE"' EXIT

if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$DOWNLOAD_URL" -o "$TMP_FILE"
elif command -v wget >/dev/null 2>&1; then
    wget -qO "$TMP_FILE" "$DOWNLOAD_URL"
else
    echo "Neither curl nor wget found. Please install one and retry." >&2
    exit 1
fi

# Install
mkdir -p "$INSTALL_DIR"
chmod +x "$TMP_FILE"
mv "$TMP_FILE" "$INSTALL_DIR/jumpr"

echo "Installed jumpr to $INSTALL_DIR/jumpr"

# Check if install dir is in PATH
case ":$PATH:" in
    *":$INSTALL_DIR:"*)
        ;;
    *)
        echo ""
        echo "Note: $INSTALL_DIR is not in your PATH."
        echo "Add it with:"
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac

# Print version
if command -v "$INSTALL_DIR/jumpr" >/dev/null 2>&1; then
    "$INSTALL_DIR/jumpr" --version
fi
