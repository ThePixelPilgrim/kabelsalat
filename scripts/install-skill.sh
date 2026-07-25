#!/bin/sh
# Link the kabelsalat agent skill into the user's Claude skills directory, so
# an agent working in any project can find it. A symlink rather than a copy:
# the skill then tracks whatever this checkout has.
set -eu

repo="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
src="$repo/skills/kabelsalat"
dest="${HOME}/.claude/skills/kabelsalat"

[ -d "$src" ] || { echo "no skill at $src" >&2; exit 1; }
mkdir -p "$(dirname "$dest")"
ln -sfn "$src" "$dest"
echo "linked $dest -> $src"
