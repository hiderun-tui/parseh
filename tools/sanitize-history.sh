#!/usr/bin/env bash
# sanitize-history.sh — destructive git history rewrite before public flip.
#
# What it does
#   Wraps `git filter-repo` to:
#     1. Replace a list of literal strings throughout every blob in
#        every branch and every tag of the repository. The list of
#        strings is supplied by the maintainer via an external file
#        (NOT committed; see "Configuration" below).
#     2. Rewrite every author and committer timestamp from the local
#        timezone to +0000 (UTC), so the geographic cluster currently
#        visible in commit metadata is removed.
#
# Why the replacement list is external
#   A script committed to a public repository must not itself contain
#   the very strings it is rewriting away — that defeats the purpose.
#   The maintainer keeps the literal values in a private file at
#   ${REPLACEMENTS_FILE} below; that path is in `.gitignore`. The
#   script refuses to run if the file is missing.
#
# ⚠ DESTRUCTIVE
#   This rewrites every commit SHA. Every open PR is invalidated. Anyone
#   with a clone has stale history. Run BEFORE the repo flips to public.
#
# Prereqs
#   - `git-filter-repo` installed (`pip install --user git-filter-repo`
#     or `brew install git-filter-repo`)
#   - Repository remote URL matches expected (see REMOTE_URL_GUARD)
#   - You have a SEPARATE backup; this script makes one but a copy in
#     another location is recommended
#
# Configuration
#   By default the script reads replacements from:
#       $HOME/.config/parseh/history-replacements.txt
#   Override with the env var REPLACEMENTS_FILE.
#
#   File format — one rule per line:
#       literal-string==>replacement
#       another-literal==>another-replacement
#       # comments and blank lines are allowed
#
#   Recommended fill-in: include each absolute filesystem path, prior-
#   project name, account handle, and IP literal that exists anywhere
#   in your history and that you want gone from the public artefact.
#   See `tools/sanitize-replacements.example.txt` for the schema.
#
# Usage
#   1. Create $HOME/.config/parseh/history-replacements.txt with your
#      actual replacement rules.
#   2. Dry-run first:
#        ./tools/sanitize-history.sh --dry-run
#      Produces /tmp/parseh-history-preview/ for inspection.
#   3. When the preview looks right, execute:
#        ./tools/sanitize-history.sh --execute
#      Two confirmation prompts before any destructive action.

set -euo pipefail

# ─── Configuration ──────────────────────────────────────────────────────────

REMOTE_NAME="${REMOTE_NAME:-origin}"
REMOTE_URL_GUARD="${REMOTE_URL_GUARD:-hiderun-tui/parseh}"
BACKUP_DIR="${BACKUP_DIR:-/tmp/parseh-pre-sanitize-backup-$(date +%Y%m%d-%H%M%S)}"
PREVIEW_DIR="${PREVIEW_DIR:-/tmp/parseh-history-preview}"
REPLACEMENTS_FILE="${REPLACEMENTS_FILE:-$HOME/.config/parseh/history-replacements.txt}"

# ─── Argument parsing ──────────────────────────────────────────────────────

MODE=""
case "${1:-}" in
  --dry-run|--preview)  MODE="dry-run" ;;
  --execute|--for-real) MODE="execute" ;;
  *)
    cat <<USAGE
Usage:  $0 --dry-run    # preview only
        $0 --execute    # rewrite history + force-push

Reads literal replacement rules from:
  $REPLACEMENTS_FILE

The replacements file MUST exist before running and is NOT committed.
USAGE
    exit 2
    ;;
esac

# ─── Sanity checks ─────────────────────────────────────────────────────────

if ! command -v git-filter-repo >/dev/null 2>&1; then
  echo "Error: git-filter-repo is not installed."
  echo "Install:  pip install --user git-filter-repo   (or brew install git-filter-repo)"
  exit 1
fi

if ! git rev-parse --git-dir >/dev/null 2>&1; then
  echo "Error: not a git repository. Run this from the root of the parseh clone."
  exit 1
fi

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "Error: working tree has uncommitted changes. Commit or stash first."
  git status --short
  exit 1
fi

REMOTE_URL="$(git remote get-url "$REMOTE_NAME" 2>/dev/null || true)"
if [[ -z "$REMOTE_URL" ]]; then
  echo "Error: remote '$REMOTE_NAME' has no URL configured."
  exit 1
fi
if [[ "$REMOTE_URL" != *"$REMOTE_URL_GUARD"* ]]; then
  echo "Error: remote URL does not match the expected repository pattern."
  echo "Got:      $REMOTE_URL"
  echo "Expected: pattern containing '$REMOTE_URL_GUARD'"
  echo "Override with: REMOTE_URL_GUARD=<pattern> $0 $1"
  exit 1
fi

if [[ ! -f "$REPLACEMENTS_FILE" ]]; then
  cat >&2 <<MISSING

