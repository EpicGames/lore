// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bytes::Bytes;
use http::HeaderMap;
use http::HeaderValue;
use http::StatusCode;
use lore_aws::aws::ClientCore;
use lore_aws::aws::ServiceProtocol;
use lore_aws::aws::error::AwsCallError;
use lore_aws::aws::error::ErrorPayload;
use lore_aws::aws::error::S3ErrorCode;
use lore_aws::aws::error::ServiceError;
use lore_aws::aws::error::ServiceErrorCode;
use lore_aws::aws::error::TransportError;
use lore_aws::aws::error::is_overloaded;
use lore_aws::aws::http::HttpResponse;
use lore_aws::aws::item::ByteStr;
use lore_aws::aws::s3::*;
use url::Url;

fn client(endpoint: &str, force_path_style: bool) -> S3Client {
    let http =
        lore_aws::aws::http::HttpClient::new(&lore_aws::clients::HttpClientSettings::default())
            .expect("a client builds");
    let credentials = Arc::new(lore_aws::aws::creds::CachedCredentials::new(Arc::new(
        lore_aws::aws::creds::StaticProvider::new(lore_aws::aws::creds::Credentials::new(
            "AKIDTEST",
            "secret-for-tests",
            None,
            None,
        )),
    )));

    S3Client::new(
        Arc::new(ClientCore::new(
            http,
            credentials,
            Arc::from("eu-west-1"),
            Url::parse(endpoint).expect("a test endpoint parses"),
            std::time::Duration::from_secs(1),
            false,
        )),
        force_path_style,
    )
}

#[test]
fn a_named_endpoint_puts_the_bucket_in_the_host() {
    let client = client("https://s3.eu-west-1.amazonaws.com", false);

    assert_eq!(
        client.url("lore-store", Some("ab12"), None).as_str(),
        "https://lore-store.s3.eu-west-1.amazonaws.com/ab12"
    );
    // Bucket-level: no key, and no stray trailing segment.
    assert_eq!(
        client.url("lore-store", None, None).as_str(),
        "https://lore-store.s3.eu-west-1.amazonaws.com/"
    );
}

#[test]
fn path_style_puts_the_bucket_in_the_path() {
    let client = client("https://s3.eu-west-1.amazonaws.com", true);

    assert_eq!(
        client.url("lore-store", Some("ab12"), None).as_str(),
        "https://s3.eu-west-1.amazonaws.com/lore-store/ab12"
    );
    assert_eq!(
        client.url("lore-store", None, Some("versions")).as_str(),
        "https://s3.eu-west-1.amazonaws.com/lore-store?versions"
    );
}

/// The integration tests point at `http://127.0.0.1:9000`, where there is no name to prefix a
/// bucket onto. Getting this wrong sends every request to a host that does not resolve.
#[test]
fn an_ip_endpoint_forces_path_style() {
    let client = client("http://127.0.0.1:9000", false);

    assert_eq!(
        client.url("lore-store", Some("ab12"), None).as_str(),
        "http://127.0.0.1:9000/lore-store/ab12"
    );
}

/// An endpoint behind a gateway can carry a path of its own, which the bucket and key follow.
#[test]
fn a_bucket_follows_the_endpoints_own_path() {
    let path_style = client("http://127.0.0.1:9000/s3/", false);
    assert_eq!(
        path_style.url("lore-store", Some("ab12"), None).as_str(),
        "http://127.0.0.1:9000/s3/lore-store/ab12"
    );
    assert_eq!(
        path_style.url("lore-store", None, None).as_str(),
        "http://127.0.0.1:9000/s3/lore-store"
    );

    let in_host = client("https://gw.example.com/s3", false);
    assert_eq!(
        in_host.url("lore-store", Some("ab12"), None).as_str(),
        "https://lore-store.gw.example.com/s3/ab12"
    );
}

