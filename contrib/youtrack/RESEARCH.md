<!-- SPDX-FileCopyrightText: 2026 RedRatInHat -->
<!-- SPDX-License-Identifier: MIT -->

# YouTrack integration research

This report records the source review behind the YouTrack integration design.
Research date: October 31, 2026. Lore baseline:
`b8fc623d5b064e910012957c58dc6d3597c45b41`.

The implementation choices below are design decisions. They do not certify a
deployed connection to a live YouTrack instance. Tests and their results belong
in the integration's implementation and validation report.

## What existing integrations do

An external issue link, a version control webhook, and a YouTrack REST client
are different components. In Gitea and GitLab, the YouTrack-specific issue
tracker code mostly renders links. YouTrack's own connector receives version
control events and reads repository data. Its server implementation is not
part of either open-source repository.

| Project and inspected revision | Interaction with YouTrack | Useful implementation practice | Limit of the evidence |
| --- | --- | --- | --- |
| Gitea `a4d17117775e8a1a90c48faad607d6b1793b71ba`, October 31, 2026, `main`, MIT | External tracker configuration renders issue links. General webhook machinery sends repository events. YouTrack provides the receiving connector. | Persist webhook payloads, filter branches, bound HTTP work, retain delivery diagnostics, and redact credentials. | The inspected delivery path marks a task delivered before HTTP. Its persisted queue is not an exactly-once guarantee. |
| GitLab FOSS `d7b9a6176ab4e279465ba3ee09922cde2b74de47`, October 23, 2026, `19.6.0-pre`, MIT | The YouTrack integration supplies URL templates and issue matching. General webhooks feed YouTrack's connector. | Separate link rendering from event delivery. Track a request identity, apply timeouts and response limits, and redact custom headers. | A webhook idempotency key does not establish that another receiver deduplicates it. Worker retry settings do not prove retries for every HTTP error. |
| JetBrains YouTrack Apps `4133cefb2e0d15d69dc803cecc3eab1c68a40554`, September 30, 2026, `1.1.8`, Apache-2.0 | Official CLI calls YouTrack REST with a permanent Bearer token. | Central request builder, environment configuration, origin checks, explicit pagination, and handling of empty responses. | This REST client is not a persistent version control delivery service. |
| Jenkins YouTrack plugin `7844f96bb89e7584f9bb1c63473c79c850310adb`, May 2, 2019, MIT | Reads build changelogs and executes issue commands through the legacy YouTrack REST API. | Separate links, comments, and commands. Persist processed revision IDs across restarts. | Its API and authentication are old. Its processed-ID file has no atomic protocol for an uncertain HTTP outcome. |
| ELCA YouTrack API client `d29b8c20eb09df91be52e122316220f5d90dbcea`, October 22, 2026, `1.9.0`, MPL-2.0 | Go client calls YouTrack REST with a Bearer token. | Typed HTTP errors, `Retry-After` parsing, a configurable timeout, and bounded pagination. | A transient-error classifier identifies a potentially recoverable failure, not an operation that is safe to repeat. |

These projects were studied for behavior and architecture. Their source code is
not vendored or copied into the integration. The design uses Python's standard
library and Lore's existing command-line interface. License labels describe the
inspected source trees, not a claim that all third-party dependencies have the
same license.

### Gitea and GitLab source findings

Gitea's `ExternalTrackerConfig` contains a URL, URL format, style, and regular
expression. `PrepareWebhook` persists a payload as a `HookTask` and schedules its
ID. `Deliver` applies HTTP handling and records a result. Its general webhook
signature is HMAC-SHA256; that signature format must not be substituted for
YouTrack Generic VCS authentication.

See the inspected Gitea sources:

- [External tracker configuration][gitea-config].
- [Webhook preparation][gitea-prepare].
- [Webhook delivery][gitea-deliver].
- [Persisted hook tasks][gitea-task].

GitLab's `Integrations::Base::Youtrack` defines link fields and the issue matcher.
The shared issue-tracker base reports `support_close_issue` as false and uses a
HEAD request when testing the configured project URL. It does not implement
YouTrack's issue-command REST protocol. General webhook delivery is a separate
service and worker.

See the inspected GitLab sources:

- [YouTrack integration model][gitlab-model].
- [Shared issue tracker model][gitlab-base].
- [Webhook service][gitlab-service].
- [Webhook worker][gitlab-worker].

