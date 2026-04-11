# Maintainer: Kushal Das <mail@kushaldas.in>
pkgname=chithi
pkgver=0.1.0
pkgrel=1
pkgdesc="Desktop email client with IMAP, JMAP and Microsoft 365 support"
arch=('x86_64' 'aarch64')
url="https://github.com/SUNET/chithi"
license=('GPL-3.0-only')
depends=(
    'webkit2gtk-4.1'
    'gtk3'
    'libayatana-appindicator'
    'openssl'
    'libsoup3'
)
makedepends=(
    'rust'
    'cargo'
    'nodejs'
    'npm'
    'base-devel'
)
source=("$pkgname-$pkgver.tar.gz::$url/archive/v$pkgver.tar.gz")
# TODO: replace with the real SHA-256 (or b2sums) once a v0.1.0 release
# tag exists on GitHub. Until then the package builds locally but cannot
# be submitted to the AUR.
sha256sums=('SKIP')

prepare() {
    cd "$pkgname-$pkgver"
    # pnpm is not a first-class Arch package; corepack (bundled with
    # nodejs >= 16) provides a stable shim so we don't need to pin a
    # specific pnpm version in makedepends.
    corepack enable
    corepack prepare pnpm@10.27.0 --activate

    # Pre-fetch all dependencies so build() can run offline.
    cargo fetch --locked --manifest-path src-tauri/Cargo.toml
    pnpm install --frozen-lockfile
}

build() {
    cd "$pkgname-$pkgver"
    export CARGO_NET_OFFLINE=true
    pnpm tauri build --bundles deb
}

package() {
    cd "$pkgname-$pkgver"
    install -Dm755 src-tauri/target/release/chithi "$pkgdir/usr/bin/chithi"
    install -Dm644 src-tauri/icons/icon.png "$pkgdir/usr/share/icons/hicolor/128x128/apps/chithi.png"
    install -Dm644 debian/chithi.desktop "$pkgdir/usr/share/applications/chithi.desktop"
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
