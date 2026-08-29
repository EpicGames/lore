// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod jwt;
pub mod token_store;
pub mod util;

// Re-export key types at the crate root for convenience
pub use jwt::JWTUserInfo;
pub use jwt::JwtUsageError;
pub use jwt::UserInfo;
pub use jwt::domain_in_root_domains;
pub use jwt::identity_from_token;
pub use jwt::insecure_decode_token;
pub use jwt::user_info;
pub use jwt::user_info_from_token;
pub use jwt::verify_jwt_usage_for_remote;
pub use token_store::IdentityToken;
pub use token_store::StoredIdentityInfo;
pub use token_store::TokenStoreError;
pub use util::domain_from_url_or_url;
pub use util::domain_from_url_str_or_url;
pub use util::get_domain_or_empty;
pub use util::token_fingerprint;