GitLab also documents incomplete event payloads: a push webhook contains at
most the newest 20 commits, branch creation can contain no commits, and pushes
that exceed the configured branch limit can omit hooks. This supports reading
authoritative history when events are used as a notification mechanism. See
[Webhook events in the GitLab documentation][gitlab-events].

The official configuration guides describe both sides of these integrations:
[Gitea integration in YouTrack][youtrack-gitea],
[GitLab integration in YouTrack][youtrack-gitlab], and
[YouTrack integration in GitLab][gitlab-docs].

### REST client and build integration findings

The JetBrains CLI centralizes `Accept`, Bearer authorization, and `User-Agent`
in `generateRequestParams`. `restRequest` prevents a supplied path from leaving
the configured origin and base path. It rejects an overriding Authorization
header. `textRequest` checks HTTP success and accepts empty or `204` responses.
`listRequest` sends `$skip` and `$top` explicitly. These are reusable interface
principles, even though Generic VCS uses a different credential.

See the JetBrains CLI's [request builder][apps-request],
[raw REST command][apps-rest], [API client][apps-client], and
[environment configuration][apps-config].

The Jenkins plugin distinguishes link annotations, comments, and commands. Its
`YoutrackProcessedRevisionsSaver` reloads processed IDs from disk. The caller
checks this set before executing commands, then records processed revisions
afterward. This is evidence for keeping persistent delivery state, but not a
template for exactly-once execution: the remote operation and local save are
separate, and the key does not include the destination repository.

See the Jenkins plugin's [revision processing][jenkins-updater],
[processed-ID storage][jenkins-processed], and [legacy API calls][jenkins-api].

The ELCA client distinguishes authentication failures, conflicts, rate limits,
server failures, and transport errors. It parses `Retry-After` as seconds or an
HTTP date. Its pagination has a termination bound. Its `IsRetryable` result
must not be interpreted as proof that a preceding POST had no effect.

See the ELCA client's [HTTP implementation][elca-client],
[pagination][elca-pagination], and [error tests][elca-errors].

## YouTrack Generic VCS contract

The target is the documented Generic VCS receiver in YouTrack 2026.2. The
integration administrator creates a Generic VCS connection and supplies its
generated Resource URL and Auth Header value to the sender. The repository URL
template uses `$hash$` to link to a revision viewer.

```http
POST /api/vcsHooksReceiver/generic/<integration-id>
Accept: application/json
Content-Type: application/json
X-YT-Simple-Signature: <configured-shared-secret>
```

```json
{
  "branch": "refs/heads/main",
  "commits": [
    {
      "version": "immutable-revision-id",
      "date": 1730130046519,
      "user": {
        "userName": "developer",
        "email": "developer@example.org"
      },
      "comment": "GAME-123 Fix dialogue loading"
    }
  ]
}
```

The signature header carries the configured secret unchanged. It is not an
HMAC of the request. The Cloud documentation explicitly describes exact
matching and rejection of missing, incorrect, or empty secrets. The Server
example also includes Basic authorization but does not explain it as a second
required credential for the receiver. The design therefore separates the
receiver secret from any optional REST token used for read-only reconciliation.

The example timestamp is Unix time in milliseconds. The documented branch
filter syntax is `refs/heads/<name>`, although the example request has an
inconsistent `/refs/head/main` value. The design uses one explicit branch
normalization rather than inferring it from that example.

The receiver's settings determine which projects, branches, and committers are
processed. They also determine whether messages only attach revisions or apply
issue commands. The proposed initial configuration is **Add commits, ignore
commands**. Command execution can be enabled separately by the YouTrack
administrator after author mappings and access are verified.

See [Generic VCS Integration in YouTrack Server][generic-server] and
[Integrate with a Generic VCS in YouTrack Cloud][generic-cloud].

### Delivery semantics that the public contract does not guarantee

The reviewed documentation provides a request example, not a complete protocol
schema. It does not specify batch limits, an idempotency key, duplicate-command
suppression, batch atomicity, or a per-revision acknowledgement response.

A timeout or server error can occur after a request has reached YouTrack.
Repeating it might execute a message command again. The design therefore
persists an in-flight attempt and retains an uncertain outcome as `UNKNOWN`.
Automatic delivery must not resend it. A successful HTTP response records
transport acceptance, not proof that a particular issue was linked.

