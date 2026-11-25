---
type: improvement
---

- `lore-aws`: the AWS stores talk to S3 and DynamoDB through their own client, on the HTTP stack the rest of Lore uses,
  and no longer depend on the AWS SDK (`aws-config`, `aws-sdk-s3`, `aws-sdk-dynamodb` and the `aws-smithy-*` crates).
  The `timeout_millis` budget now also bounds reading an S3 object's body, where the SDK stopped only one that stalled.
  `pool_idle_timeout_seconds` now takes effect: idle connections close after it, 30 seconds by default, where they
  stayed open for 90 seconds. S3 checks a CRC32 of every object written, as it did with the SDK, and refuses a damaged
  one, which the store sends again. A read is no longer checked against the checksum S3 stored; Lore clients check each
  payload they fetch against its hash, unless told to trust their remote with `--trust-remote`
