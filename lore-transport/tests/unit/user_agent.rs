// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod compose_user_agent {
    use lore_base::version::LORE_LIBRARY_VERSION;
    use lore_transport::compose_user_agent;

    fn lore_token() -> String {
        format!("lore/{}", LORE_LIBRARY_VERSION.as_str())
    }

    #[test]
    fn is_only_the_lore_token_when_nothing_is_named() {
        assert_eq!(compose_user_agent(None, None), lore_token());
    }

    #[test]
    fn puts_the_application_before_the_lore_token() {
        assert_eq!(
            compose_user_agent(Some("my-app"), None),
            format!("my-app {}", lore_token())
        );
    }

    #[test]
    fn puts_the_product_before_the_lore_token() {
        assert_eq!(
            compose_user_agent(None, Some("my-server")),
            format!("my-server {}", lore_token())
        );
    }

    #[test]
    fn puts_the_application_before_the_product() {
        assert_eq!(
            compose_user_agent(Some("My App"), Some("my-server")),
            format!("My App my-server {}", lore_token())
        );
    }
}