/// A dotted bucket in the host does not match S3's wildcard certificate, so it goes in the path.
#[test]
fn a_bucket_that_is_not_a_dns_label_goes_in_the_path() {
    let client = client("https://s3.eu-west-1.amazonaws.com", false);

    for bucket in ["my.bucket", "ab", "under_score", "UPPER"] {
        assert!(
            client
                .url(bucket, Some("k"), None)
                .as_str()
                .starts_with("https://s3.eu-west-1.amazonaws.com/"),
            "{bucket} must not be addressed in the host"
        );
    }

    assert!(is_dns_label("lore-immutable-store-local"));
    assert!(!is_dns_label("-leading-dash"));
    assert!(!is_dns_label("trailing-dash-"));
    assert!(!is_dns_label(&"a".repeat(64)));
}

/// A `.` or `..` segment would be resolved away when the URL is built, sending the request to a
/// different key or, with the bucket in the path, a different bucket. Such a key is refused; a dot
/// inside a segment is an ordinary character.
#[test]
fn a_key_with_a_dot_segment_is_refused() {
    for key in ["..", ".", "../other-bucket/x", "a/./b", "a/..", "a/../b"] {
        assert!(
            matches!(
                check_key(key),
                Err(AwsCallError::Transport(TransportError::Build(_)))
            ),
            "{key:?}"
        );
    }

    for key in ["3f2a1b", "a..b", ".hidden", "a/.b/c", "..."] {
        assert!(check_key(key).is_ok(), "{key:?}");
    }
}

/// What is signed is the path as sent, so anything outside the unreserved set is encoded here and
/// nowhere else.
#[test]
fn a_key_is_percent_encoded_once() {
    let client = client("https://s3.eu-west-1.amazonaws.com", false);
    let url = client.url("lore-store", Some("dir/a b+c%d?e"), None);

    assert_eq!(url.path(), "/dir/a%20b%2Bc%25d%3Fe");
}

#[test]
fn the_addressing_cache_answers_the_same_bucket_without_rebuilding() {
    let client = client("https://s3.eu-west-1.amazonaws.com", false);
    let addressing = &client.addressing;
    let endpoint = client.core.endpoint();

    let first = addressing.base(endpoint, "lore-store");
    let again = addressing.base(endpoint, "lore-store");
    assert!(
        Arc::ptr_eq(&first, &again),
        "the same bucket must not rebuild its base URL"
    );

    let other = addressing.base(endpoint, "other-store");
    assert!(!Arc::ptr_eq(&first, &other));
    assert_eq!(
        other.url.host_str(),
        Some("other-store.s3.eu-west-1.amazonaws.com")
    );
}

#[test]
fn a_version_query_carries_its_markers_encoded() {
    assert_eq!(versions_query("abc", None, None), "versions&prefix=abc");
    assert_eq!(
        versions_query("abc", Some("key with space"), Some("v/1")),
        "versions&prefix=abc&key-marker=key%20with%20space&version-id-marker=v%2F1"
    );
}

fn error_response(status: u16, body: &str, request_id: Option<&str>) -> HttpResponse {
    let mut headers = HeaderMap::new();
    if let Some(id) = request_id {
        headers.insert("x-amz-request-id", HeaderValue::from_str(id).unwrap());
    }

    HttpResponse {
        status: StatusCode::from_u16(status).unwrap(),
        headers,
        body: Bytes::copy_from_slice(body.as_bytes()),
    }
}

#[test]
fn parses_an_s3_error_body() {
    let error = S3Protocol::parse_error(&error_response(
        404,
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            "<Error><Code>NoSuchKey</Code><Message>no such key</Message>",
            "<RequestId>FROM-BODY</RequestId></Error>",
        ),
        Some("FROM-HEADER"),
    ));

    assert_eq!(error.code, S3ErrorCode::NoSuchKey);
    assert_eq!(error.message.as_deref(), Some("no such key"));
    assert_eq!(
        error.request_id.as_deref(),
        Some("FROM-HEADER"),
        "the header is the authority; the body is the fallback"
    );
}

