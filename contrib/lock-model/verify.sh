#!/usr/bin/env bash
#
# contrib/lock-model/verify.sh — verify the successor-locks DynamoDB model.
#
# The successor-locks design keeps a file's lock and its chain state in one
# item, so acquisition is a single conditional update that takes the lock and
# returns the state the causality check compares against. That statement is
# about what the DynamoDB API does, so this checks it against DynamoDB rather
# than reasoning about it, and reports the write capacity each operation
# consumes.
#
# It builds two tables: the model the design proposes, and today's deployed
# `locks` table from contrib/aws/storage.tf, so the costs can be compared
# directly.
#
# Options:
#   --target local|aws       local, the default, runs against DynamoDB Local;
#                            aws runs against real DynamoDB.
#   --endpoint URL           the DynamoDB endpoint: http://localhost:8000 for
#                            local, https://dynamodb.<region>.amazonaws.com
#                            for aws.
#   --region REGION          the AWS region, required for aws.
#   --profile NAME           an AWS CLI profile to take credentials from,
#                            through `aws configure export-credentials`.
#                            Without it an aws run reads AWS_ACCESS_KEY_ID,
#                            AWS_SECRET_ACCESS_KEY and, for temporary
#                            credentials, AWS_SESSION_TOKEN.
#   --table-prefix PREFIX    prepended to every table name: empty for local,
#                            lore-lockmodel-$USER-<epoch>- for aws.
#   --tag KEY=VALUE          a tag on every table the run creates; repeatable.
#   --untag-before-delete KEY
#                            a tag removed from a table before deleting it, for
#                            an account whose policy refuses to delete a table
#                            that carries the tag; repeatable.
#   --index-wait-seconds N   how long a read of an index waits for a write to
#                            show (10).
#   --table-wait-seconds N   how long creating a table waits for it to become
#                            ACTIVE (600).
#   --delete-wait-seconds N  how long deleting the run's tables may take (600).
#
# Targets:
#   local runs with a placeholder signature, and a table of the same name left
#   by an earlier run is replaced.
#
#   aws signs every request with SigV4. curl reads the credentials from a pipe,
#   so they never appear in a process list.
#
# Cleanup: an aws run refuses to start when a table it would create exists
# already, and touches no table it did not create. Every run deletes the
# tables it created when it exits: on success, on failure, and on Ctrl-C,
# SIGTERM or SIGHUP. Before deleting a table it removes the
# --untag-before-delete tags and turns deletion protection off, since a
# process other than the run can turn it on. A deletion refused for access is
# retried while --untag-before-delete is given, since a policy keyed on a tag
# can take minutes to see the tag gone, and one that hasn't finished within
# --delete-wait-seconds is reported, as is every table left when a signal
# stops the waiting, for deletion by hand.
#
# Permissions: an aws run calls dynamodb:ListTables, CreateTable,
# DescribeTable, DeleteTable, PutItem, UpdateItem, DeleteItem, Query, Scan and
# BatchWriteItem; TagResource with --tag; UntagResource with
# --untag-before-delete; and UpdateTable when a table it created has deletion
# protection on. TransactWriteItems is authorized as PutItem.
#
# Real DynamoDB updates indexes asynchronously, so every check that reads an
# index after a write polls every 200 ms until the index shows the write, for
# up to --index-wait-seconds, and the run reports how long each took.
#
# Usage:
#   docker run -d --name lore-ddb -p 8000:8000 amazon/dynamodb-local:latest \
#     -jar DynamoDBLocal.jar -inMemory -sharedDb
#   bash contrib/lock-model/verify.sh
#   docker rm -f lore-ddb
#
#   bash contrib/lock-model/verify.sh --target aws --region <region> \
#     --profile <profile> [--tag <key>=<value>]... [--untag-before-delete <key>]...
#
# Cost: an aws run makes under a hundred requests against two on-demand
# tables, a fraction of a cent.
#
# Exit codes:
#   0  every check passed
#   1  a check failed; the failing expectation is printed
#   2  DynamoDB is not reachable at the endpoint, or a table did not become
#      ACTIVE in time
#   3  an option is unknown or invalid, or the region or credentials are
#      missing
#   4  a table the run would create exists already

set -uo pipefail

usage() {
    cat <<'EOF'
usage: verify.sh [--target local|aws] [--endpoint URL] [--region REGION]
                 [--profile NAME] [--table-prefix PREFIX] [--tag KEY=VALUE]...
                 [--untag-before-delete KEY]... [--index-wait-seconds N]
                 [--table-wait-seconds N] [--delete-wait-seconds N]
EOF
}

