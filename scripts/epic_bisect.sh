#!/bin/sh
# Which commit of an epic broke it (TESTING.md, "When an epic's smoke is red")? Feature branches are squash-merged into an epic, so an
# epic is one commit per piece of work, none of them booted before the epic's own smoke. This bisects from the epic's fork point on
# origin/main (good) to HEAD (bad), running one make target per step.
#
# Usage: scripts/epic_bisect.sh [MAKE_TARGET]     (default: os-ktest-4, the kernel-only boot at four harts)
# Run it on the epic branch with a clean tree. It leaves the bisect open so the result can be read: `git bisect reset` when done.
set -e
target=${1:-os-ktest-4}
cd "$(git rev-parse --show-toplevel)"
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "epic_bisect: the tree has uncommitted changes; commit or stash them first" >&2
    exit 2
fi
git fetch -q origin main
base=$(git merge-base HEAD origin/main)
n=$(git rev-list --count "$base..HEAD")
if [ "$n" -eq 0 ]; then
    echo "epic_bisect: HEAD has no commits beyond origin/main" >&2
    exit 2
fi
echo "epic_bisect: $n commits since $(git rev-parse --short "$base"); running 'make $target' at each step"
# --first-parent: a merge of main into the epic is one step, not a walk through main's history.
git bisect start --first-parent HEAD "$base"
git bisect run make "$target"
echo "epic_bisect: done (the first bad commit is named above). 'git bisect reset' returns to the branch."