#[test]
fn an_error_without_a_request_id_header_takes_the_one_in_the_body() {
    let error = S3Protocol::parse_error(&error_response(
        403,
        "<Error><Code>AccessDenied</Code><RequestId>FROM-BODY</RequestId></Error>",
        None,
    ));

    assert_eq!(error.request_id.as_deref(), Some("FROM-BODY"));
}

/// `HeadObject` reports a missing key with a bare 404 and no body at all.
#[test]
fn a_bodyless_head_error_reads_its_status() {
    let error = S3Protocol::parse_error(&error_response(404, "", None));

    assert_eq!(error.code, S3ErrorCode::NotFound);
    assert!(error.code.is_not_found());
}

/// Every code S3 answers a transient condition with must classify as retryable.
///
/// `RequestTimeout` is the one worth naming: S3 sends it as a 400 when a socket was too slow, and
/// a 400 that reads as a hard failure turns a stalled upload into a failed push.
#[test]
fn transient_s3_codes_are_retried() {
    for (status, code) in [
        (503, "SlowDown"),
        (503, "ServiceUnavailable"),
        (500, "InternalError"),
        (400, "RequestTimeout"),
        (400, "RequestTimeoutException"),
        (429, "TooManyRequests"),
    ] {
        let body = format!("<Error><Code>{code}</Code></Error>");

        let error = S3Protocol::parse_error(&error_response(status, &body, None));

        assert_eq!(
            error.code.retryable(),
            lore_aws::aws::error::Retryable::Yes,
            "{code}"
        );
    }
}

/// A shed request must be retried, whatever the store calls it.
///
/// This is the response `MinIO` answers with once `MINIO_API_REQUESTS_MAX` binds — a 429 with a
/// code AWS itself never sends. Classified as an unrecognised code it would never be retried, so a
/// throttle would reach the store as a hard failure and the only recovery left would be the storage
/// layer retrying the whole operation.
#[test]
fn a_shed_request_is_a_slow_down_however_it_is_spelled() {
    let minio = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
        "<Error><Code>TooManyRequests</Code>",
        "<Message>Please reduce your request rate</Message>",
        "<Resource>/lore/0123</Resource><RequestId>18CE22C45F2A09A3</RequestId></Error>"
    );

    let error = S3Protocol::parse_error(&error_response(429, minio, None));

    assert_eq!(error.code, S3ErrorCode::SlowDown);
    assert_eq!(error.code.retryable(), lore_aws::aws::error::Retryable::Yes);

    // A 429 whose body did not survive reads the same.
    let bodyless = S3Protocol::parse_error(&error_response(429, "", None));

    assert_eq!(bodyless.code, S3ErrorCode::SlowDown);
    assert_eq!(
        bodyless.code.retryable(),
        lore_aws::aws::error::Retryable::Yes,
        "the status alone says to come back later"
    );
}

/// A 408 arrives without a body from the operations that have none, and still has to read as the
/// timeout it is rather than as an unknown code.
#[test]
fn a_bodyless_timeout_reads_its_status() {
    let error = S3Protocol::parse_error(&error_response(408, "", None));

    assert_eq!(error.code, S3ErrorCode::RequestTimeout);
    assert_eq!(
        error.code.retryable(),
        lore_aws::aws::error::Retryable::Yes,
        "a timeout is what retrying exists for"
    );
}

/// Both spellings of an expired session must ask for a refresh rather than a re-send.
#[test]
fn expired_credentials_are_recognised_under_either_spelling() {
    for code in ["ExpiredToken", "TokenRefreshRequired"] {
        let body = format!("<Error><Code>{code}</Code></Error>");

        let error = S3Protocol::parse_error(&error_response(400, &body, None));

        assert_eq!(error.code, S3ErrorCode::ExpiredToken, "{code}");
        assert!(
            error.code.is_expired_credentials(),
            "{code} must trigger a refresh"
        );
    }
}