# fail_usage MESSAGE — reports an invalid invocation and exits.
fail_usage() {
    echo "verify.sh: $1" >&2
    usage >&2
    exit 3
}

target=local
endpoint=""
region=""
profile=""
table_prefix=""
table_prefix_given=0
tags=()
untag_keys=()
index_wait_seconds=10
table_wait_seconds=600
delete_wait_seconds=600

while (( $# )); do
    option="$1"
    shift
    case "${option}" in
    -h | --help)
        usage
        exit 0
        ;;
    --*=*)
        set -- "${option#*=}" "$@"
        option="${option%%=*}"
        ;;
    esac
    case "${option}" in
    --target | --endpoint | --region | --profile | --table-prefix | --tag | \
        --untag-before-delete | --index-wait-seconds | --table-wait-seconds | \
        --delete-wait-seconds)
        (( $# )) || fail_usage "${option} needs a value"
        value="$1"
        shift
        ;;
    *)
        fail_usage "unknown option ${option}"
        ;;
    esac
    case "${option}" in
    --target) target="${value}" ;;
    --endpoint) endpoint="${value}" ;;
    --region) region="${value}" ;;
    --profile) profile="${value}" ;;
    --table-prefix)
        table_prefix="${value}"
        table_prefix_given=1
        ;;
    --tag)
        [[ "${value}" == ?*=* ]] || fail_usage "--tag takes KEY=VALUE, not '${value}'"
        tags+=("${value}")
        ;;
    --untag-before-delete)
        [[ -n "${value}" ]] || fail_usage "--untag-before-delete needs a tag key"
        untag_keys+=("${value}")
        ;;
    --index-wait-seconds | --table-wait-seconds | --delete-wait-seconds)
        [[ "${value}" =~ ^[0-9]+$ ]] ||
            fail_usage "${option} takes a whole number of seconds, not '${value}'"
        name="${option#--}"
        printf -v "${name//-/_}" '%s' "${value}"
        ;;
    esac
done

case "${target}" in
local)
    [[ -z "${profile}" ]] || fail_usage "--profile needs --target aws"
    default_endpoint="http://localhost:8000"
    default_prefix=""
    ;;
aws)
    [[ -n "${region}" ]] || fail_usage "--target aws needs --region"
    default_endpoint="https://dynamodb.${region}.amazonaws.com"
    user="${USER:-$(id -un)}"
    default_prefix="lore-lockmodel-${user//[^A-Za-z0-9_.-]/-}-$(date +%s)-"
    ;;
*)
    fail_usage "--target is '${target}'; it must be local or aws"
    ;;
esac
(( table_prefix_given )) || table_prefix="${default_prefix}"
if [[ "${table_prefix}" =~ [^A-Za-z0-9_.-] ]]; then
    fail_usage "--table-prefix '${table_prefix}' holds a character a table name cannot"
fi

