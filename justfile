# Chithi - Desktop Email Client
# Run `just --list` to see available targets

# Default target
default:
    @just --list

# Run development server
dev:
    pnpm tauri dev

# Build production app (all formats)
build:
    pnpm tauri build

# Build only the frontend
build-frontend:
    pnpm build

# Install frontend dependencies
install:
    pnpm install

# Clean build artifacts
clean:
    rm -rf dist
    rm -rf src-tauri/target
    rm -rf node_modules

# Clean only dist directory (package outputs)
clean-dist:
    rm -rf dist

# Clean only Rust build artifacts
clean-rust:
    rm -rf src-tauri/target

# Run frontend tests
test:
    pnpm test -- --run

# Run Rust tests
test-rust:
    cd src-tauri && cargo test

# Run all tests
test-all: test test-rust

# Convert semver prerelease to RPM-compatible version
# 0.3.0-alpha.1 -> 0.3.0~alpha.1 (sorts before 0.3.0)
_rpm-version:
    #!/usr/bin/env bash
    VERSION=$(grep '"version"' src-tauri/tauri.conf.json | head -1 | sed 's/.*: *"\([^"]*\)".*/\1/')
    echo "${VERSION//-/~}"

# Convert semver prerelease to DEB-compatible version
# 0.3.0-alpha.1 -> 0.3.0~alpha.1 (sorts before 0.3.0)
_deb-version:
    #!/usr/bin/env bash
    VERSION=$(grep '"version"' src-tauri/tauri.conf.json | head -1 | sed 's/.*: *"\([^"]*\)".*/\1/')
    echo "${VERSION//-/~}"

# Build RPM package using Docker
# Usage: just build-rpm [base_image]
# Examples:
#   just build-rpm              # Uses fedora:43 (default)
#   just build-rpm fedora:42
build-rpm base_image="fedora:43":
    #!/usr/bin/env bash
    set -e
    BASE_IMAGE="{{base_image}}"
    DISTRO_NAME=$(echo "$BASE_IMAGE" | tr ':' '-')
    OUTPUT_DIR="dist/$DISTRO_NAME"

    SEMVER=$(grep '"version"' src-tauri/tauri.conf.json | head -1 | sed 's/.*: *"\([^"]*\)".*/\1/')
    RPM_VERSION="${SEMVER//-/\~}"

    echo "Building RPM for $BASE_IMAGE..."
    echo "Semver: $SEMVER -> RPM version: $RPM_VERSION"
    mkdir -p "$OUTPUT_DIR"

    docker build \
        --build-arg BASE_IMAGE="$BASE_IMAGE" \
        --build-arg PKG_VERSION="$RPM_VERSION" \
        -f Dockerfile.rpm \
        -t "chithi-rpm-$DISTRO_NAME" \
        .

    CONTAINER_ID=$(docker create "chithi-rpm-$DISTRO_NAME")
    docker cp "$CONTAINER_ID:/app/src-tauri/target/release/bundle/rpm/." "$OUTPUT_DIR/"
    docker rm "$CONTAINER_ID"

    # Rename RPM files to include distro tag
    distro=$(echo "$BASE_IMAGE" | cut -d: -f1)
    ver=$(echo "$BASE_IMAGE" | cut -d: -f2)
    case "$distro" in
        fedora) distro_tag="fc${ver}" ;;
        centos|rocky|alma) distro_tag="el${ver}" ;;
        *) distro_tag="${distro}${ver}" ;;
    esac
    for f in "$OUTPUT_DIR"/*.rpm; do
        [ -f "$f" ] || continue
        basename=$(basename "$f")
        newname=$(echo "$basename" | sed "s/\.\(x86_64\|aarch64\)\.rpm/.${distro_tag}.\1.rpm/")
        if [ "$basename" != "$newname" ]; then
            mv "$f" "$OUTPUT_DIR/$newname"
        fi
    done

    echo ""
    echo "RPM package(s) for $BASE_IMAGE available in $OUTPUT_DIR/"
    ls -la "$OUTPUT_DIR/"*.rpm 2>/dev/null || echo "No RPM files found"

# Build DEB package using Docker
# Usage: just build-deb [base_image]
# Examples:
#   just build-deb                  # Uses debian:13 (default)
#   just build-deb debian:12
#   just build-deb ubuntu:24.04
#   just build-deb ubuntu:22.04
build-deb base_image="debian:13":
    #!/usr/bin/env bash
    set -e
    BASE_IMAGE="{{base_image}}"
    DISTRO_NAME=$(echo "$BASE_IMAGE" | tr ':' '-')
    OUTPUT_DIR="dist/$DISTRO_NAME"

    SEMVER=$(grep '"version"' src-tauri/tauri.conf.json | head -1 | sed 's/.*: *"\([^"]*\)".*/\1/')
    DEB_VERSION="${SEMVER//-/\~}"

    echo "Building DEB for $BASE_IMAGE..."
    echo "Semver: $SEMVER -> DEB version: $DEB_VERSION"
    mkdir -p "$OUTPUT_DIR"

    docker build \
        --build-arg BASE_IMAGE="$BASE_IMAGE" \
        --build-arg PKG_VERSION="$DEB_VERSION" \
        -f Dockerfile.deb \
        -t "chithi-deb-$DISTRO_NAME" \
        .

    CONTAINER_ID=$(docker create "chithi-deb-$DISTRO_NAME")
    docker cp "$CONTAINER_ID:/app/src-tauri/target/release/bundle/deb/." "$OUTPUT_DIR/"
    docker rm "$CONTAINER_ID"

    # Rename DEB files to include distro name
    distro_tag=$(echo "$DISTRO_NAME" | tr '-' '')
    for f in "$OUTPUT_DIR"/*.deb; do
        [ -f "$f" ] || continue
        basename=$(basename "$f")
        newname=$(echo "$basename" | sed "s/_\(amd64\|arm64\|armhf\)\.deb/_${distro_tag}_\1.deb/")
        if [ "$basename" != "$newname" ]; then
            mv "$f" "$OUTPUT_DIR/$newname"
        fi
    done

    echo ""
    echo "DEB package(s) for $BASE_IMAGE available in $OUTPUT_DIR/"
    ls -la "$OUTPUT_DIR/"*.deb 2>/dev/null || echo "No DEB files found"

# Build Arch Linux package using Docker
# Usage: just build-arch [base_image]
# Examples:
#   just build-arch                 # Uses archlinux:latest (default)
build-arch base_image="archlinux:latest":
    #!/usr/bin/env bash
    set -e
    BASE_IMAGE="{{base_image}}"
    DISTRO_NAME=$(echo "$BASE_IMAGE" | tr ':' '-')
    OUTPUT_DIR="dist/$DISTRO_NAME"

    SEMVER=$(grep '"version"' src-tauri/tauri.conf.json | head -1 | sed 's/.*: *"\([^"]*\)".*/\1/')

    echo "Building Arch package for $BASE_IMAGE..."
    echo "Version: $SEMVER"
    mkdir -p "$OUTPUT_DIR"

    docker build \
        --build-arg BASE_IMAGE="$BASE_IMAGE" \
        --build-arg PKG_VERSION="$SEMVER" \
        -f Dockerfile.arch \
        -t "chithi-arch-$DISTRO_NAME" \
        .

    CONTAINER_ID=$(docker create "chithi-arch-$DISTRO_NAME")
    docker cp "$CONTAINER_ID:/build/output/." "$OUTPUT_DIR/"
    docker rm "$CONTAINER_ID"

    echo ""
    echo "Arch package(s) for $BASE_IMAGE available in $OUTPUT_DIR/"
    ls -la "$OUTPUT_DIR/"*.pkg.tar.zst 2>/dev/null || echo "No Arch packages found"

