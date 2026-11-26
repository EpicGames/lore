// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! End-to-end tests for the S3 operations, against a scripted local service.
//!
//! The S3 half of what `dispatch.rs` does for `DynamoDB`. What only shows up here is composition:
//! that an object's metadata is signed along with the request that carries it, that a body arrives
//! streamed and intact, that a `SlowDown` is retried while a 404 is not, and that a version listing
//! follows its markers to the end.
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use lore_aws::aws::ClientCore;
use lore_aws::aws::creds::CachedCredentials;
use lore_aws::aws::creds::Credentials;
use lore_aws::aws::creds::StaticProvider;
use lore_aws::aws::error::AwsCallError;
use lore_aws::aws::error::S3ErrorCode;
use lore_aws::aws::error::TransportError;
use lore_aws::aws::http::HttpClient;
use lore_aws::aws::item::ByteStr;
use lore_aws::aws::s3::ObjectMetadata;
use lore_aws::aws::s3::S3Client;
use lore_aws::aws::sign::payload_sha256;
use lore_aws::clients::HttpClientSettings;
use lore_base::runtime::LORE_CONTEXT;

use crate::aws::test_server::StubResponse;
use crate::aws::test_server::StubServer;

const BUCKET: &str = "lore-store";
/// A key shaped like the ones this store uses: a content hash in hex.
const KEY: &str = "3f2a1b";

fn client_for(server: &StubServer, timeout: Duration) -> S3Client {
    let http = HttpClient::new(&HttpClientSettings {
        pool_idle_timeout_seconds: 5,
        nodelay: true,
        max_connections: None,
        connection_acquire_timeout_millis: None,
    })
    .expect("a client builds");

    S3Client::new(
        Arc::new(ClientCore::new(
            http,
            Arc::new(CachedCredentials::new(Arc::new(StaticProvider::new(
                Credentials::new("AKIDTEST", "secret-for-tests", None, None),
            )))),
            Arc::from("eu-west-1"),
            server.endpoint(),
            timeout,
            false,
        )),
        // The stub listens on a loopback address, which cannot host a bucket name; the client works
        // that out for itself, and this asserts the setting is not what makes it.
        false,
    )
}

fn fragment_metadata() -> ObjectMetadata {
    ObjectMetadata::from([(
        ByteStr::from_static("lore-fragment"),
        ByteStr::from_static("8:3:9"),
    )])
}

/// The whole shape of a signed `PutObject`: where it goes, what it carries, and that the metadata
/// describing the bytes is signed along with them.
#[tokio::test]
async fn a_put_object_signs_its_metadata_and_its_body() {
    let server = StubServer::start(vec![StubResponse::empty(200)]).await;
    let client = client_for(&server, Duration::from_secs(5));

    let payload = Bytes::from_static(b"the fragment payload");
    client
        .put_object(BUCKET, KEY, payload.clone(), Some(&fragment_metadata()))
        .await
        .expect("the stub accepts the object");

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];

    assert_eq!(request.method, "PUT");
    assert_eq!(
        request.target, "/lore-store/3f2a1b",
        "an IP endpoint has no name to host a bucket, so the bucket is in the path"
    );
    assert_eq!(request.body, payload, "the body arrives unchanged");
    assert_eq!(request.header("x-amz-meta-lore-fragment"), Some("8:3:9"));

    let mut digest = [0u8; 64];
    assert_eq!(
        request.header("x-amz-content-sha256"),
        Some(payload_sha256(&request.body, &mut digest)),
        "the signed payload hash must describe the body sent"
    );

    let authorization = request.header("authorization").expect("a signed request");
    assert!(
        authorization.contains("/eu-west-1/s3/aws4_request"),
        "the scope must name the S3 service: {authorization}"
    );
    assert!(
        authorization.contains("x-amz-meta-lore-fragment"),
        "unsigned object metadata could be stripped or rewritten in flight: {authorization}"
    );
}

