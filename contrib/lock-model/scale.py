#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""contrib/lock-model/scale.py — the successor-locks lock table at scale.

verify.sh checks what the model means and what one operation costs. The
questions it leaves open are about aggregates rather than single calls, and
need loops, which is why they live here. Each is a section, named in
parentheses:

  Query   (query) what listing a scope costs once the scope holds a large
          number of live locks — the call an editor makes to populate its UI,
          and the only unmeasured operation on a routine path.

  Reserve (reserve) whether transactions that want overlapping files
          converge, and how many retries it costs them. The design's answer
          is a transaction-id tiebreak plus ascending key order; this races
          real transactions in ascending order, and again in random order as
          the comparison. The tiebreak is what makes both converge; ascending
          order bounds the wasted work, so its worst case takes fewer
          attempts.

  Status  (status) what reading the holder state of many files costs, 100 to
          a BatchGetItem: the read a client makes to learn whether an
          acquisition it lost track of landed.

  ForceRelease  (force-release) what taking the lock off many files with no
          holder check costs, for files that keep chain state and files that
          have none, and what a BatchWriteItem can do of it.

  cancel  (cancel) what dropping one transaction's unconfirmed reservations
          by primary key costs, and whether the reservations of an
          overlapping transaction and the files' chain state survive it, both
          one conditional write per reservation and 25 reservations to a
          BatchWriteItem.

  Page size  (page-size) what listing a scope through its 64 shards of the
          scope index costs at several Query page sizes.

  Throttle  (throttle) sustained reservation load on one scope: the acquire
          UpdateItem on distinct files of one scope, each followed by its
          release, at --throttle-rate operations a second (200) for
          --throttle-seconds (30), through the same window. It reports the
          rate achieved per 5-second interval, the throttled calls and when
          they came, the capacity consumed by the table and by each index,
          and latency percentiles.

Every measured operation reports its round trips, the capacity
ReturnConsumedCapacity reports for them, and its wall time. Calls made per
entry run through a window of WINDOW at a time, as the server dispatches them.
Each thread keeps its connection open between calls, as an SDK's connection
pool does, so a wall time carries no connection setup per call.

Targets:
  --target local, the default, runs against DynamoDB Local at --endpoint,
  http://localhost:8000 unless given, with a placeholder signature. Table
  names carry --table-prefix, empty unless given, and a table of the same
  name left by an earlier run is replaced.

  --target aws runs against real DynamoDB in --region, which it needs, at
  --endpoint, https://dynamodb.<region>.amazonaws.com unless given. It signs
  every request with SigV4, from the credentials `aws configure
  export-credentials` gives for --profile or, without it, from
  AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY and, for temporary credentials,
  AWS_SESSION_TOKEN. Table names carry --table-prefix,
  lore-lockmodel-$USER-<epoch>- unless given.

  Every section creates a table of its own, numbered in the order the run
  creates them, with the --tag tags, and waits for it to become ACTIVE, so
  on real DynamoDB most of a run's wall time is table creation.

Cleanup:
  An aws run stops when a table it would create exists already, and touches
  no table it did not create. Every run deletes the tables it created when it
  exits: on success, on failure, and on Ctrl-C, SIGTERM or SIGHUP. Before
  deleting a table it removes the --untag-before-delete tags and turns
  deletion protection off, since a process other than the run can turn it
  on. A deletion refused for access is retried while --untag-before-delete is
  given, since a policy keyed on a tag can take minutes to see the tag gone,
  and one that hasn't finished within --delete-wait-seconds (600) is
  reported, as is every table left when a signal stops the waiting, for
  deletion by hand.

Permissions:
  An aws run calls dynamodb:ListTables, CreateTable, DescribeTable,
  DeleteTable, UpdateItem, DeleteItem, BatchGetItem, BatchWriteItem, Query
  and Scan; TagResource with --tag; UntagResource with --untag-before-delete;
  and UpdateTable when a table it created has deletion protection on.

Index propagation:
  Real DynamoDB updates indexes asynchronously, so every read of an index
  that follows a write polls every 200 ms until the index shows the write,
  for up to --index-wait-seconds (10), and the run reports how long each
  took. Creating a table waits for it to become ACTIVE for up to
  --table-wait-seconds (600).

Cost on demand:
  Every section but throttle, at its defaults, writes about a million write
  request units and reads under 100,000 read units: under a dollar at the
  us-east-1 on-demand price of $0.625 per million write units. throttle
  writes four units an operation, 4 × --throttle-rate × --throttle-seconds,
  which is 24,000 at its defaults.