# Build AppImage
build-appimage:
    pnpm tauri build --bundles appimage

# Build all packages for multiple distributions
build-all-rpm:
    just build-rpm fedora:43
    just build-rpm fedora:42

build-all-deb:
    just build-deb debian:13
    just build-deb debian:12
    just build-deb ubuntu:24.04
    just build-deb ubuntu:22.04

build-all-arch:
    just build-arch

build-all: build-all-deb build-all-rpm build-all-arch

# Collect all built packages into dist/release/ for GitHub upload
collect-release:
    #!/usr/bin/env bash
    set -e
    mkdir -p dist/release
    find dist -maxdepth 2 -path 'dist/release' -prune -o \( -name '*.rpm' -o -name '*.deb' -o -name '*.pkg.tar.zst' \) -print | while read f; do
        cp "$f" dist/release/
    done
    echo "Release packages collected in dist/release/:"
    ls -la dist/release/

# Sign all packages in dist/release/ with GPG detached signatures
sign:
    #!/usr/bin/env bash
    set -e
    for f in dist/release/*.rpm dist/release/*.deb dist/release/*.pkg.tar.zst; do
        [ -f "$f" ] || continue
        echo "Signing $f ..."
        gpg --armor --detach-sign "$f"
    done
    echo ""
    echo "Signed packages:"
    ls -la dist/release/*.asc 2>/dev/null || echo "No signatures found"

# Lint frontend code
lint:
    pnpm exec eslint src/

# Check Rust code
check-rust:
    cd src-tauri && cargo check

# Format Rust code
format-rust:
    cd src-tauri && cargo fmt

# Lint Rust code
lint-rust:
    cd src-tauri && cargo clippy

# Run all checks
check: check-rust

# Format all code
format-all: format-rust

# Show project info
info:
    @echo "Chithi - Desktop Email Client"
    @echo ""
    @echo "Frontend: Vue 3 + Vite"
    @echo "Backend: Tauri 2 (Rust)"
    @echo ""
    @echo "Node version: $(node --version)"
    @echo "pnpm version: $(pnpm --version)"
    @echo "Rust version: $(rustc --version)"
    @echo "Cargo version: $(cargo --version)"

# List available build targets
list-targets:
    @echo "RPM targets (Fedora):"
    @echo "  just build-rpm fedora:43  (default)"
    @echo "  just build-rpm fedora:42"
    @echo ""
    @echo "DEB targets (Debian/Ubuntu):"
    @echo "  just build-deb debian:13  (default)"
    @echo "  just build-deb debian:12"
    @echo "  just build-deb ubuntu:24.04"
    @echo "  just build-deb ubuntu:22.04"
    @echo ""
    @echo "Arch Linux targets:"
    @echo "  just build-arch          (archlinux:latest)"
