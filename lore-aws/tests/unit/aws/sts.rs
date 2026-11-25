// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use http::StatusCode;
use lore_aws::aws::creds::CredentialsError;
use lore_aws::aws::sts::*;

use crate::aws::utc;

const RESPONSE: &str = r#"<AssumeRoleWithWebIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <AssumeRoleWithWebIdentityResult>
<SubjectFromWebIdentityToken>system:serviceaccount:lore:lore</SubjectFromWebIdentityToken>
<Audience>sts.amazonaws.com</Audience>
<AssumedRoleUser>
  <Arn>arn:aws:sts::111122223333:assumed-role/LoreRole/lore</Arn>
  <AssumedRoleId>AROAEXAMPLE:lore</AssumedRoleId>
</AssumedRoleUser>
<Credentials>
  <AccessKeyId>ASIAEXAMPLE</AccessKeyId>
  <SecretAccessKey>secret-from-sts</SecretAccessKey>
  <SessionToken>token-from-sts</SessionToken>
  <Expiration>2026-08-12T15:00:00Z</Expiration>
</Credentials>
<Provider>arn:aws:iam::111122223333:oidc-provider/oidc.eks</Provider>
  </AssumeRoleWithWebIdentityResult>
  <ResponseMetadata>
<RequestId>1234</RequestId>
  </ResponseMetadata>
</AssumeRoleWithWebIdentityResponse>"#;

#[test]
fn parses_a_role_assumption() {
    let credentials = parse_assume_role_response(RESPONSE).unwrap();

    assert_eq!(&*credentials.access_key_id, "ASIAEXAMPLE");
    assert_eq!(&*credentials.secret_access_key, "secret-from-sts");
    assert_eq!(credentials.session_token.as_deref(), Some("token-from-sts"));
    assert_eq!(credentials.expires_at, Some(utc(2026, 8, 12, 15, 0, 0)));
}

/// The response carries an `<Arn>` outside `<Credentials>`. Reading fields regardless of where
/// they sit would let unrelated elements land in the credentials.
#[test]
fn only_fields_inside_credentials_are_read() {
    let confusing = r#"<Response>
        <AssumedRoleUser><AccessKeyId>WRONG</AccessKeyId></AssumedRoleUser>
        <Credentials>
          <AccessKeyId>RIGHT</AccessKeyId>
          <SecretAccessKey>s</SecretAccessKey>
          <Expiration>2026-08-12T15:00:00Z</Expiration>
        </Credentials>
    </Response>"#;

    let credentials = parse_assume_role_response(confusing).unwrap();

    assert_eq!(&*credentials.access_key_id, "RIGHT");
}

/// A role assumption always expires, so an expiry that cannot be read must fail rather than
/// produce credentials the cache holds as permanent and never refreshes.
#[test]
fn an_unreadable_expiry_is_a_failure() {
    let body = r#"<Response><Credentials>
          <AccessKeyId>A</AccessKeyId>
          <SecretAccessKey>s</SecretAccessKey>
          <Expiration>soon</Expiration>
        </Credentials></Response>"#;

    let error = parse_assume_role_response(body).expect_err("not an expiry");

    assert!(matches!(error, CredentialsError::Failed { .. }), "{error}");
}

#[test]
fn a_response_missing_a_credentials_field_is_a_failure() {
    for body in [
        "",
        "<Response/>",
        "<Response><Credentials></Credentials></Response>",
        "<Response><Credentials><AccessKeyId>A</AccessKeyId></Credentials></Response>",
        "<Response><Credentials><AccessKeyId>A</AccessKeyId><SecretAccessKey>s</SecretAccessKey></Credentials></Response>",
        "not xml at all",
    ] {
        assert!(
            parse_assume_role_response(body).is_err(),
            "{body:?} carries no usable credentials"
        );
    }
}

/// A body cut short must fail rather than pass off what was read before the cut as the whole
/// answer: the fields ahead of it are enough to build credentials, and the expiry behind it is
/// what stops the cache holding them forever.
#[test]
fn a_truncated_response_is_a_failure() {
    let mid_value = RESPONSE.find("token-from-sts").unwrap() + 5;
    let mid_tag = RESPONSE.find("<Expiration>").unwrap() + 6;

    for cut in [mid_value, mid_tag] {
        let error =
            parse_assume_role_response(&RESPONSE[..cut]).expect_err("the document does not end");

        assert!(
            matches!(
                error,
                CredentialsError::Failed {
                    provider: "web-identity",
                    ..
                }
            ),
            "a cut at {cut} must fail rather than produce credentials: {error:?}"
        );
    }
}

/// A shed STS answer is transient, so an operation waits it out instead of failing on a credential
/// refresh that would have succeeded moments later.
#[test]
fn a_shed_role_assumption_is_transient() {
    let throttling = r#"<ErrorResponse><Error>
          <Code>Throttling</Code>
          <Message>Rate exceeded</Message>
        </Error></ErrorResponse>"#;

    for (status, body) in [
        (StatusCode::BAD_REQUEST, throttling),
        (StatusCode::TOO_MANY_REQUESTS, ""),
        (StatusCode::SERVICE_UNAVAILABLE, ""),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"<ErrorResponse><Error><Code>IDPCommunicationError</Code></Error></ErrorResponse>"#,
        ),
    ] {
        let error = assume_role_error(status, body);

        assert!(
            matches!(error, CredentialsError::Transient { .. }),
            "{status} is STS shedding: {error}"
        );
        assert!(error.is_transient());
    }
}

/// A refused role assumption is not transient: waiting cannot make a rejected token valid, and
/// reporting it as backpressure would hide a misconfiguration behind a retry ladder.
#[test]
fn a_refused_role_assumption_is_a_failure() {
    for code in [
        "InvalidIdentityToken",
        "AccessDenied",
        "ExpiredTokenException",
    ] {
        let body = format!("<ErrorResponse><Error><Code>{code}</Code></Error></ErrorResponse>");
        let error = assume_role_error(StatusCode::BAD_REQUEST, &body);

        assert!(
            matches!(error, CredentialsError::Failed { .. }),
            "{code} is STS answering: {error}"
        );
        assert!(!error.is_transient());
    }
}

#[test]
fn the_form_body_encodes_every_value() {
    let body = form_body(
        "arn:aws:iam::1:role/Lore",
        "lore",
        "header.payload.signature+/=",
    );

    assert!(
        body.contains("RoleArn=arn%3Aaws%3Aiam%3A%3A1%3Arole%2FLore"),
        "an ARN's colons and slashes must be encoded: {body}"
    );
    assert!(
        body.contains("WebIdentityToken=header.payload.signature%2B%2F%3D"),
        "a token's reserved characters must be encoded: {body}"
    );
    assert!(body.starts_with("Action=AssumeRoleWithWebIdentity&Version=2011-06-15"));
}