/// A body that is not text must not panic and must not lose the status.
#[test]
fn a_body_that_is_not_utf8_falls_back_to_the_status() {
    let response = HttpResponse {
        status: StatusCode::SERVICE_UNAVAILABLE,
        headers: HeaderMap::new(),
        body: Bytes::from_static(&[0xff, 0xfe, 0x00]),
    };

    let error = S3Protocol::parse_error(&response);

    assert_eq!(error.code, S3ErrorCode::ServiceUnavailable);
    assert_eq!(error.code.retryable(), lore_aws::aws::error::Retryable::Yes);
}

#[test]
fn object_metadata_takes_only_its_own_headers() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-amz-meta-lore-fragment",
        HeaderValue::from_static("8:1:2"),
    );
    headers.insert("x-amz-request-id", HeaderValue::from_static("R1"));
    headers.insert("content-length", HeaderValue::from_static("2"));

    let metadata = ObjectMetadata::from_headers(&headers);

    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata.get("lore-fragment"), Some("8:1:2"));
    assert_eq!(metadata.get("request-id"), None);
    assert_eq!(content_length(&headers), Some(2));
}

/// Damaged metadata must not read as absent: the store treats absence as "written before the
/// metadata existed" and goes looking for a legacy row, which is the wrong recovery for damage.
#[test]
fn metadata_that_is_not_text_is_kept_as_empty() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-amz-meta-lore-fragment",
        HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
    );

    let metadata = ObjectMetadata::from_headers(&headers);

    assert_eq!(metadata.get("lore-fragment"), Some(""));
}

#[test]
fn metadata_insert_replaces_rather_than_duplicating() {
    let mut metadata = ObjectMetadata::new();
    metadata.insert(ByteStr::from_static("a"), ByteStr::from_static("1"));
    metadata.insert(ByteStr::from_static("b"), ByteStr::from_static("2"));
    metadata.insert(ByteStr::from_static("a"), ByteStr::from_static("3"));

    assert_eq!(metadata.len(), 2);
    assert_eq!(metadata.get("a"), Some("3"));
    assert_eq!(
        metadata.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        ["a", "b"],
        "ordering is insertion order, so two identical requests serialize identically"
    );
}

#[test]
fn parses_a_version_listing_page() {
    let body = concat!(
        r#"<?xml version="1.0" encoding="UTF-8"?>"#,
        r#"<ListVersionsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
        "<Name>lore-store</Name><Prefix>ab</Prefix><MaxKeys>1000</MaxKeys>",
        "<IsTruncated>true</IsTruncated>",
        "<NextKeyMarker>ab</NextKeyMarker><NextVersionIdMarker>v2</NextVersionIdMarker>",
        "<Version><Key>ab</Key><VersionId>v1</VersionId><IsLatest>false</IsLatest>",
        "<LastModified>2026-08-12T00:00:00.000Z</LastModified><ETag>&quot;e&quot;</ETag>",
        "<Size>3</Size><StorageClass>STANDARD</StorageClass>",
        "<Owner><ID>owner-id</ID><DisplayName>owner</DisplayName></Owner></Version>",
        "<DeleteMarker><Key>ab</Key><VersionId>v2</VersionId><IsLatest>true</IsLatest>",
        "<Owner><ID>owner-id</ID></Owner></DeleteMarker>",
        "</ListVersionsResult>",
    );

    let page = parse_versions_page(body.as_bytes()).expect("a well-formed page parses");

    assert!(page.truncated);
    assert_eq!(page.next_key_marker.as_deref(), Some("ab"));
    assert_eq!(page.next_version_id_marker.as_deref(), Some("v2"));
    assert_eq!(page.versions.len(), 2, "a delete marker is a version too");

    assert_eq!(page.versions[0].key, "ab");
    assert_eq!(page.versions[0].version_id.as_deref(), Some("v1"));
    assert!(!page.versions[0].is_delete_marker);
    assert!(!page.versions[0].is_latest);

    assert!(page.versions[1].is_delete_marker);
    assert!(page.versions[1].is_latest);
    assert_eq!(page.versions[1].version_id.as_deref(), Some("v2"));
}

