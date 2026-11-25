---
type: breaking
---

- `lore-server` (AWS store): a store whose region is not set by its `s3_region` or `dynamodb_region` setting,
  `AWS_REGION`, `AWS_DEFAULT_REGION` or, on EC2, the instance metadata service refuses to start. The region is no
  longer read from `~/.aws/config`
- `lore-server` (AWS store): credentials resolve from, in order, the environment, container credentials (ECS and EKS Pod
  Identity), web identity (EKS IRSA), the EC2 instance metadata service, and static keys in `~/.aws/credentials` or
  `AWS_SHARED_CREDENTIALS_FILE`. `~/.aws/config` is not read, so a profile that takes its credentials from `role_arn`
  and `source_profile`, SSO or `credential_process` no longer resolves; export its credentials into the environment
  instead, as `aws configure export-credentials --format env` prints them. Static keys from the credentials file come
  after a web identity, container or instance identity, where they came before them
- `lore-server` (AWS store): the AWS SDK's configuration variables `AWS_CONFIG_FILE`, `AWS_USE_FIPS_ENDPOINT`,
  `AWS_USE_DUALSTACK_ENDPOINT`, `AWS_MAX_ATTEMPTS`, `AWS_RETRY_MODE`, `AWS_REQUEST_CHECKSUM_CALCULATION`,
  `AWS_RESPONSE_CHECKSUM_VALIDATION`, `AWS_ACCOUNT_ID_ENDPOINT_MODE` and `AWS_ACCOUNT_ID` have no effect. A deployment
  that reaches FIPS endpoints through `AWS_USE_FIPS_ENDPOINT` names them in `s3_endpoint_url` and
  `dynamodb_endpoint_url` instead
- `lore-server` (AWS store): DynamoDB requests go to the regional endpoint, `dynamodb.<region>.amazonaws.com`, where
  credentials carrying an account ID — web identity, container credentials, or keys with `AWS_ACCOUNT_ID` or
  `aws_account_id` set — reached the account-based `<account>.ddb.<region>.amazonaws.com`. STS requests follow
  `HTTPS_PROXY`, the other proxy variables and the system's proxy settings, where they went direct. Network rules and
  proxies that name hostnames have to allow these. Every request names `lore/<version>` as its user agent, where it
  named the AWS SDK
- `lore-server` (AWS store): over plain HTTP, `AWS_CONTAINER_CREDENTIALS_FULL_URI` must name a loopback address,
  `localhost` or the ECS or EKS Pod Identity agent's address, where any host name that resolved to loopback was
  accepted
- `lore-server` (AWS store): the operation metrics label a failure no service answered with the store's own `aws_code`
  values: `Timeout` in place of `SdkTimeoutError` and `DispatchFailure_Timeout`; `TransportIo`, `TransportBody` or
  `TransportProtocol` in place of `DispatchFailure_IO`, `DispatchFailure_Other` and `DispatchFailure`; and, among the
  failures the SDK labelled `<unknown>`, `TransportBuild` for a request that could not be built and `Credentials` or
  `CredentialsTransient` for one that could not be signed. A response that could not be parsed, `<unknown>` before, is
  `TransportProtocol`. Service error codes label as before, except that spellings meaning the same thing fold into
  one name — S3's `TooManyRequests`, `RequestThrottled` and `ThrottlingException` into `SlowDown`,
  `AllAccessDisabled` into `AccessDenied`, `RequestTimeoutException` into `RequestTimeout` and `TokenRefreshRequired`
  into `ExpiredToken`; DynamoDB's `ThrottledException`, `Throttling` and `TooManyRequests` into `ThrottlingException`,
  `InternalFailure` into `InternalServerError`, `ServiceUnavailableException` into `ServiceUnavailable`,
  `AccessDenied` into `AccessDeniedException`, `SignatureDoesNotMatch` into `InvalidSignatureException`,
  `RequestExpired` into `RequestTimeTooSkewed` and `ExpiredToken` into `ExpiredTokenException` — and an answer with no
  body, `<unknown>` before, is named from its status: S3's as `Forbidden`, `ServiceUnavailable`, `RequestTimeout`,
  `SlowDown` or `InternalError`, DynamoDB's 5xx as `InternalServerError` and 429 as `ThrottlingException`, and any
  other as its status number
- `lore-server`: the lock service refuses a lock, unlock, admin lock or lock status request naming more resources than
  the lock store takes at once, 100 for the `local` and `aws` lock stores, with `OUT_OF_RANGE`, and one naming a
  resource twice with `INVALID_ARGUMENT`. A server forwarding the request to a peer checks it against its own lock
  store first, and the peer checks it again against its own. A lock, unlock or admin lock request of any size went to
  the store, which could apply part of it, and a status request over 100 resources was refused with `INVALID_ARGUMENT`
- `lore-aws`: `AwsMutableStoreSettings` loses `force_write`, which nothing read, and `AwsMutableStoreSettings::new`
  takes only the DynamoDB settings. `lore-server`: `AwsMutableStorePluginConfig` loses it too; a
  `[plugins.aws.mutable_store]` table that still sets `force_write` loads as before
