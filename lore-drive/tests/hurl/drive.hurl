# lore-drive integration suite (hurl).
#
# Run via tests/hurl/run.sh — it creates a FRESH scratch workspace, starts
# lore-drive in it, and executes this file with the right --variable/--file-root.
# The asserts assume a virgin repository (node ids are deterministic from
# an empty tree).

# ── Read API sanity ───────────────────────────────────────────────────────────
GET {{base}}/api/v1/info
HTTP 200
[Asserts]
jsonpath "$.mode" == "drive"
jsonpath "$.repository_id" isString
jsonpath "$.workdir" isString

GET {{base}}/api/v1/tree
HTTP 200
[Asserts]
jsonpath "$.node_id" == 0
jsonpath "$.children" count == 0

# ── Nested upload ─────────────────────────────────────────────────────────────
# hurl sends the relative path as the multipart filename → lands nested.
POST {{base}}/api/v1/upload?parent_id=0
[MultipartFormData]
file: file,folder1/file1;
HTTP 201
[Captures]
file1_node: jsonpath "$.files[0].node_id"
file1_addr: jsonpath "$.files[0].address"
[Asserts]
jsonpath "$.files[0].path" == "/folder1/file1"
jsonpath "$.files[0].size" == 12

GET {{base}}/api/v1/tree
HTTP 200
[Asserts]
jsonpath "$.children" count == 1
jsonpath "$.children[0].name" == "folder1"
jsonpath "$.children[0].kind" == "directory"
[Captures]
folder1_node: jsonpath "$.children[0].node_id"

# ── Ghost-conflict regression (the fixed bug) ─────────────────────────────────
# upload nested folder1/file1 → delete folder1/ → re-upload the SAME path.
# Before the fix, resolve_path resolved the staged-delete tombstone and the
# conflict check answered 409 for a path gone from listings and disk.
DELETE {{base}}/api/v1/node/{{folder1_node}}
HTTP 200

GET {{base}}/api/v1/tree
HTTP 200
[Asserts]
jsonpath "$.children" count == 0

POST {{base}}/api/v1/upload?parent_id=0
[MultipartFormData]
file: file,folder1/file1;
HTTP 201
[Asserts]
jsonpath "$.files[0].path" == "/folder1/file1"

GET {{base}}/api/v1/tree
HTTP 200
[Asserts]
jsonpath "$.children" count == 1
jsonpath "$.children[0].name" == "folder1"

# ── Live conflicts still fire ─────────────────────────────────────────────────
POST {{base}}/api/v1/upload?parent_id=0
[MultipartFormData]
file: file,folder1/file1;
HTTP 409
[Asserts]
jsonpath "$.conflicts" count == 1
jsonpath "$.conflicts[0]" == "/folder1/file1"

# ── Properties: set / list / persist across rename ────────────────────────────
# (re-capture folder1 + file1 node ids after resurrection)
GET {{base}}/api/v1/tree
HTTP 200
[Captures]
folder1b: jsonpath "$.children[?(@.name=='folder1')].node_id" nth 0

GET {{base}}/api/v1/tree?node_id={{folder1b}}
HTTP 200
[Captures]
file1b: jsonpath "$.children[?(@.name=='file1')].node_id" nth 0

GET {{base}}/api/v1/node/{{file1b}}/properties
HTTP 200
[Asserts]
jsonpath "$.properties.*" count == 0

PUT {{base}}/api/v1/node/{{file1b}}/properties
{"key": "owner", "value": "nsauzede"}
HTTP 200
[Asserts]
jsonpath "$.properties.owner" == "nsauzede"

PUT {{base}}/api/v1/node/{{file1b}}/properties
{"key": "status", "value": "draft"}
HTTP 200
[Asserts]
jsonpath "$.properties.*" count == 2

# properties on a FOLDER
PUT {{base}}/api/v1/node/{{folder1b}}/properties
{"key": "team", "value": "platform"}
HTTP 200
[Asserts]
jsonpath "$.properties.team" == "platform"

# overwrite an existing key
PUT {{base}}/api/v1/node/{{file1b}}/properties
{"key": "status", "value": "final"}
HTTP 200
[Asserts]
jsonpath "$.properties.status" == "final"
jsonpath "$.properties.*" count == 2

# validation: empty key
PUT {{base}}/api/v1/node/{{file1b}}/properties
{"key": "", "value": "x"}
HTTP 400

# unknown node
GET {{base}}/api/v1/node/9999/properties
HTTP 404

# rename file1 → renamed1: node id and properties must survive
PATCH {{base}}/api/v1/node/{{file1b}}
{"name": "renamed1"}
HTTP 200

GET {{base}}/api/v1/node/{{file1b}}/properties
HTTP 200
[Asserts]
jsonpath "$.properties.owner" == "nsauzede"
jsonpath "$.properties.status" == "final"

# ── Search: names, property keys, property values ─────────────────────────────
GET {{base}}/api/v1/search?q=renamed
HTTP 200
[Asserts]
jsonpath "$.results" count == 1
jsonpath "$.results[0].name" == "renamed1"
jsonpath "$.results[0].path" == "/folder1/renamed1"
jsonpath "$.results[0].matches[0].field" == "name"

GET {{base}}/api/v1/search?q=owner
HTTP 200
[Asserts]
jsonpath "$.results" count == 1
jsonpath "$.results[0].matches[0].field" == "property"
jsonpath "$.results[0].matches[0].key" == "owner"
jsonpath "$.results[0].matches[0].value" == "nsauzede"

# case-insensitive, value match, folder hit
GET {{base}}/api/v1/search?q=PLATFORM
HTTP 200
[Asserts]
jsonpath "$.results" count == 1
jsonpath "$.results[0].kind" == "directory"
jsonpath "$.results[0].name" == "folder1"

# no hit
GET {{base}}/api/v1/search?q=zzzznothing
HTTP 200
[Asserts]
jsonpath "$.results" count == 0

# empty query rejected
GET {{base}}/api/v1/search?q=
HTTP 400

# ── Property removal ──────────────────────────────────────────────────────────
DELETE {{base}}/api/v1/node/{{file1b}}/properties/status
HTTP 200
[Asserts]
jsonpath "$.properties.*" count == 1

DELETE {{base}}/api/v1/node/{{file1b}}/properties/status
HTTP 404

# ── Delete cleans the subtree's properties ────────────────────────────────────
DELETE {{base}}/api/v1/node/{{folder1b}}
HTTP 200

GET {{base}}/api/v1/search?q=nsauzede
HTTP 200
[Asserts]
jsonpath "$.results" count == 0

GET {{base}}/api/v1/search?q=platform
HTTP 200
[Asserts]
jsonpath "$.results" count == 0

# resurrect the same path: the reused node ids must carry NO stale properties
POST {{base}}/api/v1/upload?parent_id=0
[MultipartFormData]
file: file,folder1/file1;
HTTP 201
[Captures]
file1c: jsonpath "$.files[0].node_id"

GET {{base}}/api/v1/node/{{file1c}}/properties
HTTP 200
[Asserts]
jsonpath "$.properties.*" count == 0
