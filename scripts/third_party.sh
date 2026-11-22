#!/usr/bin/env bash
# Regenerates assets/THIRD_PARTY.txt: the open-source components shipped in Draisine
# (bundled rclone, fonts, and every Rust crate compiled into the binary on any platform).
# Run after changing dependencies:  scripts/third_party.sh
set -euo pipefail
cd "$(dirname "$0")/.."
RCLONE_VERSION="$(sed -n 's/^RCLONE_VERSION="${RCLONE_VERSION:-\(v[^}]*\)}"/\1/p' scripts/package.sh)"
OUT=assets/THIRD_PARTY.txt

# Crates actually linked (normal deps, all targets), excluding Draisine itself.
used="$(cargo tree --target all -e normal --prefix none --format '{p}' \
  | sed 's/ (.*//; s/ (\*)//' | grep -v '^draisine ' | sort -u)"

{
  cat <<HDR
Draisine is built on open-source software. Thank you to all authors and contributors.

Bundled programs
----------------
rclone ${RCLONE_VERSION}
  License: MIT
  https://rclone.org  ·  https://github.com/rclone/rclone

Fonts
-----
Noto Sans (Regular, SemiBold)
  Copyright 2022 The Noto Project Authors
  License: SIL Open Font License 1.1
  https://github.com/notofonts/latin-greek-cyrillic

Material Design Icons 6.5.95 (icon font)
  Copyright Pictogrammers
  License: Apache-2.0
  https://pictogrammers.com/library/mdi/  ·  https://github.com/Templarian/MaterialDesign-Webfont

Rust crates
-----------
HDR
  cargo metadata --format-version 1 --locked \
    | jq -r --arg used "$used" '
        ($used | split("\n") | map(select(length > 0)) | map({key: ., value: true}) | from_entries) as $u
        | .packages[]
        | select($u[.name + " v" + .version])
        | "\(.name) \(.version)\t\(.license // "see repository")\t\(.repository // .homepage // "")"' \
    | sort -f \
    | awk -F'\t' '{ printf "%s\n  License: %s\n", $1, $2; if ($3 != "") printf "  %s\n", $3 }'
  cat <<FTR

Full license texts are available at the linked repositories and at
https://spdx.org/licenses/
FTR
} > "$OUT"
echo "$OUT: $(grep -c '^  License:' "$OUT") components"