access_key=""
secret_key=""
session_token=""
if [[ "${target}" == aws ]]; then
    if [[ -n "${profile}" ]]; then
        if ! exported="$(aws configure export-credentials --profile "${profile}" --format process)"; then
            echo "no credentials for profile '${profile}'; log it in and run again" >&2
            exit 3
        fi
        {
            IFS= read -r access_key
            IFS= read -r secret_key
            IFS= read -r session_token
        } < <(python3 -c '
import sys, json
d = json.load(sys.stdin)
for name in ("AccessKeyId", "SecretAccessKey", "SessionToken"):
    print(d.get(name, ""))' <<<"${exported}")
        unset exported
    else
        access_key="${AWS_ACCESS_KEY_ID:-}"
        secret_key="${AWS_SECRET_ACCESS_KEY:-}"
        session_token="${AWS_SESSION_TOKEN:-}"
    fi
    if [[ -z "${access_key}" || -z "${secret_key}" ]]; then
        echo "--target aws needs credentials: --profile NAME, or AWS_ACCESS_KEY_ID and" \
            "AWS_SECRET_ACCESS_KEY in the environment" >&2
        exit 3
    fi
fi

tags_field=""
if (( ${#tags[@]} )); then
    tags_field=",\"Tags\":$(python3 -c '
import sys, json
print(json.dumps([dict(zip(("Key", "Value"), tag.split("=", 1))) for tag in sys.argv[1:]]))' "${tags[@]}")"
fi
untag_json="[]"
if (( ${#untag_keys[@]} )); then
    untag_json="$(python3 -c 'import sys, json; print(json.dumps(sys.argv[1:]))' "${untag_keys[@]}")"
fi

readonly TARGET="${target}"
readonly REGION="${region}"
readonly ENDPOINT="${endpoint:-${default_endpoint}}"
readonly SPEC_TABLE="${table_prefix}spec-locks"
readonly TODAY_TABLE="${table_prefix}today-locks"
readonly INDEX_WAIT_MS=$(( index_wait_seconds * 1000 ))
readonly TABLE_WAIT_SECONDS="${table_wait_seconds}"
readonly DELETE_WAIT_SECONDS="${delete_wait_seconds}"

failures=0
created_tables=()
batch_file=""
stop_waiting=0

# curl_credentials — the curl config lines that carry the credentials, for
# curl to read from a pipe rather than from its command line.
curl_credentials() {
    printf 'user = "%s:%s"\n' "${access_key}" "${secret_key}"
    if [[ -n "${session_token}" ]]; then
        printf 'header = "x-amz-security-token: %s"\n' "${session_token}"
    fi
}

# ddb TARGET JSON — one DynamoDB API call. In aws mode curl signs it with
# SigV4; DynamoDB Local accepts any signature, so in local mode the
# Authorization header is a placeholder. DynamoDB answers some successful
# calls, such as UntagResource, with an empty body, which prints as {}; a call
# that gets no answer at all prints nothing.
ddb() {
    local request=(-s -X POST "${ENDPOINT}/"
        -H "Content-Type: application/x-amz-json-1.0"
        -H "X-Amz-Target: DynamoDB_20120810.$1"
        -d "$2") body
    if [[ "${TARGET}" == aws ]]; then
        body="$(curl "${request[@]}" --aws-sigv4 "aws:amz:${REGION}:dynamodb" -K <(curl_credentials))" || return
    else
        body="$(curl "${request[@]}" \
            -H "Authorization: AWS4-HMAC-SHA256 Credential=local/20260101/us-east-2/dynamodb/aws4_request, SignedHeaders=host;x-amz-date;x-amz-target, Signature=placeholder")" || return
    fi
    [[ -n "${body}" ]] || body='{}'
    printf '%s' "${body}"
}

# check DESCRIPTION EXPECTED ACTUAL
check() {
    if [[ "$2" == "$3" ]]; then
        printf '  ok    %-46s %s\n' "$1" "$3"
    else
        printf '  FAIL  %-46s expected %s, got %s\n' "$1" "$2" "$3"
        failures=$((failures + 1))
    fi
}

# capacity JSON — total write units a response reports.
capacity() {
    python3 -c 'import sys,json; d=json.load(sys.stdin); c=d.get("ConsumedCapacity"); c=c[0] if isinstance(c,list) else c; print(c["CapacityUnits"] if c else "none")'
}

# Each reader takes a response on stdin and prints one value, or "absent" when
# the response doesn't carry it. Keeping them separate avoids passing Python
# expressions through nested command substitution, which quotes badly.

# count — the Count of a Scan or Query.
count() {
    python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("Count","absent"))'
}

# error — the exception name a response carries, or "none" for success.
error() {
    python3 -c 'import sys,json;print(json.load(sys.stdin).get("__type","none").split("#")[-1])'
}

# binary SECTION NAME — a binary attribute, decoded to text. SECTION is
# Attributes (a returned prior state) or Item (a failed condition's row).
binary() {
    python3 -c '
import sys, json, base64
section, name = sys.argv[1], sys.argv[2]
d = json.load(sys.stdin)
try:
    print(base64.b64decode(d[section][name]["B"]).decode())
except (KeyError, TypeError):
    print("absent")' "$1" "$2"
}

# has_attributes — whether a response returned a prior state at all.
has_attributes() {
    python3 -c 'import sys,json;print("present" if json.load(sys.stdin).get("Attributes") else "absent")'
}

# table_state — from a DescribeTable response, ACTIVE when the table and every
# index are active, GONE when the table does not exist, and otherwise the
# statuses it reports.
table_state() {
    python3 -c '
import sys, json
d = json.load(sys.stdin)
if d.get("__type", "").endswith("ResourceNotFoundException"):
    print("GONE")
elif "Table" in d:
    table = d["Table"]
    states = [table.get("TableStatus")] + [
        index.get("IndexStatus") for index in table.get("GlobalSecondaryIndexes", [])
    ]
    print("ACTIVE" if all(s == "ACTIVE" for s in states) else "/".join(map(str, states)))
else:
    print("ERROR:" + d.get("__type", "unknown"))'
}

require_dynamodb() {
    local r
    r="$(ddb ListTables '{}')"
    if ! grep -q TableNames <<<"${r}"; then
        if [[ "${TARGET}" == aws ]]; then
            echo "DynamoDB unreachable at ${ENDPOINT}: ${r:-no response}" >&2
        else
            echo "DynamoDB Local unreachable at ${ENDPOINT} — see the usage note above." >&2
        fi
        exit 2
    fi
}

# wait_for_table TABLE STATE — polls until the table is STATE, and fails after
# --table-wait-seconds.
wait_for_table() {
    local deadline state
    deadline=$(( $(date +%s) + TABLE_WAIT_SECONDS ))
    while :; do
        state="$(ddb DescribeTable "{\"TableName\":\"$1\"}" | table_state)"
        [[ "${state}" == "$2" ]] && return 0
        if (( $(date +%s) >= deadline )); then
            echo "table $1 is ${state}, not $2, after ${TABLE_WAIT_SECONDS}s" >&2
            return 1
        fi
        sleep 0.5
    done
}

# deletion_state — from a DescribeTable response, the table's status (GONE when
# it does not exist), whether deletion protection is on, and its ARN.
deletion_state() {
    python3 -c '
import sys, json
d = json.load(sys.stdin)
if d.get("__type", "").endswith("ResourceNotFoundException"):
    print("GONE false -")
elif "Table" in d:
    table = d["Table"]
    protected = "true" if table.get("DeletionProtectionEnabled") else "false"
    print(table.get("TableStatus"), protected, table.get("TableArn", "-"))
else:
    print("ERROR:" + d.get("__type", "unknown"), "false", "-")'
}

# step_ok STEP RESPONSE — returns 0 when the response to one step of a deletion
# succeeded. Otherwise prints what delete_attempt reports for it and returns 1.
step_ok() {
    case "$(printf '%s' "$2" | error)" in
    none | ResourceNotFoundException) return 0 ;;
    ResourceInUseException) echo waiting ;;
    AccessDeniedException)
        if (( ${#untag_keys[@]} )); then
            echo "retry:$1 refused: $2"
        else
            echo "failed:$1 refused: $2"
        fi
        ;;
    ValidationException)
        if [[ "$2" == *protect* ]]; then
            echo "retry:$1 refused: $2"
        else
            echo "failed:$1 failed: $2"
        fi
        ;;
    *) echo "failed:$1 failed: ${2:-no response}" ;;
    esac
    return 1
}

# delete_attempt TABLE — one round towards deleting a table this run created.
# Prints gone; waiting while the deletion runs or the table is busy;
# retry:REASON for a refusal that can clear; or failed:REASON.
delete_attempt() {
    local r state protected arn
    r="$(ddb DescribeTable "{\"TableName\":\"$1\"}")"
    read -r state protected arn < <(printf '%s' "${r}" | deletion_state)
    case "${state}" in
    GONE)
        echo gone
        return
        ;;
    DELETING | CREATING | UPDATING)
        echo waiting
        return
        ;;
    ERROR:*)
        echo "failed:describing failed: ${r:-no response}"
        return
        ;;
    esac
    if (( ${#untag_keys[@]} )); then
        r="$(ddb UntagResource "{\"ResourceArn\":\"${arn}\",\"TagKeys\":${untag_json}}")"
        step_ok untagging "${r}" || return 0
    fi
    if [[ "${protected}" == true ]]; then
        r="$(ddb UpdateTable "{\"TableName\":\"$1\",\"DeletionProtectionEnabled\":false}")"
        step_ok "turning deletion protection off" "${r}" || return 0
    fi
    r="$(ddb DeleteTable "{\"TableName\":\"$1\"}")"
    step_ok deleting "${r}" || return 0
    echo waiting
}

# delete_tables TABLE... — deletes tables this run created and waits until each
# is gone, for up to --delete-wait-seconds or until a signal stops the waiting.
# Reports every table it leaves behind with the reason, and returns 1 when it
# leaves any.
delete_tables() {
    local pending=("$@") remaining table outcome delay deadline left=0
    local -A reason=()
    deadline=$(( $(date +%s) + DELETE_WAIT_SECONDS ))
    while (( ${#pending[@]} )); do
        remaining=()
        delay=0.5
        for table in "${pending[@]}"; do
            outcome="$(delete_attempt "${table}")"
            case "${outcome}" in
            gone)
                [[ "${TARGET}" == aws ]] && echo "deleted table ${table}"
                ;;
            retry:*)
                remaining+=("${table}")
                reason["${table}"]="${outcome#retry:}"
                delay=15
                ;;
            failed:*)
                echo "table ${table} is left behind; delete it by hand. ${outcome#failed:}" >&2
                left=1
                ;;
            *)
                remaining+=("${table}")
                reason["${table}"]="${reason["${table}"]:-not gone yet}"
                ;;
            esac
        done
        pending=("${remaining[@]}")
        (( ${#pending[@]} )) || break
        if (( stop_waiting || $(date +%s) >= deadline )); then
            for table in "${pending[@]}"; do
                echo "table ${table} is left behind; delete it by hand. ${reason["${table}"]}" >&2
            done
            return 1
        fi
        sleep "${delay}"
    done
    return "${left}"
}

# create_table TABLE JSON — creates the table and waits until it and its
# indexes are ACTIVE.
create_table() {
    local r
    if [[ "${TARGET}" == local ]]; then
        delete_tables "$1" || exit 2
    fi
    # Registered before the request: in aws mode the start-up check showed the
    # name unused, so a table of this name can only be this run's, and an
    # interrupt that lands before the response still has it deleted.
    created_tables+=("$1")
    r="$(ddb CreateTable "$2")"
    if [[ "$(printf '%s' "${r}" | error)" != none ]]; then
        echo "creating table $1 failed: ${r:-no response}" >&2
        exit 2
    fi
    wait_for_table "$1" ACTIVE || exit 2
}

refuse_existing_tables() {
    local table
    for table in "${SPEC_TABLE}" "${TODAY_TABLE}"; do
        if [[ "$(ddb DescribeTable "{\"TableName\":\"${table}\"}" | table_state)" != GONE ]]; then
            echo "table ${table} exists already, and this run only uses tables it creates." >&2
            echo "Pass --table-prefix with a prefix no table uses." >&2
            exit 4
        fi
    done
}

cleanup() {
    local status=$?
    trap 'stop_waiting=1' INT TERM HUP
    (( ${#created_tables[@]} )) && delete_tables "${created_tables[@]}"
    [[ -n "${batch_file}" ]] && rm -f "${batch_file}"
    exit "${status}"
}

create_spec_table() {
    create_table "${SPEC_TABLE}" "{
      \"TableName\":\"${SPEC_TABLE}\", \"BillingMode\":\"PAY_PER_REQUEST\"${tags_field},
      \"AttributeDefinitions\":[
        {\"AttributeName\":\"k\",\"AttributeType\":\"B\"},
        {\"AttributeName\":\"held_pk\",\"AttributeType\":\"S\"},
        {\"AttributeName\":\"held_sk\",\"AttributeType\":\"S\"},
        {\"AttributeName\":\"scope_pk\",\"AttributeType\":\"S\"},
        {\"AttributeName\":\"scope_sk\",\"AttributeType\":\"S\"}],
      \"KeySchema\":[{\"AttributeName\":\"k\",\"KeyType\":\"HASH\"}],
      \"GlobalSecondaryIndexes\":[
        {\"IndexName\":\"held\",
         \"KeySchema\":[{\"AttributeName\":\"held_pk\",\"KeyType\":\"HASH\"},
                        {\"AttributeName\":\"held_sk\",\"KeyType\":\"RANGE\"}],
         \"Projection\":{\"ProjectionType\":\"KEYS_ONLY\"}},
        {\"IndexName\":\"scope\",
         \"KeySchema\":[{\"AttributeName\":\"scope_pk\",\"KeyType\":\"HASH\"},
                        {\"AttributeName\":\"scope_sk\",\"KeyType\":\"RANGE\"}],
         \"Projection\":{\"ProjectionType\":\"KEYS_ONLY\"}}]}"
}

# Today's table, matching contrib/aws/storage.tf: three ALL-projected indexes
# over a path-hash primary key.
create_today_table() {
    create_table "${TODAY_TABLE}" "{
      \"TableName\":\"${TODAY_TABLE}\", \"BillingMode\":\"PAY_PER_REQUEST\"${tags_field},
      \"AttributeDefinitions\":[
        {\"AttributeName\":\"hash\",\"AttributeType\":\"B\"},
        {\"AttributeName\":\"repositoryBranch\",\"AttributeType\":\"B\"},
        {\"AttributeName\":\"ownerId\",\"AttributeType\":\"S\"},
        {\"AttributeName\":\"repository\",\"AttributeType\":\"B\"},
        {\"AttributeName\":\"branch\",\"AttributeType\":\"B\"},
        {\"AttributeName\":\"description\",\"AttributeType\":\"S\"}],
      \"KeySchema\":[{\"AttributeName\":\"hash\",\"KeyType\":\"HASH\"},
                     {\"AttributeName\":\"repositoryBranch\",\"KeyType\":\"RANGE\"}],
      \"GlobalSecondaryIndexes\":[
        {\"IndexName\":\"owner-repo-branch\",
         \"KeySchema\":[{\"AttributeName\":\"ownerId\",\"KeyType\":\"HASH\"},
                        {\"AttributeName\":\"repositoryBranch\",\"KeyType\":\"RANGE\"}],
         \"Projection\":{\"ProjectionType\":\"ALL\"}},
        {\"IndexName\":\"repo-branch\",
         \"KeySchema\":[{\"AttributeName\":\"repository\",\"KeyType\":\"HASH\"},
                        {\"AttributeName\":\"branch\",\"KeyType\":\"RANGE\"}],
         \"Projection\":{\"ProjectionType\":\"ALL\"}},
        {\"IndexName\":\"repo-branch-description\",
         \"KeySchema\":[{\"AttributeName\":\"repositoryBranch\",\"KeyType\":\"HASH\"},
                        {\"AttributeName\":\"description\",\"KeyType\":\"RANGE\"}],
         \"Projection\":{\"ProjectionType\":\"ALL\"}}]}"
}

# A latest entry with no lock: what a concluded session leaves behind.
seed_latest_entry() {
    ddb PutItem "{\"TableName\":\"${SPEC_TABLE}\",\"Item\":{
      \"k\":{\"B\":\"AAAAAAAAAAE=\"},
      \"revision\":{\"B\":\"cmV2MDE=\"},
      \"content\":{\"B\":\"Y29udDAx\"},
      \"scope_pk\":{\"S\":\"scope-main#07\"},
      \"scope_sk\":{\"S\":\"file-0001\"}}}" >/dev/null
}

acquire() {
    ddb UpdateItem "{\"TableName\":\"${SPEC_TABLE}\",\"Key\":{\"k\":{\"B\":\"$1\"}},
      \"ConditionExpression\":\"attribute_not_exists(holder) OR expires_at < :now\",
      \"UpdateExpression\":\"SET holder=:h, owner_id=:o, confirmed=:c, expires_at=:e, held_pk=:hp, held_sk=:hs\",
      \"ExpressionAttributeValues\":{
        \":now\":{\"N\":\"$2\"}, \":h\":{\"B\":\"$3\"}, \":o\":{\"S\":\"tester\"},
        \":c\":{\"BOOL\":false}, \":e\":{\"N\":\"$4\"},
        \":hp\":{\"S\":\"repo-x#07\"}, \":hs\":{\"S\":\"$5\"}},
      \"ReturnValues\":\"ALL_OLD\",
      \"ReturnValuesOnConditionCheckFailure\":\"ALL_OLD\",
      \"ReturnConsumedCapacity\":\"INDEXES\"}"
}


# Every call below captures its response to a variable before reading it.
# Nesting a ddb call inside check's own command substitution quotes badly, and
# the failure looks like a model problem rather than a shell one.

index_count() {
    ddb Scan "{\"TableName\":\"${SPEC_TABLE}\",\"IndexName\":\"$1\",\"Select\":\"COUNT\"}"
}

query_held() {
    ddb Query "{\"TableName\":\"${SPEC_TABLE}\",\"IndexName\":\"held\",
      \"KeyConditionExpression\":\"$1\",\"ExpressionAttributeValues\":$2}"
}

delete_guarded() {
    ddb DeleteItem "{\"TableName\":\"${SPEC_TABLE}\",\"Key\":{\"k\":{\"B\":\"$1\"}},
      \"ConditionExpression\":\"holder = :h AND attribute_not_exists(revision)\",
      \"ExpressionAttributeValues\":{\":h\":{\"B\":\"$2\"}}}"
}

index_reads=0
index_first_reads=0
index_longest_ms=0
index_slow=()

now_ms() {
    date +%s%3N
}

# index_check DESCRIPTION EXPECTED COMMAND... — checks that the Count COMMAND
# reads from an index is EXPECTED. An index shows a write only once DynamoDB
# has propagated it, so COMMAND runs every 200 ms until it reads EXPECTED, for
# up to --index-wait-seconds, and the lag before the read that matched is
# recorded.
index_check() {
    local description="$1" expected="$2" first="" started value reads=0 lag
    shift 2
    while :; do
        started="$(now_ms)"
        first="${first:-${started}}"
        value="$("$@" | count)"
        reads=$((reads + 1))
        if [[ "${value}" == "${expected}" ]] || (( started - first >= INDEX_WAIT_MS )); then
            break
        fi
        sleep 0.2
    done
    lag=$((started - first))
    index_reads=$((index_reads + 1))
    if [[ "${value}" == "${expected}" ]] && (( reads == 1 )); then
        index_first_reads=$((index_first_reads + 1))
    else
        index_slow+=("$(printf '%-46s %s' "${description}" "${value} after ${lag} ms, ${reads} reads")")
    fi
    (( lag > index_longest_ms )) && index_longest_ms=${lag}
    check "${description}" "${expected}" "${value}"
}

echo "successor-locks DynamoDB model verification against ${ENDPOINT}"
[[ "${TARGET}" == aws ]] && echo "aws target in ${REGION}, tables ${SPEC_TABLE} and ${TODAY_TABLE}"
echo
require_dynamodb
[[ "${TARGET}" == aws ]] && refuse_existing_tables
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
create_spec_table
create_today_table
seed_latest_entry

echo "sparse indexes — a latest entry carries no lock, so it indexes only as chain"
index_check "chain-only row in held index" "0" index_count held
index_check "chain-only row in scope index" "1" index_count scope

echo
echo "acquire — one conditional update takes the lock and returns the chain"
r="$(acquire "AAAAAAAAAAE=" 1000 "YnJhbmNoLUE=" 9999 "branch-A#file-0001")"
check "ALL_OLD carries the chain revision" "rev01"  "$(printf '%s' "$r" | binary Attributes revision)"
check "ALL_OLD carries the chain content"  "cont01" "$(printf '%s' "$r" | binary Attributes content)"
check "write units to acquire"             "2.0"    "$(printf '%s' "$r" | capacity)"
index_check "held index now holds the lock" "1" index_count held

echo
echo "contention — the denial has to name the holder"
r="$(acquire "AAAAAAAAAAE=" 1000 "YnJhbmNoLUI=" 9999 "branch-B#file-0001")"
check "refused"                          "ConditionalCheckFailedException" "$(printf '%s' "$r" | error)"
check "denial carries the current holder" "branch-A"                       "$(printf '%s' "$r" | binary Item holder)"

echo
echo "expiry is enforced on read, so no TTL is required"
r="$(acquire "AAAAAAAAAAE=" 99999 "YnJhbmNoLUI=" 199999 "branch-B#file-0001")"
check "stale holder is displaced" "rev01" "$(printf '%s' "$r" | binary Attributes revision)"

echo
echo "first edit — a file with no row at all"
r="$(acquire "AAAAAAAAAAI=" 1000 "YnJhbmNoLUM=" 9999 "branch-C#file-0002")"
check "no prior state to compare" "absent" "$(printf '%s' "$r" | has_attributes)"

echo
echo "confirm — KEYS_ONLY means flipping a boolean touches no index"
r="$(ddb UpdateItem "{\"TableName\":\"${SPEC_TABLE}\",\"Key\":{\"k\":{\"B\":\"AAAAAAAAAAE=\"}},
  \"UpdateExpression\":\"SET confirmed=:c\",\"ExpressionAttributeValues\":{\":c\":{\"BOOL\":true}},
  \"ReturnConsumedCapacity\":\"INDEXES\"}")"
check "write units to confirm" "1.0" "$(printf '%s' "$r" | capacity)"

echo
echo "conclude — advance the chain and drop the lock in one write"
r="$(ddb UpdateItem "{\"TableName\":\"${SPEC_TABLE}\",\"Key\":{\"k\":{\"B\":\"AAAAAAAAAAE=\"}},
  \"ConditionExpression\":\"holder = :h\",
  \"UpdateExpression\":\"SET revision=:r, content=:c, scope_pk=:sp, scope_sk=:ss REMOVE holder, owner_id, confirmed, expires_at, held_pk, held_sk\",
  \"ExpressionAttributeValues\":{\":h\":{\"B\":\"YnJhbmNoLUI=\"},\":r\":{\"B\":\"cmV2MDI=\"},
    \":c\":{\"B\":\"Y29udDAy\"},\":sp\":{\"S\":\"scope-main#07\"},\":ss\":{\"S\":\"file-0001\"}},
  \"ReturnConsumedCapacity\":\"INDEXES\"}")"
check "write units to conclude" "2.0" "$(printf '%s' "$r" | capacity)"
index_check "lock left the held index" "1" index_count held

echo
echo "release on a mergeable file — the guard cannot reach chain data"
# file-0002 holds a lock and no chain: what a legacy lock on a mergeable file leaves.
r="$(delete_guarded "AAAAAAAAAAI=" "YnJhbmNoLUM=")"
check "lock-only row is deleted" "none" "$(printf '%s' "$r" | error)"
# Aim the same guard at a file that does have a chain: the guard alone must stop it.
acquire "AAAAAAAAAAE=" 1000 "YnJhbmNoLUQ=" 9999 "branch-D#file-0001" >/dev/null
r="$(delete_guarded "AAAAAAAAAAE=" "YnJhbmNoLUQ=")"
check "delete refused where a chain exists" "ConditionalCheckFailedException" "$(printf '%s' "$r" | error)"

echo
echo "one index serves both lock queries"
index_check "Query(repository)" "1" query_held "held_pk = :p" '{":p":{"S":"repo-x#07"}}'
index_check "Query(branch) by sort-key prefix" "1" \
    query_held "held_pk = :p AND begins_with(held_sk, :b)" '{":p":{"S":"repo-x#07"},":b":{"S":"branch-D#"}}'
index_check "Query(branch) misses another branch" "0" \
    query_held "held_pk = :p AND begins_with(held_sk, :b)" '{":p":{"S":"repo-x#07"},":b":{"S":"branch-Z#"}}'

echo
echo "BatchWriteItem carries no conditions, and never says so"
batch_file="$(mktemp)"
python3 -c "
import json, base64, sys
items = [{'PutRequest': {'Item': {'k': {'B': base64.b64encode(bytes([0,0,0,0,0,0,9,i])).decode()}}}} for i in range(26)]
open(sys.argv[1],'w').write(json.dumps({'RequestItems': {'${SPEC_TABLE}': items}}))" "${batch_file}"
r="$(ddb BatchWriteItem "$(cat "${batch_file}")")"
check "26 items is rejected" "ValidationException" "$(printf '%s' "$r" | error)"
rm -f "${batch_file}"
batch_file=""
# The trap: a condition on a batched write is accepted and silently inert, so a
# batched reservation path would review as correct and grant one lock twice.
r="$(ddb BatchWriteItem "{\"RequestItems\":{\"${SPEC_TABLE}\":[{\"PutRequest\":{\"Item\":{
  \"k\":{\"B\":\"AAAAAAAACRA=\"},
  \"ConditionExpression\":{\"S\":\"attribute_not_exists(holder)\"}}}}]}}")"
check "a condition on a batched put is not refused" "none" "$(printf '%s' "$r" | error)"

echo
echo "cost against the table this replaces"
# DynamoDB charges a transaction's table write double and each index write at
# the standard rate; DynamoDB Local charges the index writes double as well.
if [[ "${TARGET}" == aws ]]; then
    today_units="5.0"
else
    today_units="8.0"
fi
r="$(ddb TransactWriteItems "{\"TransactItems\":[{\"Put\":{
  \"TableName\":\"${TODAY_TABLE}\",
  \"Item\":{\"hash\":{\"B\":\"aGFzaG9mcGF0aDAwMDAwMDAwMDAwMDA=\"},
            \"repositoryBranch\":{\"B\":\"cmVwb2JyYW5jaDAwMDAwMDAwMDAw\"},
            \"repository\":{\"B\":\"cmVwbzAwMDAwMDAwMDAwMDAwMDA=\"},
            \"branch\":{\"B\":\"YnJhbmNoMDAwMDAwMDAwMDAwMDA=\"},
            \"ownerId\":{\"S\":\"tester\"},
            \"description\":{\"S\":\"Content/Characters/Hero/Meshes/SK_Hero.uasset\"},
            \"timestamp\":{\"S\":\"2026-01-01T00:00:00Z\"}},
  \"ConditionExpression\":\"attribute_not_exists(#pk)\",
  \"ExpressionAttributeNames\":{\"#pk\":\"hash\"}}}],
 \"ReturnConsumedCapacity\":\"INDEXES\"}")"
check "today: write units to acquire" "${today_units}" "$(printf '%s' "$r" | capacity)"
r="$(ddb TransactWriteItems "{\"TransactItems\":[{\"Delete\":{
  \"TableName\":\"${TODAY_TABLE}\",
  \"Key\":{\"hash\":{\"B\":\"aGFzaG9mcGF0aDAwMDAwMDAwMDAwMDA=\"},
         \"repositoryBranch\":{\"B\":\"cmVwb2JyYW5jaDAwMDAwMDAwMDAw\"}},
  \"ConditionExpression\":\"ownerId = :o\",
  \"ExpressionAttributeValues\":{\":o\":{\"S\":\"tester\"}}}}],
 \"ReturnConsumedCapacity\":\"INDEXES\"}")"
check "today: write units to release" "${today_units}" "$(printf '%s' "$r" | capacity)"

echo
echo "index propagation — ${index_reads} index reads after a write"
for line in "${index_slow[@]}"; do
    printf '  --    %s\n' "${line}"
done
printf '  --    %-46s %s\n' "matched on the first read" "${index_first_reads} of ${index_reads}"
printf '  --    %-46s %s\n' "longest lag" "${index_longest_ms} ms"

echo
if (( failures == 0 )); then
    echo "all checks passed"
    exit 0
fi
echo "${failures} check(s) failed"
exit 1
