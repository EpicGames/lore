// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod tests {
    use std::time::Duration;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    #[test]
    fn expiry_time_is_expired() {
        let current = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();

        assert!(lore_transport::auth::exchange::is_expired(
            current.as_millis() as u64
        ));
    }

    #[test]
    fn one_second_in_the_future_is_not_expired() {
        let current = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();

        assert!(!lore_transport::auth::exchange::is_expired(
            (current + Duration::from_secs(1)).as_millis() as u64
        ));
    }

    #[test]
    fn one_second_in_the_past_is_expired() {
        let current = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();

        assert!(lore_transport::auth::exchange::is_expired(
            (current - Duration::from_secs(1)).as_millis() as u64
        ));
    }

    mod verify_jwt_usage_for_remote_tests {

        use lore_credential::JWTUserInfo;
        use lore_credential::verify_jwt_usage_for_remote;

        fn make_jwt_with_audience(audience: Vec<String>) -> JWTUserInfo {
            JWTUserInfo {
                issuer: "my_test_issuer.example.com".to_string(),
                user_id: "my_user_id".into(),
                name: "my_name".into(),
                preferred_username: None,
                is_service_account: None,
                expires: 1,
                audience,
            }
        }

        #[test]
        fn acceptable_jwt_domains() {
            let token = make_jwt_with_audience(vec![
                "lore-1.example.com".to_string(),
                "lore-2.example.com".to_string(),
            ]);
            let acceptable_domains = token.acceptable_root_domains();

            assert_eq!(
                acceptable_domains,
                vec![
                    "my_test_issuer.example.com".to_string(),
                    "lore-1.example.com".to_string(),
                    "lore-2.example.com".to_string(),
                ]
            );
        }

        #[test]
        fn errors_on_domain_not_in_audience() {
            let token = make_jwt_with_audience(vec!["real.lore.example.com".to_string()]);
            verify_jwt_usage_for_remote(&token, "attacker.lore.example.com").unwrap_err();
        }

        #[test]
        fn allows_exact_jwt_aud_use() {
            let token = make_jwt_with_audience(vec![
                "some_other_remote.example.com".to_string(),
                "lore.example.com".to_string(),
            ]);

            verify_jwt_usage_for_remote(&token, "lore.example.com").unwrap();
        }

        #[test]
        fn allows_jwt_aud_root_domain_matching_use() {
            let token = make_jwt_with_audience(vec!["lore.example.com".to_string()]);

            verify_jwt_usage_for_remote(&token, "lore-server.lore.example.com").unwrap();
        }

        #[test]
        fn leading_dot_aud_matches_subdomains_and_apex() {
            let token = make_jwt_with_audience(vec![".lore.example.com".to_string()]);

            // Subdomains ("*.lore.example.com") match.
            verify_jwt_usage_for_remote(&token, "lore-server.lore.example.com").unwrap();
            // The bare apex domain ("lore.example.com") also matches.
            verify_jwt_usage_for_remote(&token, "lore.example.com").unwrap();
        }

        #[test]
        fn leading_dot_aud_rejects_unrelated_domain() {
            let token = make_jwt_with_audience(vec![".lore.example.com".to_string()]);

            verify_jwt_usage_for_remote(&token, "attackerlore.example.com").unwrap_err();
        }

        #[test]
        fn naked_aud_rejects_mid_label_suffix_domain() {
            // A dotless `aud` must respect the label boundary.
            let token = make_jwt_with_audience(vec!["epicgames.net".to_string()]);

            // The apex and true subdomains match.
            verify_jwt_usage_for_remote(&token, "epicgames.net").unwrap();
            verify_jwt_usage_for_remote(&token, "lore.epicgames.net").unwrap();
            // The look-alike registrable domain does not.
            verify_jwt_usage_for_remote(&token, "evilepicgames.net").unwrap_err();
        }
    }
}