Throughput:
  Run throttle from the table's region, such as from EC2 in --region: from
  further away the round trip, rather than DynamoDB, limits what a window of
  WINDOW calls reaches. A new on-demand table serves up to 4,000 write units
  a second at first, and a rate above that throttles until the table scales.
  DynamoDB warm throughput (UpdateTable's WarmThroughput) pre-warms a table
  to a stated rate; this script does not set it.

Usage:
  docker run -d --name lore-ddb -p 8000:8000 amazon/dynamodb-local:latest \
    -jar DynamoDBLocal.jar -inMemory -sharedDb
  python3 contrib/lock-model/scale.py [option ...] [section ...]
  docker rm -f lore-ddb

  python3 contrib/lock-model/scale.py --target aws --region <region> \
    --profile <profile> [--tag <key>=<value>]... \
    [--untag-before-delete <key>]... [section ...]

With no section named, every section runs.

Exit codes:
  0  every check passed
  1  a check failed; the failing expectation is printed
  2  DynamoDB is not reachable at the endpoint, or a table did not become
     ACTIVE in time
  3  an option or section is unknown or invalid, or the region or
     credentials are missing
  4  a table the run would create exists already
  128 + n  stopped by signal n, after deleting the tables it created
"""

import argparse
import base64
import collections
import concurrent.futures
import getpass
import hashlib
import hmac
import http.client
import json
import math
import os
import random
import re
import signal
import struct
import subprocess
import sys
import threading
import time
import urllib.parse

# The run's configuration, which configure() sets from the command line.
TARGET = "local"
REGION = ""
ENDPOINT = "http://localhost:8000"
PREFIX = ""
TAGS = []
UNTAG_KEYS = []
CREDENTIALS = {}
INDEX_WAIT = 10.0
TABLE_WAIT = 600.0
DELETE_WAIT = 600.0

# The table the current section uses, which create_table() sets.
TABLE = ""
SCOPE = "scope-main#07"
SECTIONS = (
    "query",
    "reserve",
    "status",
    "force-release",
    "cancel",
    "page-size",
    "throttle",
)
SHARDS = 64
WINDOW = 16
LOCK_ATTRIBUTES = "holder, owner_id, confirmed, expires_at, held_pk, held_sk"
THROTTLING = ("ThrottlingException", "ProvisionedThroughputExceededException")
TRANSIENT = THROTTLING + (
    "RequestLimitExceeded",
    "InternalServerError",
    "ServiceUnavailable",
)
RETRIES = 10
THROTTLE_SCOPE = 7
INTERVAL = 5.0

failures = 0
connections = threading.local()
created = set()
table_count = 0
index_waits = []
stopping = threading.Event()
stop_deleting = threading.Event()


class TableTimeout(RuntimeError):
    pass


class TableExists(RuntimeError):
    pass


class Interrupted(Exception):
    def __init__(self, signum):
        super().__init__(signum)
        self.signum = signum


def on_signal(signum, _frame):
    raise Interrupted(signum)


def check(description, expected, actual):
    global failures
    if expected == actual:
        print(f"  ok    {description:<46} {actual}")
    else:
        print(f"  FAIL  {description:<46} expected {expected}, got {actual}")
        failures += 1


def note(description, value):
    print(f"  --    {description:<46} {value}")


def sigv4(method, host, path, headers, body, region, service, access_key, secret_key):
    """The SigV4 Authorization header for a request with no query string. It
    signs `host` and every header of `headers`, which must hold X-Amz-Date."""
    signed = {
        name.lower(): " ".join(str(value).split()) for name, value in headers.items()
    }
    signed["host"] = host
    names = sorted(signed)
    canonical_request = "\n".join(
        [
            method,
            path,
            "",
            "".join(f"{name}:{signed[name]}\n" for name in names),
            ";".join(names),
            hashlib.sha256(body).hexdigest(),
        ]
    )
    amz_date = signed["x-amz-date"]
    scope = f"{amz_date[:8]}/{region}/{service}/aws4_request"
    string_to_sign = "\n".join(
        [
            "AWS4-HMAC-SHA256",
            amz_date,
            scope,
            hashlib.sha256(canonical_request.encode()).hexdigest(),
        ]
    )
    signing_key = f"AWS4{secret_key}".encode()
    for part in (amz_date[:8], region, service, "aws4_request"):
        signing_key = hmac.new(signing_key, part.encode(), hashlib.sha256).digest()
    signature = hmac.new(
        signing_key, string_to_sign.encode(), hashlib.sha256
    ).hexdigest()
    return (
        f"AWS4-HMAC-SHA256 Credential={access_key}/{scope}, "
        f"SignedHeaders={';'.join(names)}, Signature={signature}"
    )


def connection():
    """This thread's connection to ENDPOINT, opened on first use and kept open
    between calls, as an SDK's connection pool keeps them."""
    current = getattr(connections, "current", None)
    if current is None:
        parts = urllib.parse.urlsplit(ENDPOINT)
        kind = (
            http.client.HTTPSConnection
            if parts.scheme == "https"
            else http.client.HTTPConnection
        )
        current = connections.current = kind(parts.hostname, parts.port, timeout=60)
    return current


def drop_connection():
    """Closes this thread's connection, so its next call opens a fresh one."""
    current = getattr(connections, "current", None)
    if current is not None:
        current.close()
        connections.current = None


def ddb_once(target, body):
    """One DynamoDB call, on this thread's connection. In aws mode it is
    signed with SigV4; DynamoDB Local accepts any signature, so in local mode
    the Authorization header is a placeholder. DynamoDB answers some
    successful calls, such as UntagResource, with an empty body, which returns
    as {}. A call that doesn't reach DynamoDB or loses its answer, such as one
    whose connection times out or was closed while idle, is sent again on a
    fresh connection after a backoff, as an SDK sends it again, and returns as
    a TransportError once every attempt has failed."""
    data = json.dumps(body).encode()
    headers = {
        "Content-Type": "application/x-amz-json-1.0",
        "X-Amz-Target": f"DynamoDB_20120810.{target}",
    }
    if TARGET == "aws":
        host = urllib.parse.urlsplit(ENDPOINT).netloc
        headers["X-Amz-Date"] = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
        if CREDENTIALS["session_token"]:
            headers["X-Amz-Security-Token"] = CREDENTIALS["session_token"]
        headers["Authorization"] = sigv4(
            "POST",
            host,
            "/",
            headers,
            data,
            REGION,
            "dynamodb",
            CREDENTIALS["access_key"],
            CREDENTIALS["secret_key"],
        )
        headers["Host"] = host
    else:
        headers["Authorization"] = (
            "AWS4-HMAC-SHA256 Credential=local/20260101/us-east-2/dynamodb/"
            "aws4_request, SignedHeaders=host;x-amz-date;x-amz-target, "
            "Signature=placeholder"
        )
    for attempt in range(RETRIES):
        try:
            client = connection()
            client.request("POST", "/", body=data, headers=headers)
            answer = client.getresponse().read()
            break
        except (http.client.HTTPException, OSError) as error:
            drop_connection()
            failure = str(error)
            time.sleep(backoff(attempt))
    else:
        return {"__type": "TransportError", "message": failure}
    return json.loads(answer) if answer else {}


def error_name(response):
    """The exception a response names, without its namespace, or "" for none."""
    return response.get("__type", "").rsplit("#", 1)[-1]


def backoff(attempt):
    return min(1.0, 0.025 * 2**attempt) * random.uniform(0.5, 1.0)


def ddb(target, body):
    """One DynamoDB call, retried with backoff while DynamoDB throttles it or
    reports a transient failure, as an SDK retries."""
    for attempt in range(RETRIES):
        response = ddb_once(target, body)
        if error_name(response) not in TRANSIENT:
            break
        time.sleep(backoff(attempt))
    return response


def key(index, scope=0):
    """A 16-byte file id, as the model keys on. `scope` fills its high half."""
    return base64.b64encode(struct.pack(">QQ", scope, index)).decode()


def require_ok(target, response):
    if "__type" in response:
        raise RuntimeError(f"{target} failed: {response}")
    return response


def table_state(table):
    """ACTIVE when `table` and every index are active, GONE when it does not
    exist, and otherwise the statuses DynamoDB reports."""
    response = ddb("DescribeTable", {"TableName": table})
    if error_name(response) == "ResourceNotFoundException":
        return "GONE"
    description = require_ok("DescribeTable", response)["Table"]
    states = [description.get("TableStatus")] + [
        index.get("IndexStatus")
        for index in description.get("GlobalSecondaryIndexes", [])
    ]
    if all(state == "ACTIVE" for state in states):
        return "ACTIVE"
    return "/".join(map(str, states))


def wait_for_table(table, wanted):
    """Polls until `table` is `wanted`, ACTIVE or GONE, for up to TABLE_WAIT."""
    deadline = time.time() + TABLE_WAIT
    while True:
        state = table_state(table)
        if state == wanted:
            return
        if time.time() >= deadline:
            raise TableTimeout(
                f"table {table} is {state}, not {wanted}, after {TABLE_WAIT:.0f}s"
            )
        time.sleep(0.5)


def deletion_step(step, response):
    """None when the response to one step of a deletion succeeded; otherwise
    the outcome delete_attempt() reports for it."""
    name = error_name(response)
    if name in ("", "ResourceNotFoundException"):
        return None
    if name == "ResourceInUseException":
        return "waiting", "not gone yet"
    if name == "AccessDeniedException" and UNTAG_KEYS:
        return "retry", f"{step} refused: {response}"
    if name == "ValidationException" and "protect" in json.dumps(response):
        return "retry", f"{step} refused: {response}"
    return "failed", f"{step} failed: {response}"


def delete_attempt(table):
    """One round towards deleting a table this run created, as an outcome and
    its reason: gone; waiting while the deletion runs or the table is busy;
    retry for a refusal that can clear; or failed."""
    response = ddb("DescribeTable", {"TableName": table})
    if error_name(response) == "ResourceNotFoundException":
        return "gone", ""
    if "Table" not in response:
        return "failed", f"describing failed: {response}"
    description = response["Table"]
    if description.get("TableStatus") in ("DELETING", "CREATING", "UPDATING"):
        return "waiting", "not gone yet"
    steps = []
    if UNTAG_KEYS:
        steps.append(
            (
                "untagging",
                "UntagResource",
                {"ResourceArn": description["TableArn"], "TagKeys": UNTAG_KEYS},
            )
        )
    if description.get("DeletionProtectionEnabled"):
        steps.append(
            (
                "turning deletion protection off",
                "UpdateTable",
                {"TableName": table, "DeletionProtectionEnabled": False},
            )
        )
    steps.append(("deleting", "DeleteTable", {"TableName": table}))
    for step, target, body in steps:
        outcome = deletion_step(step, ddb(target, body))
        if outcome:
            return outcome
    return "waiting", "not gone yet"


def delete_tables(tables):
    """Deletes `tables`, created by this run, and waits until each is gone, for
    up to DELETE_WAIT or until stop_deleting is set. Returns each table left
    behind, with the reason."""
    deadline = time.time() + DELETE_WAIT
    pending = {table: "not gone yet" for table in tables}
    left = {}
    while pending:
        delay = 0.5
        for table in list(pending):
            try:
                outcome, reason = delete_attempt(table)
            except (OSError, ValueError) as failure:
                outcome, reason = "retry", f"request failed: {failure}"
            if outcome == "gone":
                del pending[table]
                if TARGET == "aws":
                    print(f"deleted table {table}")
            elif outcome == "failed":
                del pending[table]
                left[table] = reason
            else:
                pending[table] = reason
                if outcome == "retry":
                    delay = 15.0
        if not pending or stop_deleting.is_set() or time.time() >= deadline:
            break
        stop_deleting.wait(delay)
    left.update(pending)
    return left


def create_table():
    """Creates the run's next table, makes it TABLE, and waits until it and its
    indexes are ACTIVE. In local mode a table of the same name an earlier run
    left is deleted first; in aws mode one that exists stops the run."""
    global TABLE, table_count
    table_count += 1
    TABLE = f"{PREFIX}scale-locks-{table_count}"
    if TARGET == "local":
        for table, reason in delete_tables([TABLE]).items():
            raise TableTimeout(f"table {table} could not be replaced: {reason}")
    elif table_state(TABLE) != "GONE":
        raise TableExists(TABLE)
    # Registered before the request: the check above showed the name unused,
    # so a table of this name can only be this run's, and an interrupt that
    # lands before the response still has it deleted.
    created.add(TABLE)
    body = {
        "TableName": TABLE,
        "BillingMode": "PAY_PER_REQUEST",
        "AttributeDefinitions": [
            {"AttributeName": "k", "AttributeType": "B"},
            {"AttributeName": "held_pk", "AttributeType": "S"},
            {"AttributeName": "held_sk", "AttributeType": "S"},
            {"AttributeName": "scope_pk", "AttributeType": "S"},
            {"AttributeName": "scope_sk", "AttributeType": "S"},
        ],
        "KeySchema": [{"AttributeName": "k", "KeyType": "HASH"}],
        "GlobalSecondaryIndexes": [
            {
                "IndexName": "held",
                "KeySchema": [
                    {"AttributeName": "held_pk", "KeyType": "HASH"},
                    {"AttributeName": "held_sk", "KeyType": "RANGE"},
                ],
                "Projection": {"ProjectionType": "KEYS_ONLY"},
            },
            {
                "IndexName": "scope",
                "KeySchema": [
                    {"AttributeName": "scope_pk", "KeyType": "HASH"},
                    {"AttributeName": "scope_sk", "KeyType": "RANGE"},
                ],
                "Projection": {"ProjectionType": "KEYS_ONLY"},
            },
        ],
    }
    if TAGS:
        body["Tags"] = TAGS
    response = ddb("CreateTable", body)
    require_ok("CreateTable", response)
    wait_for_table(TABLE, "ACTIVE")


def delete_created_tables():
    """Stops the run's workers and deletes every table this run created. A
    signal while it waits stops the waiting, and every table not yet gone is
    reported for deletion by hand."""
    stopping_signals = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)
    previous = {
        signum: signal.signal(signum, lambda _signum, _frame: stop_deleting.set())
        for signum in stopping_signals
    }
    stopping.set()
    try:
        left = delete_tables(sorted(created))
    finally:
        for signum, handler in previous.items():
            signal.signal(signum, handler)
    for table, reason in sorted(left.items()):
        print(
            f"table {table} is left behind; delete it by hand. {reason}",
            file=sys.stderr,
        )
    created.clear()


