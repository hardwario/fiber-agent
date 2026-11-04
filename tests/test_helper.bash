# Shared by every bats file: the repository root, nothing else.
REPO_ROOT="$(cd "$(dirname "${BATS_TEST_FILENAME}")/.." && pwd)"
export REPO_ROOT
