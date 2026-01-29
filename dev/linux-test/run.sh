#!/bin/bash
set -e

# Get the root of the repository
REPO_ROOT=$(git rev-parse --show-toplevel)
cd "$REPO_ROOT"

echo "==> Building and running Zerobrew Linux verification container..."

# Check if podman or docker is available
if command -v podman >/dev/null 2>&1; then
    ENGINE="podman"
elif command -v docker >/dev/null 2>&1; then
    ENGINE="docker"
else
    echo "Error: Neither podman nor docker found in PATH."
    exit 1
fi

echo "Using engine: $ENGINE"

# Build the image
$ENGINE build -t zerobrew-test -f dev/linux-test/Dockerfile .

# Run the image to execute verify commands
$ENGINE run --rm zerobrew-test echo "Zerobrew verification successful!"