/// A `PutObject` carries the CRC32 of its body, signed with the rest of the request, so S3 refuses
/// a body damaged on the way rather than storing it.
#[tokio::test]
async fn a_put_object_carries_the_crc32_of_its_body() {
    let server = StubServer::start(vec![StubResponse::empty(200)]).await;
    let client = client_for(&server, Duration::from_secs(5));

    client
        .put_object(
            BUCKET,
            KEY,
            Bytes::from_static(b"123456789"),
            Some(&fragment_metadata()),
        )
        .await
        .expect("the stub accepts the object");

    let requests = server.requests();
    let request = &requests[0];
    assert_eq!(
        request.header("x-amz-checksum-crc32"),
        Some("y/Q5Jg=="),
        "the CRC32 of the check input 123456789 is 0xCBF43926, base64-encoded big-endian"
    );
    assert_eq!(request.header("x-amz-meta-lore-fragment"), Some("8:3:9"));

    let authorization = request.header("authorization").expect("a signed request");
    assert!(
        authorization.contains("x-amz-checksum-crc32"),
        "an unsigned checksum could be rewritten to match a damaged body: {authorization}"
    );
}

#[tokio::test]
async fn a_get_object_streams_its_body_and_reads_its_metadata() {
    let server = StubServer::start(vec![
        StubResponse::ok("the fragment payload").header("x-amz-meta-lore-fragment", "8:3:9"),
    ])
    .await;
    let client = client_for(&server, Duration::from_secs(5));

    let mut output = client
        .get_object(BUCKET, KEY, None)
        .await
        .expect("the stub answers");

    assert_eq!(output.metadata().get("lore-fragment"), Some("8:3:9"));
    assert_eq!(
        output.content_length(),
        Some("the fragment payload".len() as u64)
    );

    let mut body = Vec::new();
    while let Some(chunk) = output.body.next().await {
        body.extend_from_slice(&chunk.expect("the body reads"));
    }

    assert_eq!(body, b"the fragment payload");
    assert_eq!(server.requests()[0].method, "GET");
}

#[tokio::test]
async fn a_ranged_get_sends_and_signs_its_range() {
    let server = StubServer::start(vec![StubResponse::new(206, "frag")]).await;
    let client = client_for(&server, Duration::from_secs(5));

    client
        .get_object(BUCKET, KEY, Some(16..1040))
        .await
        .expect("the stub answers");

    let request = &server.requests()[0];
    assert_eq!(
        request.header("range"),
        Some("bytes=16-1039"),
        "an HTTP range is inclusive at both ends, a Rust one is not"
    );
    assert!(
        request
            .header("authorization")
            .expect("signed")
            .contains(";range;"),
        "the range decides which bytes come back, so it is signed"
    );
}

/// A `200` to a ranged read carries the whole object, which is what something in front of S3 that
/// ignored the range answers with. Handing that body over as the range would give the caller the
/// wrong bytes, so it is refused.
#[tokio::test]
async fn a_ranged_get_answered_with_the_whole_object_is_refused() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_owned()), async {
            let server = StubServer::start(vec![StubResponse::ok("the whole object")]).await;
            let client = client_for(&server, Duration::from_secs(5));

            let error = client
                .get_object(BUCKET, KEY, Some(4..8))
                .await
                .expect_err("a full body is not the range that was asked for");

            assert!(
                matches!(error, AwsCallError::Transport(TransportError::Protocol(_))),
                "unexpected failure: {error:?}"
            );
            assert_eq!(server.request_count(), 1);
        })
        .await;
}

/// A key with a `.` or `..` segment would address another object once the URL resolved it, so no
/// operation sends one.
#[tokio::test]
async fn a_key_with_a_dot_segment_is_refused_without_sending_anything() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_owned()), async {
            let server = StubServer::start(vec![StubResponse::empty(200)]).await;
            let client = client_for(&server, Duration::from_secs(5));
            let key = "../other-bucket/x";

            let errors = [
                client.head_object(BUCKET, key).await.err(),
                client.get_object(BUCKET, key, None).await.err(),
                client
                    .put_object(BUCKET, key, Bytes::from_static(b"payload"), None)
                    .await
                    .err(),
                client.delete_object(BUCKET, key, None).await.err(),
            ];

            for error in errors {
                assert!(
                    matches!(
                        error,
                        Some(AwsCallError::Transport(TransportError::Build(_)))
                    ),
                    "unexpected outcome: {error:?}"
                );
            }
            assert_eq!(server.request_count(), 0);
        })
        .await;
}

