# Phase 2 — replace-upload metadata-freshness regression (the fixed
# stale-size bug). Run AFTER drive.hurl with --file-root tests/hurl/fixtures2,
# whose folder1/file1 carries DIFFERENT, LONGER content (45 B vs 12 B).
#
# drive.hurl leaves /folder1/file1 in place (12 B). Overwriting it must
# refresh the served size AND the address, not just the bytes.

POST {{base}}/api/v1/upload?parent_id=0&overwrite=true
[MultipartFormData]
file: file,folder1/file1;
HTTP 201
[Captures]
new_addr: jsonpath "$.files[0].address"
file1_node: jsonpath "$.files[0].node_id"
[Asserts]
jsonpath "$.files[0].size" == 45

# The tree must agree — this is exactly what stayed stale before the
# stage.rs fix (already-staged nodes skipped size/mode refresh).
GET {{base}}/api/v1/node/{{file1_node}}
HTTP 200
[Asserts]
jsonpath "$.size" == 45
jsonpath "$.address" == "{{new_addr}}"

# Byte-identical re-upload stays a success no-op.
POST {{base}}/api/v1/upload?parent_id=0&overwrite=true
[MultipartFormData]
file: file,folder1/file1;
HTTP 201
[Asserts]
jsonpath "$.files[0].size" == 45
jsonpath "$.files[0].address" == "{{new_addr}}"
