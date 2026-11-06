---
type: breaking
---

- `lore-transport`: A gRPC request that the server or the authentication service answers with
  `Unauthenticated` reports `NotAuthenticated` (16) to its caller rather than an internal error
  (-1, exit status 255). The requests and commands these notes name fail with it, or with
  `NotAuthorized` (17) for `PermissionDenied`. A Lore server with `[server.auth]` sends
  `Unauthenticated` for a request without a bearer token, and `PermissionDenied` for a token it
  can't verify, such as an expired or malformed one. One with `server.trust_unsigned_identity`
  sends `Unauthenticated` for an unsigned token that doesn't decode. `NotAuthenticated` is a
  refusal of the credentials, as `NotAuthorized` is: a connection whose token exchange is refused
  this way is not held for its back-off, so the next call connects again at once. Both refusals
  keep the server's reason, where it gives one, on the error's trace. Scripts and C API callers
  that test one of these for 255 or -1 should test for 16 and 17 as well.
- `lore-transport`: A storage query, fragment verification or mutable-store request that the server
  answers with `Unauthenticated` or `PermissionDenied` fails with `NotAuthenticated` (16) or
  `NotAuthorized` (17), where it failed as an internal error. A push whose check for the fragments
  the server holds is refused this way fails with that code, and so do the mutable-store requests
  of a C API storage handle that reaches only the remote.
- `lore-revision`: `lore repository verify fragment` fails with `NotAuthenticated` (16) or
  `NotAuthorized` (17) when the server refuses the verification, where it failed as an internal
  error. Its fragment event reports the failure as before.
- `lore-revision`: A branch lookup that reaches the remote, as one for a branch the instance does
  not hold or one under `--remote` does, fails with `NotAuthenticated` (16) or `NotAuthorized` (17)
  when the remote refuses the connect or the query. A refused connect failed as `BranchNotFound`
  (87), a refused query as an internal error, and a lookup by id without `--remote` answered the id
  with no revision. `lore file history --branch`, `lore branch reset --branch`,
  `lore file reset --last-merged-from` and `lore branch metadata get`, `set` and `clear` with
  `--branch` fail with the refusal too, where they reported `BranchNotFound` (87), or
  `InvalidArguments` (3) for the metadata commands. Any other failure is reported as before.
- `lore-revision`: A fragment upload the server refuses fails with `NotAuthenticated` (16) or
  `NotAuthorized` (17), where it failed as an internal error. That is the upload of `lore push`
  (`lore branch push`), `lore branch merge into` and `lore revision restore`, and of a metadata
  blob by `lore branch metadata set` and `clear` and `lore repository metadata set` and `clear`.
- `lore-revision`: A read through a remote mutable store, as a server whose mutable store is a
  remote store makes, fails with `NotAuthenticated` (16) or `NotAuthorized` (17) when that store
  refuses it, and with the code of any other failure but a missing key, where it failed as an
  internal error. Its writes already failed this way.
- `lore-notification`: A subscription the server refuses as `Unauthenticated` or `PermissionDenied`
  fails at once with `NotAuthenticated` (16) or `NotAuthorized` (17). An `Unauthenticated` refusal
  failed as an internal error, and a `PermissionDenied` one was retried before it did.
- `lore-revision`: A notification subscription whose connect to the remote is refused fails with
  `NotAuthenticated` (16) or `NotAuthorized` (17), where it failed as an internal error.
- `lore-server`: A server whose immutable store is a remote store logs a request that store refuses
  as `Unauthenticated` as `NotAuthenticated` rather than as an internal error. Through a composite
  store, a fragment read the remote store refuses this way fails as a store failure, where the
  composite went on to its other stores, and its outcome metric counts the refusal as
  `not_authenticated`.