/// A page holds at most S3's thousand entries, so one naming more is refused rather than read.
#[test]
fn a_version_page_past_its_entry_limit_is_refused() {
    let page = |entries: usize| {
        let mut body = String::from("<ListVersionsResult><IsTruncated>false</IsTruncated>");
        for _ in 0..entries {
            body.push_str("<Version><Key>ab</Key><VersionId>v</VersionId></Version>");
        }
        body.push_str("</ListVersionsResult>");
        body
    };

    assert!(parse_versions_page(page(MAX_VERSIONS_PER_PAGE).as_bytes()).is_ok());
    assert!(matches!(
        parse_versions_page(page(MAX_VERSIONS_PER_PAGE + 1).as_bytes()),
        Err(TransportError::Protocol(_))
    ));
}

#[test]
fn a_finished_listing_has_no_markers() {
    let body = concat!(
        "<ListVersionsResult><IsTruncated>false</IsTruncated>",
        "<NextKeyMarker></NextKeyMarker>",
        "<Version><Key>ab</Key><VersionId>v1</VersionId></Version>",
        "</ListVersionsResult>",
    );

    let page = parse_versions_page(body.as_bytes()).unwrap();

    assert!(!page.truncated);
    assert_eq!(page.next_key_marker, None, "an empty marker means none");
    assert_eq!(page.versions.len(), 1);
}

/// An object written while versioning was off or suspended is the null version, listed with the
/// literal id `null`. Deleting by that id removes it; deleting with no id would only add a delete
/// marker on a versioned bucket, so the id is kept as listed.
#[test]
fn a_null_version_id_is_kept_as_the_id_to_delete_by() {
    let body = "<ListVersionsResult><Version><Key>ab</Key><VersionId>null</VersionId>\
                </Version></ListVersionsResult>";

    let page = parse_versions_page(body.as_bytes()).unwrap();

    assert_eq!(page.versions[0].version_id.as_deref(), Some("null"));
}

#[test]
fn an_empty_version_id_reads_as_absent() {
    let body = "<ListVersionsResult><Version><Key>ab</Key><VersionId></VersionId>\
                </Version></ListVersionsResult>";

    let page = parse_versions_page(body.as_bytes()).unwrap();

    assert_eq!(page.versions[0].version_id, None);
}

/// A key or version id carrying an entity reads as its unescaped text.
#[test]
fn a_listed_key_reads_with_its_entities_resolved() {
    let body = "<ListVersionsResult><Version><Key>a&amp;b</Key><VersionId>v&lt;1</VersionId>\
                </Version></ListVersionsResult>";

    let page = parse_versions_page(body.as_bytes()).unwrap();

    assert_eq!(page.versions[0].key, "a&b");
    assert_eq!(page.versions[0].version_id.as_deref(), Some("v<1"));
}

/// A document that is not a version listing — an error page answered with a 200, a different
/// operation's result — must not read as an empty listing, which an obliteration would take as
/// nothing left to delete.
#[test]
fn a_document_that_is_not_a_version_listing_is_a_protocol_failure() {
    for body in [
        &b"<ListBucketResult><Name>lore-store</Name></ListBucketResult>"[..],
        &b"<html><body>OK</body></html>"[..],
        &b""[..],
        &b"<?xml version=\"1.0\"?>"[..],
        &b"<ListVersionsResult>"[..],
    ] {
        assert!(
            matches!(parse_versions_page(body), Err(TransportError::Protocol(_))),
            "{body:?}"
        );
    }
}

/// A namespace prefix on the root names the same element.
#[test]
fn a_prefixed_root_is_a_version_listing() {
    let body = "<s3:ListVersionsResult xmlns:s3=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                <IsTruncated>false</IsTruncated></s3:ListVersionsResult>";

    let page = parse_versions_page(body.as_bytes()).unwrap();

    assert_eq!(page, VersionsPage::default());
}