/// An empty range would be sent as `bytes=n-(n-1)`, which S3 answers with a 416. Refusing it here says
/// what is wrong instead.
#[tokio::test]
async fn an_empty_range_is_refused_without_sending_anything() {
    let server = StubServer::start(vec![StubResponse::ok("frag")]).await;
    let client = client_for(&server, Duration::from_secs(5));

    let error = client
        .get_object(BUCKET, KEY, Some(16..16))
        .await
        .expect_err("an empty range is a caller error");

    assert!(
        matches!(error, AwsCallError::Transport(_)),
        "unexpected failure: {error:?}"
    );
    assert_eq!(server.request_count(), 0);
}

/// A missing object is the answer to "is this hash stored", so it must arrive as a code the store can
/// branch on — and it must not be retried, because it will not change.
#[tokio::test]
async fn a_missing_object_reads_as_not_found_and_is_not_retried() {
    let server = StubServer::start(vec![StubResponse::empty(404)]).await;
    let client = client_for(&server, Duration::from_secs(5));

    let error = client
        .head_object(BUCKET, KEY)
        .await
        .expect_err("a missing object fails");

    let service = error.as_service_error().expect("a service error");
    assert!(
        service.code.is_not_found(),
        "a bodyless 404 is how HeadObject says no such key: {:?}",
        service.code
    );
    assert_eq!(server.request_count(), 1, "absence must not be retried");
}

/// A HEAD answers a key it may not read with a bare 403, a missing key included when the caller
/// lacks `s3:ListBucket`, so the refusal goes straight back without a credential refresh or a
/// second request.
#[tokio::test]
async fn a_bodyless_403_is_not_re_sent() {
    let server = StubServer::start(vec![StubResponse::empty(403), StubResponse::empty(200)]).await;
    let client = client_for(&server, Duration::from_secs(5));

    let error = client
        .head_object(BUCKET, KEY)
        .await
        .expect_err("a refusal fails");

    assert_eq!(
        error.as_service_error().map(|service| service.code.clone()),
        Some(S3ErrorCode::Forbidden)
    );
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn head_object_reads_the_metadata_off_a_bodyless_response() {
    let server = StubServer::start(vec![
        StubResponse::empty(200).header("x-amz-meta-lore-fragment", "8:3:9"),
    ])
    .await;
    let client = client_for(&server, Duration::from_secs(5));

    let output = client
        .head_object(BUCKET, KEY)
        .await
        .expect("the stub answers");

    assert_eq!(output.metadata().get("lore-fragment"), Some("8:3:9"));
    assert_eq!(server.requests()[0].method, "HEAD");
}

/// A slow down is congestion, and congestion belongs to the storage layer's ladder rather than to a
/// second one down here that would put the request back on the wire in twenty-five milliseconds.
#[tokio::test]
async fn a_slow_down_goes_straight_back_to_the_caller() {
    let server = StubServer::start(vec![
        StubResponse::s3_error(503, "SlowDown"),
        StubResponse::empty(200),
    ])
    .await;
    let client = client_for(&server, Duration::from_secs(5));

    let error = client
        .put_object(BUCKET, KEY, Bytes::from_static(b"payload"), None)
        .await
        .expect_err("a slow down is the caller's to wait out");

    assert_eq!(
        error.as_service_error().map(|service| service.code.clone()),
        Some(S3ErrorCode::SlowDown)
    );
    assert_eq!(server.request_count(), 1);
}

/// A dropped connection is still this client's to redo: one request, not the store operation.
#[tokio::test]
async fn a_dropped_connection_is_retried_and_then_succeeds() {
    let server = StubServer::start(vec![StubResponse::hangup(), StubResponse::empty(200)]).await;
    let client = client_for(&server, Duration::from_secs(5));

    client
        .put_object(BUCKET, KEY, Bytes::from_static(b"payload"), None)
        .await
        .expect("the retry succeeds");

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].body, requests[1].body,
        "a retry re-sends the identical body"
    );
}