def require_dynamodb():
    try:
        response = ddb("ListTables", {})
    except (OSError, ValueError) as failure:
        response = {"failure": str(failure)}
    if "TableNames" not in response:
        if TARGET == "aws":
            print(f"DynamoDB unreachable at {ENDPOINT}: {response}", file=sys.stderr)
        else:
            print(
                f"DynamoDB Local unreachable at {ENDPOINT} — see the usage note above.",
                file=sys.stderr,
            )
        sys.exit(2)


class Parser(argparse.ArgumentParser):
    """An argument parser that exits with the script's code for an invalid
    invocation, 3, rather than argparse's 2, which means unreachable here."""

    def error(self, message):
        self.print_usage(sys.stderr)
        self.exit(3, f"{self.prog}: {message}\n")


def tag(text):
    key, separator, value = text.partition("=")
    if not key or not separator:
        raise argparse.ArgumentTypeError(f"takes KEY=VALUE, not {text!r}")
    return {"Key": key, "Value": value}


def positive(kind):
    def parse(text):
        value = kind(text)
        if value <= 0:
            raise argparse.ArgumentTypeError(f"must be above zero, not {text!r}")
        return value

    parse.__name__ = kind.__name__
    return parse


def seconds(text):
    value = float(text)
    if value < 0:
        raise argparse.ArgumentTypeError(f"must not be negative, not {text!r}")
    return value