Optional reconciliation can read
`GET /api/issues/{issueID}/vcsChanges` and compare revision identity and the
configured integration. This requires a separate REST token with read access
to the issue and its version control changes. All pages must be examined.
Most REST collections otherwise return only a limited first page.

A matching association is positive evidence that a revision is attached.
Its absence does not prove that a command had no effect. Even a matching
association does not prove that every command in a message was executed.

See [Issue VCS changes in the YouTrack REST API][vcs-rest] and
[REST pagination][pagination].

## Lore integration points

The inspected Lore baseline provides several relevant extension points:

| Source location | Existing behavior | Design implication |
| --- | --- | --- |
| `lore-server/src/hooks/dispatch.rs`, `HookDispatcher::spawn_post` | Spawns independent tasks, returns immediately, applies a timeout, and logs errors. | Useful for notification, but the dispatcher alone does not provide durable delivery. |
| `lore-server/src/hooks/context.rs`, `HookContext` | Contains the operation user, repository, branch, and target revision. | The operation user is not necessarily the revision author. Revision metadata must be read separately. |
| `lore-server/src/grpc/revision/v1/branch_push.rs` and `grpc/handlers/branch_push.rs` | Construct hook context before pushing. After a server merge, the notification uses the resulting revision, while the post-hook context only updates its revision number. | Read the remote branch's actual latest revision rather than treating the input hook revision as the resulting branch state. |
| `lore-server/src/hooks/mod.rs` and `server.rs` | Register compiled hooks and callbacks supplied by an embedding server. The inspected stock registration function contains no discovered hooks. | A configuration-only webhook URL does not add a new hook implementation. |
| `lore-client/src/cli/commands/branch.rs` and `revision.rs` | Existing branch and revision commands expose the source data. | A companion process can use supported remote reads without changing the server protocol. |
| `lore-revision/src/revision/info.rs`, `LoreRevisionInfoEventData` | Exposes both parent hashes for a revision. | Traverse both parents to include merged history. |
| `lore-revision/src/revision/history.rs` | The history walk advances through the direct parent. | A simple linear history read is insufficient to discover all merge ancestry. |

These statements refer to
[Lore at the inspected revision][lore-baseline]. They explain the chosen
boundary; they do not require changing the hook implementation for this
integration.

## Selected design

The proposed integration lives in `contrib/youtrack` as a separate Python
standard-library process. It reads Lore's structured command output with
explicit remote access and sends revision metadata to Generic VCS. The default
monitored branch is `main`.

The design has these properties:

1. Read the remote branch's current latest revision, then walk both parents in
   the revision graph. Record immutable metadata and all discovered work in a
   SQLite outbox before delivery.
2. Use the original revision author, message, and timestamp. Map author
   identities explicitly. Do not replace them with the service account that
   reads Lore or pushes a branch.
3. Deduplicate globally within the configured repository and receiver by
   revision ID. A revision reached from a second monitored branch must not
   trigger its commands a second time. This chooses delivery once per revision
   over reporting every branch association.
4. Persist delivery attempts. Separate queued work, an in-flight attempt,
   accepted transport, a rejected request, and an unknown outcome. Preserve
   unknown outcomes across restarts and require reconciliation before later
   deliveries continue. Unknown attempts have no automatic or manual replay API.
5. Keep YouTrack unavailability outside Lore's commit and push path. Polling
   allows reachable missed history to be discovered after a restart.
6. Keep the receiver, secrets, author mappings, and state in operator-controlled
   configuration. Repository content must not be able to redirect a credential
   to another endpoint.
7. Use bounded HTTP calls, validated endpoints, no redirects for secret-bearing
   requests, and logs that exclude secrets and full revision messages.
8. Offer optional read-only REST reconciliation. Treat a matched revision
   association as evidence of attachment, not proof of command execution.

The retained history requirement is deliberate. A branch that is created,
pushed, and deleted between polls can become undiscoverable. Permanent
obliteration can also remove history before discovery. This design does not
claim a complete server audit stream. Monitor a retained integration branch
and preserve history long enough for the sender to read it.

The requested work targets an organizational fork. No upstream issue, pull
request, or maintainer outreach is part of this implementation. A later
upstream submission would need to follow Lore's contribution process.