#[tokio::test]
async fn a_bucket_reads_as_present_or_absent_rather_than_failing() {
    let present = StubServer::start(vec![StubResponse::empty(200)]).await;
    assert!(
        client_for(&present, Duration::from_secs(5))
            .bucket_exists(BUCKET)
            .await
            .expect("not an error")
    );

    let absent = StubServer::start(vec![StubResponse::empty(404)]).await;
    assert!(
        !client_for(&absent, Duration::from_secs(5))
            .bucket_exists(BUCKET)
            .await
            .expect("not an error")
    );

    // A forbidden bucket may well exist, so reporting it absent would send a deployment looking for
    // the wrong problem.
    let forbidden = StubServer::start(vec![StubResponse::s3_error(403, "AccessDenied")]).await;
    assert!(
        client_for(&forbidden, Duration::from_secs(5))
            .bucket_exists(BUCKET)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_delete_names_the_version_it_removes() {
    let server = StubServer::start(vec![StubResponse::empty(204)]).await;
    let client = client_for(&server, Duration::from_secs(5));

    client
        .delete_object(BUCKET, KEY, Some("a/version+id"))
        .await
        .expect("the stub answers");

    let request = &server.requests()[0];
    assert_eq!(request.method, "DELETE");
    assert_eq!(
        request.target, "/lore-store/3f2a1b?versionId=a%2Fversion%2Bid",
        "a version id is opaque, so it is encoded rather than pasted in"
    );
}

fn versions_page(entries: &str, next: Option<(&str, &str)>) -> StubResponse {
    let truncated = next.is_some();
    let markers = next.map_or_default(|(key, version)| {
        format!(
            "<NextKeyMarker>{key}</NextKeyMarker>\
             <NextVersionIdMarker>{version}</NextVersionIdMarker>"
        )
    });

    StubResponse::ok(&format!(
        "<ListVersionsResult><Name>lore-store</Name>\
         <IsTruncated>{truncated}</IsTruncated>{markers}{entries}</ListVersionsResult>"
    ))
}

fn version_entry(version: &str) -> String {
    format!("<Version><Key>{KEY}</Key><VersionId>{version}</VersionId></Version>")
}

/// A key with more versions than a page holds is listed in full, or an obliteration would leave the
/// versions past the first page behind and report success.
#[tokio::test]
async fn a_version_listing_follows_its_markers_to_the_end() {
    let server = StubServer::start(vec![
        versions_page(
            &format!("{}{}", version_entry("v1"), version_entry("v2")),
            Some((KEY, "v2")),
        ),
        versions_page(&version_entry("v3"), None),
    ])
    .await;
    let client = client_for(&server, Duration::from_secs(5));

    let output = client
        .list_object_versions(BUCKET, KEY)
        .await
        .expect("the listing completes");

    assert!(!output.truncated, "the listing reached its end");
    assert_eq!(
        output
            .versions_of(KEY)
            .map(|version| version.version_id.as_deref())
            .collect::<Vec<_>>(),
        [Some("v1"), Some("v2"), Some("v3")],
        "every version must reach the caller, or an obliteration leaves bytes behind"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "the second page was fetched");
    assert_eq!(requests[0].target, "/lore-store?versions&prefix=3f2a1b");
    assert_eq!(
        requests[1].target,
        "/lore-store?versions&prefix=3f2a1b&key-marker=3f2a1b&version-id-marker=v2",
        "the second page has to continue from where the first stopped"
    );
}

/// The null version is listed with the id `null`, and deleting by that id is what removes it: a
/// delete with no id would only add a delete marker on a versioned bucket.
#[tokio::test]
async fn the_null_version_is_deleted_by_its_listed_id() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_owned()), async {
            let server = StubServer::start(vec![
                versions_page(&version_entry("null"), None),
                StubResponse::empty(204),
            ])
            .await;
            let client = client_for(&server, Duration::from_secs(5));

            let output = client
                .list_object_versions(BUCKET, KEY)
                .await
                .expect("the listing completes");
            for version in output.versions_of(KEY) {
                client
                    .delete_object(BUCKET, KEY, version.version_id.as_deref())
                    .await
                    .expect("the stub answers");
            }

            let requests = server.requests();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[1].method, "DELETE");
            assert_eq!(requests[1].target, "/lore-store/3f2a1b?versionId=null");
        })
        .await;
}

/// A service that says "truncated" without saying where to continue would be asked for ever. Stopping
/// is right; stopping *quietly* is not, so the output says the listing is unfinished.
#[tokio::test]
async fn a_truncated_listing_without_markers_stops_and_says_so() {
    let server = StubServer::start(vec![StubResponse::ok(
        "<ListVersionsResult><IsTruncated>true</IsTruncated>\
         <Version><Key>3f2a1b</Key><VersionId>v1</VersionId></Version></ListVersionsResult>",
    )])
    .await;
    let client = client_for(&server, Duration::from_secs(5));

    let output = client
        .list_object_versions(BUCKET, KEY)
        .await
        .expect("an unfinished listing is not an error");

    assert!(output.truncated, "the caller has to be able to tell");
    assert_eq!(output.versions.len(), 1);
    assert_eq!(server.request_count(), 1, "it must not loop");
}

