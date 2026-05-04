#!/usr/bin/env bash
# Publish a tagged release to the public GitHub mirror.
#
# Each release becomes a single commit on github/main (child of the previous
# release commit), tagged with the version. GitLab history is never rewritten;
# the GitHub mirror has its own linear history of releases only.
#
# Prerequisites:
#   - gitleaks installed (see scripts/install-gitleaks.sh or
#     https://github.com/gitleaks/gitleaks/releases)
#   - GitHub remote configured:
#       git remote add github git@github.com:hardwario/fiber-agent.git
#   - Hooks path activated:
#       git config core.hooksPath scripts/hooks
#
# Usage:
#   scripts/publish-to-github.sh v3.2.0
#   scripts/publish-to-github.sh v3.2.0 --dry-run

set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

tag=""
dry_run=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry_run=1 ;;
        -h|--help)
            sed -n '2,20p' "$0"
            exit 0
            ;;
        v*)
            [[ -z "$tag" ]] || { echo "publish: multiple tags given" >&2; exit 2; }
            tag="$arg"
            ;;
        *)
            echo "publish: unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

bail() { echo "publish: $1" >&2; exit 1; }

[[ -n "$tag" ]] || bail "missing tag argument (e.g. v3.2.0)"

command -v gitleaks >/dev/null 2>&1 || bail "gitleaks not installed."
git remote get-url github >/dev/null 2>&1 || bail "remote 'github' not configured."

[[ "$(git config --get core.hooksPath)" == "scripts/hooks" ]] \
    || bail "core.hooksPath is not 'scripts/hooks' — run: git config core.hooksPath scripts/hooks"

branch="$(git rev-parse --abbrev-ref HEAD)"
[[ "$branch" == "main" ]] || bail "must be on 'main' (current: $branch)."

[[ -z "$(git status --porcelain)" ]] || bail "working tree not clean — commit or stash first."

echo "publish: fetching origin (GitLab)..."
git fetch origin main --quiet

local_sha="$(git rev-parse main)"
origin_sha="$(git rev-parse origin/main)"
[[ "$local_sha" == "$origin_sha" ]] \
    || bail "local main ($local_sha) != origin/main ($origin_sha) — sync first."

git rev-parse --verify "$tag" >/dev/null 2>&1 \
    || bail "tag '$tag' does not exist locally — create it first: git tag -a $tag -m 'Release $tag'"

tag_sha="$(git rev-parse "$tag^{commit}")"
[[ "$tag_sha" == "$local_sha" ]] \
    || bail "tag '$tag' points to $tag_sha but main is at $local_sha — re-tag or sync."

echo "publish: gitleaks scan of working tree..."
gitleaks dir --config .gitleaks.toml --redact --no-banner . \
    || bail "gitleaks found findings — aborting."

tree_sha="$(git rev-parse "main^{tree}")"

echo "publish: fetching github/main (if it exists)..."
parent_args=()
if git ls-remote --exit-code github refs/heads/main >/dev/null 2>&1; then
    git fetch github main --quiet
    github_parent="$(git rev-parse github/main)"
    parent_args=(-p "$github_parent")
    echo "publish: parent = $github_parent (github/main)"
else
    echo "publish: github/main does not exist — first release will be orphan."
fi

if (( dry_run )); then
    echo "publish: dry run — would create commit with tree=$tree_sha and tag=$tag, then push to github."
    exit 0
fi

commit_msg="Release $tag

Snapshot of internal main at $tag.
See https://gitlab.hardwario.com/fiber-v2/application for development history.
"

new_commit="$(echo "$commit_msg" | git commit-tree "$tree_sha" "${parent_args[@]}")"
echo "publish: created commit $new_commit"

echo "publish: pushing $new_commit -> github/main..."
git push github "$new_commit:refs/heads/main"

echo "publish: creating and pushing tag $tag -> $new_commit..."
# Create an annotated tag in the github namespace locally to push it.
# Use a temp ref so we don't clash with the existing local tag pointing at GitLab's commit.
git update-ref "refs/github-tags/$tag" "$new_commit"
git push github "refs/github-tags/$tag:refs/tags/$tag"
git update-ref -d "refs/github-tags/$tag"

echo "publish: done. https://github.com/hardwario/fiber-agent/releases/tag/$tag"