[gitea-config]: https://github.com/go-gitea/gitea/blob/a4d17117775e8a1a90c48faad607d6b1793b71ba/models/repo/repo_unit.go
[gitea-prepare]: https://github.com/go-gitea/gitea/blob/a4d17117775e8a1a90c48faad607d6b1793b71ba/services/webhook/webhook.go
[gitea-deliver]: https://github.com/go-gitea/gitea/blob/a4d17117775e8a1a90c48faad607d6b1793b71ba/services/webhook/deliver.go
[gitea-task]: https://github.com/go-gitea/gitea/blob/a4d17117775e8a1a90c48faad607d6b1793b71ba/models/webhook/hooktask.go
[gitlab-model]: https://github.com/gitlabhq/gitlabhq/blob/d7b9a6176ab4e279465ba3ee09922cde2b74de47/app/models/concerns/integrations/base/youtrack.rb
[gitlab-base]: https://github.com/gitlabhq/gitlabhq/blob/d7b9a6176ab4e279465ba3ee09922cde2b74de47/app/models/concerns/integrations/base/issue_tracker.rb
[gitlab-service]: https://github.com/gitlabhq/gitlabhq/blob/d7b9a6176ab4e279465ba3ee09922cde2b74de47/app/services/web_hook_service.rb
[gitlab-worker]: https://github.com/gitlabhq/gitlabhq/blob/d7b9a6176ab4e279465ba3ee09922cde2b74de47/app/workers/web_hook_worker.rb
[gitlab-events]: https://docs.gitlab.com/user/project/integrations/webhook_events/
[youtrack-gitea]: https://www.jetbrains.com/help/youtrack/server/integration-with-gitea.html
[youtrack-gitlab]: https://www.jetbrains.com/help/youtrack/server/integration-with-gitlab.html
[gitlab-docs]: https://docs.gitlab.com/user/project/integrations/youtrack/
[apps-request]: https://github.com/JetBrains/youtrack-apps/blob/4133cefb2e0d15d69dc803cecc3eab1c68a40554/packages/apps-tools/lib/net/request.ts
[apps-rest]: https://github.com/JetBrains/youtrack-apps/blob/4133cefb2e0d15d69dc803cecc3eab1c68a40554/packages/apps-tools/src/cli/commands/rest.ts
[apps-client]: https://github.com/JetBrains/youtrack-apps/blob/4133cefb2e0d15d69dc803cecc3eab1c68a40554/packages/apps-tools/src/cli/youtrack/youtrack-apps-client.ts
[apps-config]: https://github.com/JetBrains/youtrack-apps/blob/4133cefb2e0d15d69dc803cecc3eab1c68a40554/packages/apps-tools/src/cli/index.ts
[jenkins-updater]: https://github.com/jenkinsci/youtrack-plugin/blob/7844f96bb89e7584f9bb1c63473c79c850310adb/src/main/java/org/jenkinsci/plugins/youtrack/YoutrackIssueUpdater.java
[jenkins-processed]: https://github.com/jenkinsci/youtrack-plugin/blob/7844f96bb89e7584f9bb1c63473c79c850310adb/src/main/java/org/jenkinsci/plugins/youtrack/YoutrackProcessedRevisionsSaver.java
[jenkins-api]: https://github.com/jenkinsci/youtrack-plugin/blob/7844f96bb89e7584f9bb1c63473c79c850310adb/src/main/java/org/jenkinsci/plugins/youtrack/youtrackapi/YouTrackServer.java
[elca-client]: https://github.com/elcait/youtrack-api-client/blob/d29b8c20eb09df91be52e122316220f5d90dbcea/client/client.go
[elca-pagination]: https://github.com/elcait/youtrack-api-client/blob/d29b8c20eb09df91be52e122316220f5d90dbcea/client/pagination.go
[elca-errors]: https://github.com/elcait/youtrack-api-client/blob/d29b8c20eb09df91be52e122316220f5d90dbcea/client/errors_test.go
[generic-server]: https://www.jetbrains.com/help/youtrack/server/generic-vcs-integration.html
[generic-cloud]: https://www.jetbrains.com/help/youtrack/cloud/integrate-project-with-generic-vcs.html
[vcs-rest]: https://www.jetbrains.com/help/youtrack/devportal/resource-api-issues-issueID-vcsChanges.html
[pagination]: https://www.jetbrains.com/help/youtrack/devportal/api-concept-pagination.html
[lore-baseline]: https://github.com/EpicGames/lore/tree/b8fc623d5b064e910012957c58dc6d3597c45b41
