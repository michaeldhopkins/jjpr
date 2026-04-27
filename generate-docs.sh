#!/bin/bash
# Build the jjpr docs site and deploy it into michaeldhopkins.com.
#
# Steps:
#   1. Regenerate docs/src/version-footer.js from Cargo.toml.
#   2. Build the mdbook in docs/book/.
#   3. Mirror docs/book/ into ~/projects/michaeldhopkins.com/public/docs/jjpr/
#      with rsync --delete (removes stale files).
#
# Run this whenever you update the docs. Commit changes in jjpr (docs/
# sources) and michaeldhopkins.com (public/docs/jjpr/) separately.

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

# 3. Deploy to michaeldhopkins.com.
site_dir="$HOME/projects/michaeldhopkins.com/public/docs/jjpr"
if [[ ! -d "$HOME/projects/michaeldhopkins.com" ]]; then
    echo "warn: $HOME/projects/michaeldhopkins.com not found — skipping deploy" >&2
    exit 0
fi
mkdir -p "$site_dir"
rsync -a --delete docs/book/ "$site_dir/"
echo "Deployed to $site_dir"
