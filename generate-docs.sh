#!/bin/bash
# Build the jjpr docs book into docs/book/.
#
# Steps:
#   1. Regenerate docs/src/version-footer.js from Cargo.toml.
#   2. Build the mdbook in docs/book/.
#   3. If SITE_DIR is set, mirror docs/book/ into it with rsync --delete.
#      Only the release workflow sets it, so local runs never touch
#      michaeldhopkins.com; each release publishes the book there.
#
# Run this whenever you update the docs, to check the book builds.

set -euo pipefail

repo_root="$(cd "$(dirname "$0")" && pwd)"
cd "$repo_root"

if ! command -v mdbook &> /dev/null; then
    echo "error: mdbook is not on PATH" >&2
    echo "install with: cargo install mdbook" >&2
    exit 1
fi

# 1. Regenerate the sidebar version footer from Cargo.toml.
version=$(grep -E '^version = ' Cargo.toml | head -n1 | sed -E 's/version = "(.*)"/\1/')
if [[ -z "$version" ]]; then
    echo "error: could not parse version from Cargo.toml" >&2
    exit 1
fi

cat > docs/src/version-footer.js <<EOF
document.addEventListener('DOMContentLoaded', function() {
    var nav = document.querySelector('.nav-wide-wrapper') || document.querySelector('.nav-wrapper');
    if (nav) {
        var footer = document.createElement('div');
        footer.className = 'version-footer';
        footer.textContent = 'jjpr v$version';
        nav.parentNode.insertBefore(footer, nav.nextSibling);
    }
});
EOF
echo "Wrote docs/src/version-footer.js ($version)"

# 2. Build the book.
mdbook build docs/
echo "Built book in docs/book/"

# 3. Publish into a site checkout (release workflow only).
if [[ -z "${SITE_DIR:-}" ]]; then
    exit 0
fi
mkdir -p "$SITE_DIR"
rsync -a --delete docs/book/ "$SITE_DIR/"
echo "Deployed to $SITE_DIR"