#[test]
fn a_truncated_listing_document_is_a_protocol_failure() {
    for body in [
        &b"<ListVersionsResult><Version><Key>ab"[..],
        &b"<ListVersionsResult><IsTruncated"[..],
        &[0xff, 0xfe][..],
    ] {
        assert!(
            matches!(parse_versions_page(body), Err(TransportError::Protocol(_))),
            "a document that cannot be read must not read as an empty listing: {body:?}"
        );
    }
}

#[test]
fn an_empty_listing_is_not_an_error() {
    let page = parse_versions_page(b"<ListVersionsResult/>").unwrap();

    assert_eq!(page, VersionsPage::default());
}

#[test]
fn versions_of_takes_one_key_out_of_a_prefix_listing() {
    let output = ListObjectVersionsOutput {
        versions: vec![
            ObjectVersion {
                key: ByteStr::from_static("ab"),
                version_id: Some(ByteStr::from_static("v1")),
                is_delete_marker: false,
                is_latest: false,
            },
            ObjectVersion {
                key: ByteStr::from_static("ab"),
                version_id: Some(ByteStr::from_static("v2")),
                is_delete_marker: true,
                is_latest: true,
            },
            ObjectVersion {
                key: ByteStr::from_static("abc"),
                version_id: Some(ByteStr::from_static("v3")),
                is_delete_marker: false,
                is_latest: true,
            },
        ],
        truncated: false,
        request_id: None,
    };

    assert_eq!(
        output
            .versions_of("ab")
            .map(|version| version.version_id.as_deref())
            .collect::<Vec<_>>(),
        [Some("v1"), Some("v2")],
        "a listing is by prefix, so a caller that means one key has to say so"
    );
}

#[test]
fn overload_classification_matches_what_the_stores_back_off_on() {
    let throttled: AwsCallError<S3ErrorCode> = AwsCallError::service(ServiceError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: S3ErrorCode::SlowDown,
        message: None,
        request_id: None,
        payload: ErrorPayload::None,
    });
    let missing: AwsCallError<S3ErrorCode> = AwsCallError::service(ServiceError {
        status: StatusCode::NOT_FOUND,
        code: S3ErrorCode::NoSuchKey,
        message: None,
        request_id: None,
        payload: ErrorPayload::None,
    });

    assert!(is_overloaded(&throttled));
    assert!(!is_overloaded(&missing));
    assert!(is_overloaded(&AwsCallError::<S3ErrorCode>::Timeout));
    assert!(!is_overloaded(&AwsCallError::<S3ErrorCode>::Credentials {
        detail: "none".into(),
        transient: false,
    }));
    assert!(is_overloaded(&AwsCallError::<S3ErrorCode>::Credentials {
        detail: "throttled".into(),
        transient: true,
    }));
}

/// A shed request is heard through a code no build knows, because the status says so too.
///
/// An endpoint in front of S3 names its own exceptions, and an unrecognised code on its own is
/// never retryable, so without the status a proxy's 503 would surface as a hard failure.
#[test]
fn a_shedding_status_is_backed_off_under_any_code() {
    for status in [
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::GATEWAY_TIMEOUT,
        StatusCode::TOO_MANY_REQUESTS,
    ] {
        let error: AwsCallError<S3ErrorCode> = AwsCallError::service(ServiceError {
            status,
            code: S3ErrorCode::Other(ByteStr::from_static("BackendBusy")),
            message: None,
            request_id: None,
            payload: ErrorPayload::None,
        });

        assert!(is_overloaded(&error), "{status} is the service shedding");
    }

    let rejected: AwsCallError<S3ErrorCode> = AwsCallError::service(ServiceError {
        status: StatusCode::BAD_REQUEST,
        code: S3ErrorCode::Other(ByteStr::from_static("MalformedXML")),
        message: None,
        request_id: None,
        payload: ErrorPayload::None,
    });

    assert!(!is_overloaded(&rejected));
}
