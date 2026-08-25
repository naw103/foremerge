#!/usr/bin/env bash
set -euo pipefail

# Runnable local JSON API walkthrough. Start `foremerge daemon` in the target
# repository first. This script never prints the bearer token.

command -v curl >/dev/null || { printf 'curl is required\n' >&2; exit 1; }
command -v git >/dev/null || { printf 'git is required\n' >&2; exit 1; }
command -v jq >/dev/null || { printf 'jq is required\n' >&2; exit 1; }

FOREMERGE_URL=${FOREMERGE_URL:-http://127.0.0.1:47811}

if [[ -n ${FOREMERGE_TOKEN:-} ]]; then
  token=$FOREMERGE_TOKEN
else
  common_dir=$(git rev-parse --path-format=absolute --git-common-dir)
  token_file=${FOREMERGE_TOKEN_FILE:-"$common_dir/foremerge/token"}
  if [[ ! -r $token_file ]]; then
    printf 'Foremerge token is not readable at %s\n' "$token_file" >&2
    printf 'Run `foremerge init`, or set FOREMERGE_TOKEN_FILE.\n' >&2
    exit 1
  fi
  token=$(tr -d '\r\n' < "$token_file")
fi

auth_header="Authorization: Bearer $token"

post_json() {
  local path=$1
  local body=$2
  curl --fail --silent --show-error \
    --header "$auth_header" \
    --header 'content-type: application/json' \
    --request POST \
    --data "$body" \
    "$FOREMERGE_URL$path"
}

printf 'Health (public endpoint)\n'
curl --fail --silent --show-error "$FOREMERGE_URL/healthz" | jq .

stripe_agent_response=$(post_json /v1/agents/register '{"name":"api-stripe-agent","capabilities":["payments"]}')
stripe_agent_id=$(printf '%s\n' "$stripe_agent_response" | jq -er '.data.id')

paypal_agent_response=$(post_json /v1/agents/register '{"name":"api-paypal-agent","capabilities":["payments"]}')
paypal_agent_id=$(printf '%s\n' "$paypal_agent_response" | jq -er '.data.id')

stripe_intent_body=$(jq -n --arg agent "$stripe_agent_id" '{
  agent_id: $agent,
  task: "modernize-payments",
  summary: "Replace PaymentService with StripePaymentService",
  rationale: "Move the existing implementation behind Stripe",
  scopes: [{kind: "symbol", key: "PaymentService", operation: "replace"}],
  depends_on: [],
  metadata: {}
}')
post_json /v1/intents "$stripe_intent_body" >/dev/null

paypal_intent_body=$(jq -n --arg agent "$paypal_agent_id" '{
  agent_id: $agent,
  task: "add-paypal",
  summary: "Add PayPal support to PaymentService",
  rationale: "Support another payment provider",
  scopes: [{kind: "symbol", key: "PaymentService", operation: "extend"}],
  depends_on: [],
  metadata: {}
}')
paypal_intent_response=$(post_json /v1/intents "$paypal_intent_body")
paypal_intent_id=$(printf '%s\n' "$paypal_intent_response" | jq -er '.data.intent.id')

printf '\nConflict returned while publishing Agent B intent\n'
printf '%s\n' "$paypal_intent_response" |
  jq '.data | {conflicts: [.conflicts[] | {kind, severity, scope, explanation, suggestion, evidence}], related_work, assessment_required}'

conflict_check_body=$(jq -n --arg intent "$paypal_intent_id" '{intent_id: $intent}')
printf '\nExplicit conflict check\n'
post_json /v1/conflicts/check "$conflict_check_body" |
  jq '.data | {blocking, policy, conflicts}'

printf '\nShared work on symbol:PaymentService\n'
curl --get --fail --silent --show-error \
  --header "$auth_header" \
  "$FOREMERGE_URL/v1/work" \
  --data-urlencode 'scope=symbol:PaymentService' |
  jq '.data[] | {agent: .agent.name, intent: .intent.summary, status: .intent.status, open_conflicts}'
