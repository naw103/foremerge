#!/usr/bin/env bash
set -euo pipefail

script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
project_dir="$(CDPATH= cd -- "$script_dir/.." && pwd)"
foremerge_bin="${FOREMERGE_BIN:-$project_dir/target/debug/foremerge}"

if ! command -v git >/dev/null 2>&1; then
  echo "demo requires git" >&2
  exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "demo requires python3 only to extract fields from CLI JSON" >&2
  exit 1
fi
if [[ ! -x "$foremerge_bin" ]]; then
  cargo build --quiet --manifest-path "$project_dir/Cargo.toml"
fi

demo_root="$(mktemp -d "${TMPDIR:-/tmp}/foremerge-demo.XXXXXX")"
cleanup() {
  if [[ "${KEEP_DEMO:-0}" == "1" ]]; then
    echo "Demo repository kept at: $demo_root" >&2
  elif [[ "$demo_root" == *foremerge-demo.* ]]; then
    rm -rf -- "$demo_root"
  fi
}
trap cleanup EXIT

repo="$demo_root/repo"
stripe_tree="$demo_root/stripe-agent"
paypal_tree="$demo_root/paypal-agent"

git init -q -b main "$repo"
git -C "$repo" config user.name "Foremerge Demo"
git -C "$repo" config user.email "demo@foremerge.local"
printf '%s\n' '# Payment demo' > "$repo/README.md"
git -C "$repo" add README.md
git -C "$repo" commit --no-gpg-sign -qm "seed demo"
git -C "$repo" worktree add -q -b agent/stripe "$stripe_tree" main
git -C "$repo" worktree add -q -b agent/paypal "$paypal_tree" main

json_get() {
  local expression="$1"
  python3 -c "import json,sys; print($expression)"
}

heading() {
  printf '\n\033[1;36m%s\033[0m\n' "$1"
}

heading "1. Two agents, two isolated Git worktrees, one shared coordination store"
"$foremerge_bin" --cwd "$stripe_tree" --json init \
  | python3 -m json.tool

stripe_agent="$({
  "$foremerge_bin" --cwd "$stripe_tree" --json agent register \
    --name stripe-agent --model codex --capability rust
} | json_get "json.load(sys.stdin)['data']['id']")"

paypal_agent="$({
  "$foremerge_bin" --cwd "$paypal_tree" --json agent register \
    --name paypal-agent --model codex --capability rust
} | json_get "json.load(sys.stdin)['data']['id']")"

printf 'stripe-agent: %s\npaypal-agent: %s\n' "$stripe_agent" "$paypal_agent"

heading "2. Agent A publishes a replacement intent before editing"
stripe_result="$({
  "$foremerge_bin" --cwd "$stripe_tree" --json intent publish \
    --agent "$stripe_agent" \
    --task replace-payments \
    --summary 'Replace PaymentService with StripePaymentService' \
    --scope symbol:PaymentService
})"
printf '%s\n' "$stripe_result" | python3 -m json.tool
stripe_intent="$(printf '%s\n' "$stripe_result" | json_get "json.load(sys.stdin)['data']['intent']['id']")"

heading "3. Agent B publishes an extension intent; Foremerge catches the collision"
paypal_result="$({
  "$foremerge_bin" --cwd "$paypal_tree" --json intent publish \
    --agent "$paypal_agent" \
    --task add-paypal \
    --summary 'Add PayPal support to PaymentService' \
    --scope symbol:PaymentService
})"
printf '%s\n' "$paypal_result" | python3 -m json.tool
conflict_id="$(printf '%s\n' "$paypal_result" | json_get "json.load(sys.stdin)['data']['conflicts'][0]['id']")"

heading "4. The conflict exists before either worktree has a code diff"
stripe_dirty="$(git -C "$stripe_tree" status --porcelain)"
paypal_dirty="$(git -C "$paypal_tree" status --porcelain)"
event_check="$({
  "$foremerge_bin" --cwd "$stripe_tree" --json events list
})"
changeset_count="$(printf '%s\n' "$event_check" | json_get "sum(1 for e in json.load(sys.stdin)['data'] if e['event_type'] == 'changeset.published')")"
printf 'stripe worktree dirty: %s\npaypal worktree dirty: %s\nChangeSets published: %s\n' \
  "${stripe_dirty:+yes}${stripe_dirty:-no}" \
  "${paypal_dirty:+yes}${paypal_dirty:-no}" \
  "$changeset_count"

heading "5. Agents coordinate around the suggested PaymentProvider abstraction"
"$foremerge_bin" --cwd "$paypal_tree" --json coordinate send \
  --from "$paypal_agent" \
  --to "$stripe_agent" \
  --conflict "$conflict_id" \
  --message 'Propose PaymentProvider first; Stripe and PayPal become implementations.' \
  | python3 -m json.tool

"$foremerge_bin" --cwd "$stripe_tree" --json coordinate inbox "$stripe_agent" \
  | python3 -m json.tool

heading "6. Shared work query answers who owns the semantic area"
"$foremerge_bin" --cwd "$stripe_tree" --json work query \
  --scope symbol:PaymentService \
  | python3 -m json.tool

printf '\nConflict %s was detected while intent %s still had no code changes.\n' \
  "$conflict_id" "$stripe_intent"
