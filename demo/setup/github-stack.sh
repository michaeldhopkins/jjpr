#!/usr/bin/env bash
# A three-PR stack (auth, profile, settings) on a stand-in GitHub that answers over HTTP, with
# jjpr built from this tree. The recording kit's helpers.sh documents jjpr_http_forge.
set -euo pipefail
source "$RECORD_HELPERS"
jj_stack "$FIXTURE/app" "auth:Add login form" "profile:Add profile page" "settings:Add settings page"
jjpr_http_forge "$ROOT" "$FIXTURE/app" auth profile settings