/// A marker that does not advance is the same trap by another route.
#[tokio::test]
async fn a_listing_whose_markers_repeat_stops() {
    let repeated = versions_page(&version_entry("v1"), Some((KEY, "v1")));
    let server = StubServer::start(vec![repeated]).await;
    let client = client_for(&server, Duration::from_secs(5));

    let output = client
        .list_object_versions(BUCKET, KEY)
        .await
        .expect("not an error");

    assert!(output.truncated);
    assert_eq!(
        server.request_count(),
        2,
        "one page, one attempt at continuing, then it stops"
    );
}

#[tokio::test]
async fn a_malformed_listing_is_a_protocol_failure() {
    let server = StubServer::start(vec![StubResponse::ok(
        "<ListVersionsResult><Version><Key>3f2a",
    )])
    .await;
    let client = client_for(&server, Duration::from_millis(400));

    let error = client
        .list_object_versions(BUCKET, KEY)
        .await
        .expect_err("a truncated document cannot be read");

    assert!(
        matches!(error, AwsCallError::Transport(_) | AwsCallError::Timeout),
        "unexpected failure: {error:?}"
    );
}

/// Object metadata reaches the wire as headers, so a value that could break a header must be refused
/// before it gets there.
#[tokio::test]
async fn metadata_that_would_break_a_header_is_refused() {
    let server = StubServer::start(vec![StubResponse::empty(200)]).await;
    let client = client_for(&server, Duration::from_secs(5));

    for (name, value) in [
        ("lore-fragment", "8:3:9\r\nx-amz-acl: public-read"),
        ("lore fragment", "8:3:9"),
        ("lore-fragment\r\nx-amz-acl", "8:3:9"),
    ] {
        let metadata =
            ObjectMetadata::from([(ByteStr::copy_from_str(name), ByteStr::copy_from_str(value))]);

        let error = client
            .put_object(BUCKET, KEY, Bytes::new(), Some(&metadata))
            .await
            .expect_err("a header break must not be smuggled through object metadata");

        assert!(
            matches!(error, AwsCallError::Transport(_)),
            "unexpected failure for {name:?}: {error:?}"
        );
    }

    assert_eq!(
        server.request_count(),
        0,
        "nothing may be sent once the metadata is known to be unusable"
    );
}

/// A 500 with an HTML body from something in front of S3 is still a server-side failure, and the
/// status is the only thing that says so. It reaches the caller as one, to be waited out there.
#[tokio::test]
async fn a_proxys_error_page_is_classified_by_its_status() {
    let server = StubServer::start(vec![
        StubResponse::new(502, "<html><body>502 Bad Gateway</body></html>"),
        StubResponse::empty(200),
    ])
    .await;
    let client = client_for(&server, Duration::from_secs(5));

    let error = client
        .head_object(BUCKET, KEY)
        .await
        .expect_err("a bad gateway is not an answer");

    assert_eq!(
        error.as_service_error().map(|service| service.code.clone()),
        Some(S3ErrorCode::InternalError),
        "a 5xx with no code we know is still a server-side failure"
    );
    assert_eq!(server.request_count(), 1);
}

/// The codes the stores branch on have to survive the whole round trip, not just the parser.
#[tokio::test]
async fn a_no_such_key_arrives_as_itself() {
    let server = StubServer::start(vec![StubResponse::s3_error(404, "NoSuchKey")]).await;
    let client = client_for(&server, Duration::from_secs(5));

    let error = client
        .get_object(BUCKET, KEY, None)
        .await
        .expect_err("a missing key fails");

    assert_eq!(
        error.as_service_error().map(|service| service.code.clone()),
        Some(S3ErrorCode::NoSuchKey)
    );
    // A failure keeps its request id whatever the client was configured to capture: it is what AWS
    // support asks for, and only *successful* responses pay to carry one.
    assert_eq!(error.request_id(), Some("STUB-REQ"));
}