Error: replacements file not found.
       Expected at: $REPLACEMENTS_FILE

Create it before running this script.

Format — one rule per line, lines starting with '#' or blank lines ignored:

  literal-string==>replacement
  /absolute/path/to/scrub==><projects-root>
  PriorProjectName==>prior-art

See tools/sanitize-replacements.example.txt in this repo for a complete
schema with placeholder values you can adapt.

To set a different path, run:
  REPLACEMENTS_FILE=/path/to/your/file $0 $1

MISSING
  exit 1
fi

# Validate file format: every non-comment non-blank line contains '==>'
INVALID_LINES=$(grep -vE '^\s*(#.*)?$' "$REPLACEMENTS_FILE" | grep -v '==>' || true)
if [[ -n "$INVALID_LINES" ]]; then
  echo "Error: replacements file has malformed rules (missing '==>'):" >&2
  echo "$INVALID_LINES" >&2
  exit 1
fi

RULE_COUNT=$(grep -cvE '^\s*(#.*)?$' "$REPLACEMENTS_FILE" || true)
echo "Loaded $RULE_COUNT replacement rules from $REPLACEMENTS_FILE"

# ─── Dry-run path ──────────────────────────────────────────────────────────

if [[ "$MODE" == "dry-run" ]]; then
  echo "=== DRY RUN ==="
  rm -rf "$PREVIEW_DIR"
  git clone --mirror . "$PREVIEW_DIR" >/dev/null 2>&1
  pushd "$PREVIEW_DIR" >/dev/null
  git filter-repo \
    --replace-text "$REPLACEMENTS_FILE" \
    --commit-callback '
ts_a = commit.author_date.decode().split()[0]
ts_c = commit.committer_date.decode().split()[0]
commit.author_date    = f"{ts_a} +0000".encode()
commit.committer_date = f"{ts_c} +0000".encode()
' \
    --force
  popd >/dev/null
  cat <<DRYRUN_END

Dry run complete. Inspect rewritten history:
  cd $PREVIEW_DIR
  git log --all -p | grep -i <YOUR-LITERAL>     # should return zero lines for every rule LHS
  git log --all --format='%ci' | awk '{print \$3}' | sort -u  # must show only +0000

If the preview looks right, run again with --execute.
DRYRUN_END
  exit 0
fi

# ─── Execute path ──────────────────────────────────────────────────────────

if [[ "$MODE" == "execute" ]]; then
  echo "=== EXECUTE — DESTRUCTIVE REWRITE ==="
  echo
  echo "Will:"
  echo "  1. Mirror-backup at $BACKUP_DIR"
  echo "  2. Rewrite entire git history in this clone"
  echo "  3. Force-push to $REMOTE_URL"
  echo "  4. Invalidate every open PR on the remote"
  echo
  read -p "Type the exact phrase 'rewrite history' to proceed: " CONFIRM
  if [[ "$CONFIRM" != "rewrite history" ]]; then
    echo "Aborted."
    exit 1
  fi

  echo
  echo "→ Mirror backup → $BACKUP_DIR"
  git clone --mirror . "$BACKUP_DIR" >/dev/null 2>&1

  echo
  echo "→ Rewriting history"
  git filter-repo \
    --replace-text "$REPLACEMENTS_FILE" \
    --commit-callback '
ts_a = commit.author_date.decode().split()[0]
ts_c = commit.committer_date.decode().split()[0]
commit.author_date    = f"{ts_a} +0000".encode()
commit.committer_date = f"{ts_c} +0000".encode()
'

  echo
  echo "→ Re-adding remote (filter-repo strips it)"
  git remote add "$REMOTE_NAME" "$REMOTE_URL"

  echo
  read -p "Last chance — type 'yes force push' to overwrite the remote: " CONFIRM2
  if [[ "$CONFIRM2" != "yes force push" ]]; then
    echo "Aborted before push. Local history is rewritten; remote is untouched."
    echo "Restore: rm -rf .git && git clone $BACKUP_DIR .git --mirror"
    exit 1
  fi

  git push --force --all "$REMOTE_NAME"
  git push --force --tags "$REMOTE_NAME"

  cat <<DONE

=== DONE ===
Backup: $BACKUP_DIR
Remote is now on rewritten history.

Manual follow-up on GitHub:
  1. Close every open PR (commits no longer exist on main)
  2. Delete every branch except main
  3. Wait ~30 min for Dependabot to reopen against rewritten HEAD
  4. Final scan:
       git clone https://github.com/hiderun-tui/parseh /tmp/parseh-public-final
       gitleaks detect --source /tmp/parseh-public-final --log-opts='--all' \\
         --config /tmp/parseh-public-final/.gitleaks.toml

Only AFTER all the above passes, flip the repo visibility to public.
DONE
  exit 0
fi