def parser():
    result = Parser(
        prog="scale.py",
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    add = result.add_argument
    add("sections", nargs="*", metavar="section", help=", ".join(SECTIONS))
    add("--target", choices=("local", "aws"), default="local")
    add("--endpoint", metavar="URL", help="the DynamoDB endpoint")
    add("--region", help="the AWS region, required for --target aws")
    add(
        "--profile",
        metavar="NAME",
        help="an AWS CLI profile to take credentials from",
    )
    add("--table-prefix", metavar="PREFIX", help="prepended to every table name")
    add(
        "--tag",
        type=tag,
        action="append",
        default=[],
        metavar="KEY=VALUE",
        help="a tag on every table the run creates; repeatable",
    )
    add(
        "--untag-before-delete",
        action="append",
        default=[],
        metavar="KEY",
        help="a tag removed from a table before deleting it; repeatable",
    )
    add("--index-wait-seconds", type=seconds, default=10.0, metavar="N")
    add("--table-wait-seconds", type=seconds, default=600.0, metavar="N")
    add("--delete-wait-seconds", type=seconds, default=600.0, metavar="N")
    add(
        "--locks",
        type=positive(int),
        default=100_000,
        metavar="N",
        help="live locks in the scope query and page-size list",
    )
    add(
        "--workers",
        type=positive(int),
        default=8,
        metavar="N",
        help="transactions reserve races",
    )
    add(
        "--contended-keys",
        type=positive(int),
        default=40,
        metavar="N",
        help="files every reserve transaction wants",
    )
    add(
        "--operation-keys",
        type=positive(int),
        default=10_000,
        metavar="N",
        help="files status, force-release and cancel operate on",
    )
    add(
        "--throttle-rate",
        type=positive(float),
        default=200.0,
        metavar="N",
        help="operations a second throttle sustains",
    )
    add(
        "--throttle-seconds",
        type=positive(float),
        default=30.0,
        metavar="N",
        help="how long throttle sustains them",
    )
    return result


def default_prefix(target):
    if target != "aws":
        return ""
    try:
        user = os.environ.get("USER") or getpass.getuser()
    except OSError:
        user = "user"
    return f"lore-lockmodel-{re.sub(r'[^A-Za-z0-9_.-]', '-', user)}-{int(time.time())}-"


def profile_credentials(profile):
    """The credentials `aws configure export-credentials` gives for `profile`,
    or exits with code 3 when it gives none."""
    try:
        result = subprocess.run(
            [
                "aws",
                "configure",
                "export-credentials",
                "--profile",
                profile,
                "--format",
                "process",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError as failure:
        print(f"--profile needs the AWS CLI: {failure}", file=sys.stderr)
        sys.exit(3)
    if result.returncode != 0:
        print(result.stderr.strip(), file=sys.stderr)
        print(
            f"no credentials for profile {profile!r}; log it in and run again",
            file=sys.stderr,
        )
        sys.exit(3)
    exported = json.loads(result.stdout)
    return {
        "access_key": exported.get("AccessKeyId", ""),
        "secret_key": exported.get("SecretAccessKey", ""),
        "session_token": exported.get("SessionToken", ""),
    }


def configure(arguments):
    """Sets the run's configuration from its command line and returns the
    options, or exits with code 3 for an invalid invocation."""
    global TARGET, REGION, ENDPOINT, PREFIX, TAGS, UNTAG_KEYS, CREDENTIALS
    global INDEX_WAIT, TABLE_WAIT, DELETE_WAIT
    arguments_parser = parser()
    options = arguments_parser.parse_args(arguments)
    unknown = [name for name in options.sections if name not in SECTIONS]
    if unknown:
        arguments_parser.error(
            f"unknown section {', '.join(unknown)}; the sections are {', '.join(SECTIONS)}"
        )
    if options.target == "local" and options.profile:
        arguments_parser.error("--profile needs --target aws")
    if options.target == "aws" and not options.region:
        arguments_parser.error("--target aws needs --region")
    prefix = options.table_prefix
    if prefix is None:
        prefix = default_prefix(options.target)
    if not re.fullmatch(r"[A-Za-z0-9_.-]*", prefix):
        arguments_parser.error(
            f"--table-prefix {prefix!r} holds a character a table name cannot"
        )
    if options.untag_before_delete and not all(options.untag_before_delete):
        arguments_parser.error("--untag-before-delete needs a tag key")

    if options.target == "aws":
        if options.profile:
            credentials = profile_credentials(options.profile)
        else:
            credentials = {
                "access_key": os.environ.get("AWS_ACCESS_KEY_ID", ""),
                "secret_key": os.environ.get("AWS_SECRET_ACCESS_KEY", ""),
                "session_token": os.environ.get("AWS_SESSION_TOKEN", ""),
            }
        if not (credentials["access_key"] and credentials["secret_key"]):
            print(
                "--target aws needs credentials: --profile NAME, or "
                "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY in the environment",
                file=sys.stderr,
            )
            sys.exit(3)
        CREDENTIALS = credentials
        default_endpoint = f"https://dynamodb.{options.region}.amazonaws.com"
    else:
        default_endpoint = "http://localhost:8000"

    TARGET = options.target
    REGION = options.region or ""
    ENDPOINT = (options.endpoint or default_endpoint).rstrip("/")
    PREFIX = prefix
    TAGS = options.tag
    UNTAG_KEYS = options.untag_before_delete
    INDEX_WAIT = options.index_wait_seconds
    TABLE_WAIT = options.table_wait_seconds
    DELETE_WAIT = options.delete_wait_seconds
    return options


def await_index(description, read, expected):
    """`read()` repeated every 200 ms until it returns `expected`, for up to
    INDEX_WAIT seconds: an index shows a write only once DynamoDB has
    propagated it. Records how many reads it took and the lag before the read
    that matched, and returns the last value read."""
    first = time.time()
    reads = 0
    while True:
        started = time.time()
        value = read()
        reads += 1
        if value == expected or started - first >= INDEX_WAIT:
            index_waits.append((description, reads, started - first, value == expected))
            return value
        time.sleep(0.2)


def report_index_waits():
    if not index_waits:
        return
    print(f"\nindex propagation — {len(index_waits)} index reads after a write")
    for description, reads, lag, settled in index_waits:
        if reads > 1 or not settled:
            outcome = "matched" if settled else "still unmatched"
            note(description, f"{outcome} after {lag:.2f}s, {reads} reads")
    first = sum(1 for _, reads, _, settled in index_waits if reads == 1 and settled)
    note("matched on the first read", f"{first} of {len(index_waits)}")
    note("longest lag", f"{max(lag for _, _, lag, _ in index_waits):.2f}s")


def seed_locks(count, threads=16):
    """`count` held locks in one scope, written 25 at a time, and how many
    calls DynamoDB throttled with a count of each reason they gave.

    Every row carries both index key pairs, which is what a live lock on a
    file that also has chain state looks like. Both pairs are unsharded, so
    every row lands on one partition key of each index."""
    per_batch = 25
    batches = [
        list(range(start, min(start + per_batch, count)))
        for start in range(0, count, per_batch)
    ]
    lock = threading.Lock()
    remaining = list(batches)
    throttled = []

    def worker():
        while not stopping.is_set():
            with lock:
                if not remaining:
                    return
                batch = remaining.pop()
            request = {
                TABLE: [
                    {
                        "PutRequest": {
                            "Item": {
                                "k": {"B": key(index)},
                                "holder": {"B": key(1)},
                                "owner_id": {"S": "seed"},
                                "confirmed": {"BOOL": True},
                                "expires_at": {"N": "9999999999"},
                                "revision": {"B": key(2)},
                                "content": {"B": key(3)},
                                "held_pk": {"S": "repo-x#07"},
                                "held_sk": {"S": f"branch-main#{index:08d}"},
                                "scope_pk": {"S": SCOPE},
                                "scope_sk": {"S": f"file-{index:08d}"},
                            }
                        }
                    }
                    for index in batch
                ]
            }
            result = write_fixture(request)
            with lock:
                throttled.append(result)

    pool = [threading.Thread(target=worker) for _ in range(threads)]
    for thread in pool:
        thread.start()
    for thread in pool:
        thread.join()
    return total_throttling(throttled)


def query_scope(page_limit=None):
    """Page the scope index as a server-streaming Query would, and total what
    it reads."""
    pages, items, capacity = 0, 0, 0.0
    start_key = None
    while True:
        body = {
            "TableName": TABLE,
            "IndexName": "scope",
            "KeyConditionExpression": "scope_pk = :s",
            "ExpressionAttributeValues": {":s": {"S": SCOPE}},
            "ReturnConsumedCapacity": "INDEXES",
        }
        if page_limit:
            body["Limit"] = page_limit
        if start_key:
            body["ExclusiveStartKey"] = start_key
        response = require_ok("Query", ddb("Query", body))
        pages += 1
        items += response.get("Count", 0)
        consumed = response.get("ConsumedCapacity")
        if consumed:
            consumed = consumed[0] if isinstance(consumed, list) else consumed
            capacity += consumed.get("CapacityUnits", 0.0)
        start_key = response.get("LastEvaluatedKey")
        if not start_key:
            return pages, items, capacity


def reserve(file_key, transaction, now, expires):
    """One reservation: the conditional update the design acquires with.
    Returns the holding transaction id when the condition fails."""
    response = ddb(
        "UpdateItem",
        {
            "TableName": TABLE,
            "Key": {"k": {"B": file_key}},
            "ConditionExpression": "attribute_not_exists(holder) OR expires_at < :now",
            "UpdateExpression": (
                "SET holder=:h, owner_id=:o, confirmed=:c, expires_at=:e, "
                "held_pk=:hp, held_sk=:hs"
            ),
            "ExpressionAttributeValues": {
                ":now": {"N": str(now)},
                ":h": {"B": file_key},
                ":o": {"S": transaction},
                ":c": {"BOOL": False},
                ":e": {"N": str(expires)},
                ":hp": {"S": "repo-x#07"},
                ":hs": {"S": f"branch-{transaction}"},
            },
            "ReturnValuesOnConditionCheckFailure": "ALL_OLD",
        },
    )
    if "__type" in response:
        holder = response.get("Item", {}).get("owner_id", {}).get("S")
        return False, holder
    return True, None


def cancel(keys, transaction):
    """Drop exactly this transaction's reservations, as `cancel` does."""
    for file_key in keys:
        ddb(
            "DeleteItem",
            {
                "TableName": TABLE,
                "Key": {"k": {"B": file_key}},
                "ConditionExpression": "owner_id = :o",
                "ExpressionAttributeValues": {":o": {"S": transaction}},
            },
        )


def race(workers, keys, ascending, attempt_cap=1000):
    """Race `workers` transactions for the same `keys`.

    Every worker wants every key, and reserves them in its key order. On
    contention the design's rule decides: with a total order on transaction
    ids, a conflict with a higher id's reservation keeps every reservation
    held and retries the key it met, and a conflict with a lower id's
    reservation yields every reservation held and starts over after a random
    backoff, so the lowest contender always makes progress. A key a worker
    holds already counts as granted. A worker that reaches `attempt_cap` gives
    up and releases what it holds, as a stream closed without `Confirm` does.

    The transaction-id tiebreak is what makes the workers converge, in any key
    order. `ascending` selects the key order under test: ascending order makes
    overlapping transactions contend at their first shared key, which bounds
    the work a yield throws away, so it converges in fewer attempts than
    random order rather than being required to converge at all."""
    create_table()

    results = {}
    lock = threading.Lock()

    def worker(index):
        transaction = f"t{index:04d}"
        order = list(keys)
        if not ascending:
            random.Random(index).shuffle(order)
        attempts, held, position = 0, [], 0
        while attempts < attempt_cap and not stopping.is_set():
            attempts += 1
            now = int(time.time())
            conflict = None
            while position < len(order):
                file_key = order[position]
                ok, holder = reserve(file_key, transaction, now, now + 300)
                if not ok and holder != transaction:
                    conflict = holder
                    break
                if file_key not in held:
                    held.append(file_key)
                position += 1
            if position == len(order):
                with lock:
                    results[transaction] = (attempts, len(held))
                cancel(held, transaction)  # done: release what it took
                return
            if conflict is not None and transaction < conflict:
                time.sleep(0.01)
            else:
                cancel(held, transaction)
                held, position = [], 0
                time.sleep(0.01 * random.random())
        cancel(held, transaction)
        with lock:
            results[transaction] = (attempts, -1)

    pool = [threading.Thread(target=worker, args=(i,)) for i in range(workers)]
    started = time.time()
    for thread in pool:
        thread.start()
    for thread in pool:
        thread.join()
    elapsed = time.time() - started

    completed = sum(1 for attempts, held in results.values() if held >= 0)
    worst = max(attempts for attempts, _ in results.values())
    return completed, worst, elapsed


def capacity_of(response):
    """The capacity units a response reports, summed over the tables it names."""
    consumed = response.get("ConsumedCapacity") or []
    if isinstance(consumed, dict):
        consumed = [consumed]
    return sum(entry.get("CapacityUnits", 0.0) for entry in consumed)


def capacity_parts(response):
    """The capacity units a response reports, in total, for the table, and for
    each index it names."""
    parts = {}
    consumed = response.get("ConsumedCapacity") or []
    if isinstance(consumed, dict):
        consumed = [consumed]
    for entry in consumed:
        found = [("total", entry)]
        if "Table" in entry:
            found.append(("table", entry["Table"]))
        found.extend(
            (f"index {name}", index)
            for name, index in entry.get("GlobalSecondaryIndexes", {}).items()
        )
        for part, units in found:
            parts[part] = parts.get(part, 0.0) + units.get("CapacityUnits", 0.0)
    return parts


class Tally:
    """The round trips one operation makes, the capacity they consume, and the
    wall time from the tally's creation to `stop`, counted across threads. A
    throttled call is retried with backoff, and each attempt is a round trip."""

    def __init__(self):
        self.lock = threading.Lock()
        self.calls = 0
        self.units = 0.0
        self.throttled = 0
        self.started = time.time()
        self.elapsed = 0.0

    def call(self, target, body):
        for attempt in range(RETRIES):
            response = ddb_once(target, body)
            with self.lock:
                self.calls += 1
                self.units += capacity_of(response)
            if error_name(response) not in THROTTLING:
                break
            with self.lock:
                self.throttled += 1
            time.sleep(backoff(attempt))
        return response

    def stop(self):
        self.elapsed = time.time() - self.started
        return self

    def summary(self, unit):
        throttled = f", {self.throttled:,} throttled" if self.throttled else ""
        return (
            f"{self.calls:,} round trips, {self.units:,.1f} {unit}, "
            f"{self.elapsed:.2f}s{throttled}"
        )


def in_window(work, items, window=WINDOW):
    """`work` over `items`, at most `window` calls in flight, results in order.
    An exception, an interrupt among them, cancels the calls not yet started."""
    pool = concurrent.futures.ThreadPoolExecutor(max_workers=window)
    try:
        return list(pool.map(work, items))
    finally:
        pool.shutdown(wait=True, cancel_futures=True)


def chunks(items, size):
    items = list(items)
    return [items[start : start + size] for start in range(0, len(items), size)]


def shard(index):
    """The suffix every index partition key carries: the low bits of the entry
    key, across a fixed 64 buckets."""
    return f"{index % SHARDS:02d}"


def lock_attributes(index, transaction):
    """The attributes a confirmed lock adds to a file's item."""
    return {
        "holder": {"B": key(1)},
        "owner_id": {"S": transaction},
        "confirmed": {"BOOL": True},
        "expires_at": {"N": "9999999999"},
        "held_pk": {"S": f"repo-x#{shard(index)}"},
        "held_sk": {"S": f"branch-main#{index:08d}"},
    }


def chain_attributes(index, scope):
    """The chain half of a file's item, which a concluded session leaves."""
    return {
        "revision": {"B": key(2)},
        "content": {"B": key(3)},
        "scope_pk": {"S": f"{scope}#{shard(index)}"},
        "scope_sk": {"S": f"file-{index:08d}"},
    }


def throttle_reasons(response):
    """The reasons a throttled response gives, each with the table or index it
    names, such as IndexWriteKeyRangeThroughputExceeded on index held."""
    labels = []
    for entry in response.get("throttlingReasons") or [{}]:
        resource = entry.get("resource", "")
        where = (
            f"index {resource.rsplit('/index/', 1)[1]}"
            if "/index/" in resource
            else "table"
        )
        labels.append(f"{entry.get('reason', error_name(response))} on {where}")
    return labels


def write_fixture(request):
    """Writes one BatchWriteItem request of fixtures until DynamoDB has
    processed all of it, and returns how many calls DynamoDB throttled with
    a count of each reason they gave. The measurement after a fixture means
    nothing unless the fixture is complete, so this outlasts throttling that
    ddb() gives up on."""
    calls, reasons, attempt = 0, collections.Counter(), 0
    while request and not stopping.is_set():
        response = ddb_once("BatchWriteItem", {"RequestItems": request})
        name = error_name(response)
        if name in THROTTLING:
            calls += 1
            reasons.update(throttle_reasons(response))
        elif name not in TRANSIENT:
            request = require_ok("BatchWriteItem", response).get("UnprocessedItems")
        if request:
            time.sleep(backoff(attempt))
            attempt += 1
    return calls, reasons


def total_throttling(results):
    """The throttled calls and reasons of several write_fixture() results."""
    calls, reasons = 0, collections.Counter()
    for result_calls, result_reasons in results:
        calls += result_calls
        reasons.update(result_reasons)
    return calls, reasons


def note_throttling(calls, reasons):
    note("calls throttled while seeding", f"{calls:,}")
    for reason, count in sorted(reasons.items()):
        note(f"  {reason}", f"{count:,}")


def put_items(items):
    """Writes `items` as fixtures, 25 to a BatchWriteItem, and returns how
    many calls DynamoDB throttled with a count of each reason they gave."""
    return total_throttling(
        in_window(
            lambda batch: write_fixture(
                {TABLE: [{"PutRequest": {"Item": item}} for item in batch]}
            ),
            chunks(items, 25),
        )
    )


def read_back(keys):
    """The items of `keys` that exist, read strongly consistent and keyed by
    file key. Not part of any measurement."""

    def read(batch):
        request = {
            TABLE: {"Keys": [{"k": {"B": k}} for k in batch], "ConsistentRead": True}
        }
        found = []
        attempt = 0
        while request:
            response = require_ok(
                "BatchGetItem", ddb("BatchGetItem", {"RequestItems": request})
            )
            found.extend(response.get("Responses", {}).get(TABLE, []))
            request = response.get("UnprocessedKeys") or None
            if request:
                time.sleep(backoff(attempt))
                attempt += 1
        return found

    return {
        item["k"]["B"]: item
        for found in in_window(read, chunks(keys, 100))
        for item in found
    }


def count_where(body):
    """Pages a COUNT Scan of the table or one of its indexes to the end. A
    Scan of the table reads strongly consistent; an index cannot."""
    count, start_key = 0, None
    while True:
        page = dict(body, TableName=TABLE, Select="COUNT")
        if "IndexName" not in body:
            page["ConsistentRead"] = True
        if start_key:
            page["ExclusiveStartKey"] = start_key
        response = require_ok("Scan", ddb("Scan", page))
        count += response.get("Count", 0)
        start_key = response.get("LastEvaluatedKey")
        if not start_key:
            return count


def reservations_of(transaction):
    return count_where(
        {
            "FilterExpression": "owner_id = :o",
            "ExpressionAttributeValues": {":o": {"S": transaction}},
        }
    )


def status(keys, consistent):
    """Reads the holder state of `keys`, 100 to a BatchGetItem, and returns how
    many of them are held."""
    tally = Tally()

    def read(batch):
        request = {
            TABLE: {
                "Keys": [{"k": {"B": file_key}} for file_key in batch],
                "ProjectionExpression": "#k, #h, #o, #c, #e",
                "ExpressionAttributeNames": {
                    "#k": "k",
                    "#h": "holder",
                    "#o": "owner_id",
                    "#c": "confirmed",
                    "#e": "expires_at",
                },
                "ConsistentRead": consistent,
            }
        }
        held = 0
        attempt = 0
        while request:
            response = require_ok(
                "BatchGetItem",
                tally.call(
                    "BatchGetItem",
                    {"RequestItems": request, "ReturnConsumedCapacity": "TOTAL"},
                ),
            )
            held += sum(
                1
                for item in response.get("Responses", {}).get(TABLE, [])
                if "holder" in item
            )
            request = response.get("UnprocessedKeys") or None
            if request:
                time.sleep(backoff(attempt))
                attempt += 1
        return held

    held = sum(in_window(read, chunks(keys, 100)))
    return held, tally.stop()


def force_release(keys, has_chain):
    """Takes the lock off every file of `keys` with no holder check, one write
    each. A file with chain state keeps it, so only its lock attributes go; a
    file without has nothing left to keep, so its item goes."""
    tally = Tally()

    def release(file_key):
        body = {
            "TableName": TABLE,
            "Key": {"k": {"B": file_key}},
            "ReturnConsumedCapacity": "INDEXES",
        }
        if has_chain:
            body["UpdateExpression"] = f"REMOVE {LOCK_ATTRIBUTES}"
            require_ok("UpdateItem", tally.call("UpdateItem", body))
        else:
            require_ok("DeleteItem", tally.call("DeleteItem", body))

    in_window(release, keys)
    return tally.stop()


def delete_batched(keys):
    """Deletes every item of `keys` whole, 25 to a BatchWriteItem. A batch
    carries puts and deletes of whole items only, with no condition and no
    partial update."""
    tally = Tally()

    def delete(batch):
        request = {TABLE: [{"DeleteRequest": {"Key": {"k": {"B": k}}}} for k in batch]}
        attempt = 0
        while request:
            response = require_ok(
                "BatchWriteItem",
                tally.call(
                    "BatchWriteItem",
                    {"RequestItems": request, "ReturnConsumedCapacity": "INDEXES"},
                ),
            )
            request = response.get("UnprocessedItems") or None
            if request:
                time.sleep(backoff(attempt))
                attempt += 1

    in_window(delete, chunks(keys, 25))
    return tally.stop()


def reserve_returning_old(index, transaction, now):
    """One reservation as acquire makes it, returning the file's prior
    attributes, or None where another transaction holds the file."""
    response = ddb(
        "UpdateItem",
        {
            "TableName": TABLE,
            "Key": {"k": {"B": key(index)}},
            "ConditionExpression": "attribute_not_exists(holder) OR expires_at < :now",
            "UpdateExpression": (
                "SET holder=:h, owner_id=:o, confirmed=:c, expires_at=:e, "
                "held_pk=:hp, held_sk=:hs"
            ),
            "ExpressionAttributeValues": {
                ":now": {"N": str(now)},
                ":h": {"B": key(index)},
                ":o": {"S": transaction},
                ":c": {"BOOL": False},
                ":e": {"N": str(now + 300)},
                ":hp": {"S": f"repo-x#{shard(index)}"},
                ":hs": {"S": f"branch-{transaction}#{index:08d}"},
            },
            "ReturnValues": "ALL_OLD",
        },
    )
    if response.get("__type", "").endswith("ConditionalCheckFailedException"):
        return None
    return require_ok("UpdateItem", response).get("Attributes", {})


def cancel_fixture(count):
    """t0002 reserves files that t0001 then also wants, and t0001 is granted
    `count` reservations beside them. Every other file carries chain state
    from an earlier session. Returns t0001's granted reservations, each with
    whether its file has a chain as the reservation's prior state showed, and
    how many t0002 holds."""
    create_table()
    overlap = max(count // 2, 1)
    wanted = list(range(count + overlap))
    put_items(
        [
            {"k": {"B": key(i)}, **chain_attributes(i, "scope-cancel")}
            for i in wanted
            if i % 2 == 0
        ]
    )
    now = int(time.time())
    in_window(lambda index: reserve_returning_old(index, "t0002", now), wanted[count:])
    prior = in_window(lambda index: reserve_returning_old(index, "t0001", now), wanted)
    granted = [
        (key(index), "revision" in old)
        for index, old in zip(wanted, prior)
        if old is not None
    ]
    return granted, overlap


def cancel_by_key(granted, transaction):
    """Drops `transaction`'s unconfirmed reservations by primary key, one
    conditional write each: a file with chain state loses its lock attributes, a file
    without is deleted. The condition names the transaction, so a file it does
    not hold is left alone. Returns the tally and how many writes the
    condition refused."""
    tally = Tally()
    values = {":o": {"S": transaction}, ":f": {"BOOL": False}}

    def drop(entry):
        file_key, has_chain = entry
        body = {
            "TableName": TABLE,
            "Key": {"k": {"B": file_key}},
            "ExpressionAttributeValues": values,
            "ReturnConsumedCapacity": "INDEXES",
        }
        if has_chain:
            body["ConditionExpression"] = "owner_id = :o AND confirmed = :f"
            body["UpdateExpression"] = f"REMOVE {LOCK_ATTRIBUTES}"
            return "__type" in tally.call("UpdateItem", body)
        body["ConditionExpression"] = (
            "owner_id = :o AND confirmed = :f AND attribute_not_exists(revision)"
        )
        return "__type" in tally.call("DeleteItem", body)

    refused = sum(in_window(drop, granted))
    return tally.stop(), refused


def measure_status(count):
    print(f"\nStatus — the holder state of {count:,} held files, 100 to a BatchGetItem")
    create_table()
    put_items(
        [
            {
                "k": {"B": key(i)},
                **lock_attributes(i, "t0001"),
                **chain_attributes(i, "scope-status"),
            }
            for i in range(count)
        ]
    )
    keys = [key(i) for i in range(count)]
    for consistent, label in (
        (True, "strongly consistent"),
        (False, "eventually consistent"),
    ):
        held, tally = status(keys, consistent)
        check(f"{label}: every file reads as held", count, held)
        note(f"{label}", tally.summary("read units"))


def measure_force_release(count):
    print(f"\nForceRelease — {count:,} held files, with no holder check")
    create_table()
    with_chain = [key(i) for i in range(count)]
    lock_only = [key(i) for i in range(count, 2 * count)]
    lock_only_batched = [key(i) for i in range(2 * count, 3 * count)]
    put_items(
        [
            {
                "k": {"B": key(i)},
                **lock_attributes(i, "t0001"),
                **chain_attributes(i, "scope-release"),
            }
            for i in range(count)
        ]
        + [
            {"k": {"B": key(i)}, **lock_attributes(i, "t0001")}
            for i in range(count, 3 * count)
        ]
    )

    print("  files with chain state: UpdateItem REMOVE per file")
    tally = force_release(with_chain, has_chain=True)
    after = read_back(with_chain)
    check(
        "no file holds a lock", 0, sum(1 for item in after.values() if "holder" in item)
    )
    check(
        "every file keeps its chain state",
        count,
        sum(
            1
            for item in after.values()
            if {"revision", "content", "scope_pk"} <= item.keys()
        ),
    )
    note("cost", tally.summary("write units"))

    print("  files without chain state: DeleteItem per file")
    tally = force_release(lock_only, has_chain=False)
    check("every item is deleted", 0, len(read_back(lock_only)))
    note("cost", tally.summary("write units"))

    print("  files without chain state: BatchWriteItem, 25 per call")
    tally = delete_batched(lock_only_batched)
    check("every item is deleted", 0, len(read_back(lock_only_batched)))
    note("cost", tally.summary("write units"))

    check(
        "the held index is empty",
        0,
        await_index(
            "the held index is empty after ForceRelease",
            lambda: count_where({"IndexName": "held"}),
            0,
        ),
    )
    check(
        "the scope index keeps every chain",
        count,
        await_index(
            "the scope index keeps every chain",
            lambda: count_where({"IndexName": "scope"}),
            count,
        ),
    )


def measure_cancel(count):
    print(
        f"\ncancel — dropping {count:,} unconfirmed reservations by primary key, "
        "without a query"
    )

    print("  one conditional write per reservation")
    granted, overlap = cancel_fixture(count)
    check("t0001 was granted every reservation", count, len(granted))
    chained = [file_key for file_key, has_chain in granted if has_chain]
    tally, refused = cancel_by_key(granted, "t0001")
    check("no write was refused", 0, refused)
    check("t0001 holds no reservation", 0, reservations_of("t0001"))
    check("t0002's reservations survive", overlap, reservations_of("t0002"))
    check(
        "chain state under t0001's reservations is kept",
        len(chained),
        sum(1 for item in read_back(chained).values() if "revision" in item),
    )
    note("cost", tally.summary("write units"))

    print("  BatchWriteItem, 25 reservations per call")
    granted, overlap = cancel_fixture(count)
    check("t0001 was granted every reservation", count, len(granted))
    chained = [file_key for file_key, has_chain in granted if has_chain]
    tally = delete_batched([file_key for file_key, _ in granted])
    check("t0001 holds no reservation", 0, reservations_of("t0001"))
    check("t0002's reservations survive", overlap, reservations_of("t0002"))
    # The check expects the loss: a batch deletes whole items, so the chain
    # half goes with the reservation on every file that had one.
    check(
        "chain state under t0001's reservations is lost",
        0,
        sum(1 for item in read_back(chained).values() if "revision" in item),
    )
    note("latest entries deleted with the reservations", f"{len(chained):,}")
    note("cost", tally.summary("write units"))


def list_sharded_scope(scope, limit):
    """Lists `scope` through every shard of the scope index at once, each shard
    paged to its end at `limit` items a page, or at DynamoDB's 1 MB page where
    `limit` is None. Returns how many locks it listed."""
    tally = Tally()

    def list_shard(suffix):
        items, start_key = 0, None
        while True:
            body = {
                "TableName": TABLE,
                "IndexName": "scope",
                "KeyConditionExpression": "scope_pk = :s",
                "ExpressionAttributeValues": {":s": {"S": f"{scope}#{suffix}"}},
                "ReturnConsumedCapacity": "INDEXES",
            }
            if limit:
                body["Limit"] = limit
            if start_key:
                body["ExclusiveStartKey"] = start_key
            response = require_ok("Query", tally.call("Query", body))
            items += response.get("Count", 0)
            start_key = response.get("LastEvaluatedKey")
            if not start_key:
                return items

    items = sum(in_window(list_shard, [shard(s) for s in range(SHARDS)], window=SHARDS))
    return items, tally.stop()


def measure_page_size(count):
    scope = "scope-sharded"
    print(
        f"\nQuery page size — listing a scope holding {count:,} live locks over {SHARDS} shards"
    )
    create_table()
    seeded = time.time()
    throttled = put_items(
        [
            {
                "k": {"B": key(i)},
                **lock_attributes(i, "seed"),
                **chain_attributes(i, scope),
            }
            for i in range(count)
        ]
    )
    note("seeding took", f"{time.time() - seeded:.1f}s")
    note_throttling(*throttled)
    await_index(
        "the sharded scope index lists every lock",
        lambda: count_where({"IndexName": "scope"}),
        count,
    )
    for limit in (100, 1_000, 10_000, None):
        label = f"Limit {limit:,}" if limit else "no Limit, 1 MB pages"
        items, tally = list_sharded_scope(scope, limit)
        check(f"{label}: every lock is listed", count, items)
        note(label, tally.summary("read units"))


def throttle_reserve(index, now):
    """The acquire UpdateItem on file `index` of the throttle scope."""
    file_key = key(index, THROTTLE_SCOPE)
    return {
        "TableName": TABLE,
        "Key": {"k": {"B": file_key}},
        "ConditionExpression": "attribute_not_exists(holder) OR expires_at < :now",
        "UpdateExpression": (
            "SET holder=:h, owner_id=:o, confirmed=:c, expires_at=:e, "
            "held_pk=:hp, held_sk=:hs"
        ),
        "ExpressionAttributeValues": {
            ":now": {"N": str(now)},
            ":h": {"B": file_key},
            ":o": {"S": "throttle"},
            ":c": {"BOOL": False},
            ":e": {"N": str(now + 300)},
            ":hp": {"S": f"repo-x#{shard(index)}"},
            ":hs": {"S": f"branch-throttle#{index:08d}"},
        },
        "ReturnValues": "ALL_OLD",
        "ReturnConsumedCapacity": "INDEXES",
    }


def throttle_release(index):
    """The release of file `index` of the throttle scope, which has no chain
    state, so its item goes."""
    file_key = key(index, THROTTLE_SCOPE)
    return {
        "TableName": TABLE,
        "Key": {"k": {"B": file_key}},
        "ConditionExpression": "holder = :h AND attribute_not_exists(revision)",
        "ExpressionAttributeValues": {":h": {"B": file_key}},
        "ReturnConsumedCapacity": "INDEXES",
    }


def percentile(values, fraction):
    """The nearest-rank percentile of `values`, or None for no values."""
    if not values:
        return None
    ordered = sorted(values)
    return ordered[max(0, math.ceil(fraction * len(ordered)) - 1)]


def measure_throttle(rate, seconds):
    """Reserves and releases distinct files of one scope at `rate` operations a
    second for `seconds`, at most WINDOW operations in flight. An operation is
    dispatched at its scheduled time, or as soon as the window has room when
    it is full, so the rate achieved falls below `rate` when DynamoDB cannot
    keep up. A throttled call is retried with backoff, and every throttle is
    recorded with its time."""
    total = max(int(rate * seconds), 1)
    print(
        f"\nthrottle — reserve and release {total:,} distinct files of one scope "
        f"at {rate:,.0f}/s for {seconds:,.0f}s, {WINDOW} in flight"
    )
    create_table()
    lock = threading.Lock()
    window = threading.BoundedSemaphore(WINDOW)
    done = []
    latency = {"reserve": [], "release": []}
    throttled = []
    errors = {}
    capacity = {}
    started = time.perf_counter()

    def call(kind, target, body):
        for attempt in range(RETRIES):
            sent = time.perf_counter()
            response = ddb_once(target, body)
            took = time.perf_counter() - sent
            name = error_name(response)
            with lock:
                for part, units in capacity_parts(response).items():
                    capacity[part] = capacity.get(part, 0.0) + units
                if name in THROTTLING:
                    throttled.append((sent - started, name))
                elif name:
                    errors[f"{kind}: {name}"] = errors.get(f"{kind}: {name}", 0) + 1
                else:
                    latency[kind].append(took)
            if name not in THROTTLING:
                return not name
            time.sleep(backoff(attempt))
        return False

    def operation(index):
        try:
            now = int(time.time())
            if call("reserve", "UpdateItem", throttle_reserve(index, now)) and call(
                "release", "DeleteItem", throttle_release(index)
            ):
                with lock:
                    done.append(time.perf_counter() - started)
        finally:
            window.release()

    pool = concurrent.futures.ThreadPoolExecutor(max_workers=WINDOW)
    futures = []
    try:
        for index in range(total):
            delay = started + index / rate - time.perf_counter()
            if delay > 0:
                time.sleep(delay)
            window.acquire()
            futures.append(pool.submit(operation, index))
    finally:
        pool.shutdown(wait=True, cancel_futures=True)
    elapsed = time.perf_counter() - started
    for future in futures:
        future.result()

    check("every file was reserved and released", total, len(done))
    check("no call failed other than by throttling", 0, sum(errors.values()))
    for failure, times in sorted(errors.items()):
        note(failure, f"{times:,}")
    note("achieved overall", f"{len(done) / elapsed:,.1f}/s over {elapsed:.1f}s")

    # A last interval under a second would report a rate from a handful of
    # operations, so it joins the one before it.
    intervals = max(1, math.ceil((elapsed - 1.0) / INTERVAL))
    completed_in = [0] * intervals
    throttled_in = [0] * intervals
    for offset in done:
        completed_in[min(int(offset // INTERVAL), intervals - 1)] += 1
    for offset, _ in throttled:
        throttled_in[min(int(offset // INTERVAL), intervals - 1)] += 1
    print(f"  per {INTERVAL:.0f}-second interval: operations a second, throttled calls")
    for i in range(intervals):
        start = i * INTERVAL
        end = elapsed if i == intervals - 1 else start + INTERVAL
        note(
            f"{start:.0f}–{end:.0f}s",
            f"{completed_in[i] / max(end - start, 1e-9):,.1f}/s, "
            f"{throttled_in[i]:,} throttled",
        )

    if throttled:
        by_name = {}
        for _, name in throttled:
            by_name[name] = by_name.get(name, 0) + 1
        note(
            "throttled calls",
            ", ".join(f"{times:,} {name}" for name, times in sorted(by_name.items())),
        )
        note(
            "first and last throttle",
            f"{min(offset for offset, _ in throttled):.1f}s, "
            f"{max(offset for offset, _ in throttled):.1f}s",
        )
    else:
        note("throttled calls", "none")

    for part in sorted(capacity):
        note(f"write units, {part}", f"{capacity[part]:,.1f}")
    if not set(capacity) - {"total"}:
        note("write units by table and index", "not reported by this endpoint")

    for kind in ("reserve", "release"):
        p50, p99 = percentile(latency[kind], 0.5), percentile(latency[kind], 0.99)
        note(
            f"{kind} latency, p50 and p99",
            "no call succeeded"
            if p50 is None
            else f"{p50 * 1000:.1f} ms, {p99 * 1000:.1f} ms",
        )

    check("no file holds a lock afterwards", 0, count_where({}))
    check(
        "the held index is empty afterwards",
        0,
        await_index(
            "the held index is empty after throttle",
            lambda: count_where({"IndexName": "held"}),
            0,
        ),
    )


def run(options):
    """Runs the sections `options` names, every one when it names none, in
    order, and returns the exit code their checks give."""
    sections = options.sections or SECTIONS
    locks = options.locks
    workers = options.workers
    contended = options.contended_keys
    operation_keys = options.operation_keys

    if "query" in sections:
        print(f"Query — listing a scope holding {locks:,} live locks")
        create_table()
        seeded = time.time()
        throttled = seed_locks(locks)
        note("seeding took", f"{time.time() - seeded:.1f}s")
        note_throttling(*throttled)
        await_index(
            "the scope index lists every lock",
            lambda: count_where({"IndexName": "scope"}),
            locks,
        )

        pages, items, capacity = query_scope()
        check("every lock is listed", locks, items)
        note("pages the scope index returned", pages)
        note("read units for the whole scope", f"{capacity:.1f}")
        note("read units per 1,000 locks", f"{capacity / max(locks, 1) * 1000:.2f}")

        # A KEYS_ONLY index row is small, so a page carries a lot of them. That is
        # the property that makes this a routine call rather than a scan.
        note("locks per page", f"{items // max(pages, 1):,}")

        pages_100, _, _ = query_scope(page_limit=100)
        note("pages at a 100-item client limit", pages_100)

    if "reserve" in sections:
        print(
            f"\nreservation convergence — {workers} transactions wanting the same "
            f"{contended} files"
        )
        completed, worst, elapsed = race(
            workers, [key(i) for i in range(contended)], True
        )
        check("ascending order: every transaction completed", workers, completed)
        note("worst-case attempts by one transaction", worst)
        note("wall time", f"{elapsed:.1f}s")

        # Nothing about the reservation itself changes — only the order the keys are
        # taken in — so whatever separates the two runs is the ordering rule
        # doing its work.
        completed_r, worst_r, elapsed_r = race(
            workers, [key(i) for i in range(contended)], False
        )
        note("random order: transactions that completed", f"{completed_r} of {workers}")
        note("worst-case attempts by one transaction", worst_r)
        note("wall time", f"{elapsed_r:.1f}s")
        check(
            "both converge, ascending in fewer attempts",
            True,
            completed == workers and completed_r == workers and worst < worst_r,
        )

    if "status" in sections:
        measure_status(operation_keys)

    if "force-release" in sections:
        measure_force_release(operation_keys)

    if "cancel" in sections:
        for count in (10, operation_keys):
            measure_cancel(count)

    if "page-size" in sections:
        measure_page_size(locks)

    if "throttle" in sections:
        measure_throttle(options.throttle_rate, options.throttle_seconds)

    report_index_waits()
    print()
    if failures:
        print(f"{failures} check(s) failed")
        return 1
    print("all checks passed")
    return 0


def main(arguments):
    options = configure(arguments)
    print(f"successor-locks lock table at scale against {ENDPOINT}")
    if TARGET == "aws":
        print(f"aws target in {REGION}, tables {PREFIX}scale-locks-<n>")
    print(flush=True)
    require_dynamodb()

    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGHUP, on_signal)
    try:
        return run(options)
    except KeyboardInterrupt:
        stopped("interrupted")
        return 128 + signal.SIGINT
    except Interrupted as interruption:
        stopped(f"stopped by signal {interruption.signum}")
        return 128 + interruption.signum
    except TableTimeout as timeout:
        stopped(str(timeout))
        return 2
    except TableExists as exists:
        stopped(
            f"table {exists} exists already, and this run only uses tables it "
            "creates.\nPass --table-prefix with a prefix no table uses."
        )
        return 4
    finally:
        delete_created_tables()


def stopped(reason):
    """Reports why the run ended early, after what it printed so far."""
    sys.stdout.flush()
    print(f"\n{reason}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
