#!/bin/bash
set -e

# zerobrew installer
# Usage: curl -sSL https://raw.githubusercontent.com/hanthor/zerobrew/installer/linux/install.sh | bash

ZEROBREW_REPO="https://github.com/hanthor/zerobrew.git"
ZEROBREW_BRANCH="linux-support"
: ${ZEROBREW_DIR:=$HOME/.zerobrew/src} # Repo location
: ${ZEROBREW_BIN:=$HOME/.local/bin}

# Detect OS and set default root
OS="$(uname)"
if [[ "$OS" == "Linux" ]]; then
    ZEROBREW_DEFAULT_ROOT="$HOME/.zerobrew"
else
    ZEROBREW_DEFAULT_ROOT="/opt/zerobrew"
fi
: ${ZEROBREW_ROOT:=$ZEROBREW_DEFAULT_ROOT}
: ${ZEROBREW_PREFIX:=$ZEROBREW_ROOT}

echo "Installing zerobrew..."

# Check for Rust/Cargo
if ! command -v cargo &> /dev/null; then
    echo "Rust not found. Installing via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
fi

# Ensure cargo is available
if ! command -v cargo &> /dev/null; then
    echo "Error: Cargo still not found after installing Rust"
    exit 1
fi

echo "Rust version: $(rustc --version)"

# Clone or update repo
if [[ -d "$ZEROBREW_DIR" ]]; then
    echo "Updating zerobrew..."
    cd "$ZEROBREW_DIR"
    git fetch origin "$ZEROBREW_BRANCH"
    git reset --hard "origin/$ZEROBREW_BRANCH"
else
    echo "Cloning zerobrew..."
    git clone --depth 1 -b "$ZEROBREW_BRANCH" "$ZEROBREW_REPO" "$ZEROBREW_DIR"
    cd "$ZEROBREW_DIR"
fi

# Build
echo "Building zerobrew..."
cargo build --release

# Create bin directory and install binary
mkdir -p "$ZEROBREW_BIN"
cp target/release/zb "$ZEROBREW_BIN/zb"
chmod +x "$ZEROBREW_BIN/zb"
echo "Installed zb to $ZEROBREW_BIN/zb"

# Detect shell config file
case "$SHELL" in
    */zsh)
        ZDOTDIR="${ZDOTDIR:-$HOME}"
        if [[ -f "$ZDOTDIR/.zshenv" ]]; then
            SHELL_CONFIG="$ZDOTDIR/.zshenv"
        else
            SHELL_CONFIG="$ZDOTDIR/.zshrc"
        fi
        ;;
    */bash)
        if [[ -f "$HOME/.bash_profile" ]]; then
            SHELL_CONFIG="$HOME/.bash_profile"
        else
            SHELL_CONFIG="$HOME/.bashrc"
        fi
        ;;
    *)
        SHELL_CONFIG="$HOME/.profile"
        ;;
esac

if [[ ! -w $SHELL_CONFIG ]]; then
    echo "Error, config not writable: $SHELL_CONFIG" >&2
    exit 1
fi

# Add to PATH in shell config if not already there
PATHS_TO_ADD=("$ZEROBREW_BIN" "$ZEROBREW_PREFIX/bin")
if ! grep -q "^# zerobrew$" "$SHELL_CONFIG" 2>/dev/null; then
    cat >>"$SHELL_CONFIG" <<EOF
# zerobrew
export ZEROBREW_ROOT=$ZEROBREW_ROOT
export ZEROBREW_PREFIX=$ZEROBREW_PREFIX
export ZEROBREW_BIN=$ZEROBREW_BIN
_zb_path_append() {
    local argpath="\$1"
    case ":\${PATH}:" in
        *:"\$argpath":*) ;;
        *) export PATH="\$argpath:\$PATH" ;;
    esac;
}
EOF
    for path_entry in "${PATHS_TO_ADD[@]}"; do
        if ! grep -q "$path_entry" "$SHELL_CONFIG" 2>/dev/null; then
            echo "_zb_path_append $path_entry" >>"$SHELL_CONFIG"
            echo "Added $path_entry to PATH in $SHELL_CONFIG"
        fi
    done
fi

# Export for current session so zb init works
export ZEROBREW_ROOT=$ZEROBREW_ROOT
export ZEROBREW_PREFIX=$ZEROBREW_PREFIX
export PATH="$ZEROBREW_BIN:$ZEROBREW_PREFIX/bin:$PATH"

# Set up zerobrew directories with correct ownership
echo ""
echo "Setting up zerobrew directories at $ZEROBREW_ROOT..."
CURRENT_USER=$(whoami)
if [[ ! -d "$ZEROBREW_ROOT" ]] || [[ ! -w "$ZEROBREW_ROOT" ]]; then
    if [[ "$ZEROBREW_ROOT" == "/opt/zerobrew"* ]]; then
        echo "Creating $ZEROBREW_ROOT (requires sudo)..."
        sudo mkdir -p "$ZEROBREW_ROOT/store" "$ZEROBREW_ROOT/db" "$ZEROBREW_ROOT/cache" "$ZEROBREW_ROOT/locks"
        sudo mkdir -p "$ZEROBREW_PREFIX/bin" "$ZEROBREW_PREFIX/Cellar"
        sudo chown -R "$CURRENT_USER" "$ZEROBREW_ROOT"
        if [[ "$ZEROBREW_ROOT" != "$ZEROBREW_PREFIX" ]]; then
            sudo chown -R "$CURRENT_USER" "$ZEROBREW_PREFIX"
        fi
    else
        echo "Creating $ZEROBREW_ROOT..."
        mkdir -p "$ZEROBREW_ROOT/store" "$ZEROBREW_ROOT/db" "$ZEROBREW_ROOT/cache" "$ZEROBREW_ROOT/locks"
        mkdir -p "$ZEROBREW_PREFIX/bin" "$ZEROBREW_PREFIX/Cellar"
    fi
fi

# Run zb init to finalize setup
echo ""
echo "Running zb init..."
"$ZEROBREW_BIN/zb" init

echo ""
echo "============================================"
echo "  zerobrew installed successfully!"
echo "============================================"
echo ""
echo "Run this to start using zerobrew now:"
echo ""
echo "    export PATH=\"$ZEROBREW_BIN:$ZEROBREW_PREFIX/bin:\$PATH\""
echo ""
echo "Or restart your terminal, to source updated ${SHELL_CONFIG}."
echo ""
echo "Then try: zb install ffmpeg"
echo ""
