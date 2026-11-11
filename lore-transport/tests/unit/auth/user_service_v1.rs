// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_base::types::RepositoryId;
use lore_proto::lore::user::v1::User;
use lore_proto::lore::user::v1::UserFindRequest;
use lore_proto::lore::user::v1::UserFindResponse;
use lore_proto::lore::user::v1::UserGetRequest;
use lore_proto::lore::user::v1::UserGetResponse;
use lore_proto::lore::user::v1::user_find_request::Query;
use lore_proto::lore::user::v1::user_service_server::UserService as Service;
use lore_proto::lore::user::v1::user_service_server::UserServiceServer;
use lore_transport::auth::user_service_v1::UserServiceV1;
use lore_transport::traits::UserService;
use tonic::service::Routes;

/// `(id, display_name, username)`. Two users share the display name Sam.
const USERS: [(&str, &str, &str); 4] = [
    ("u-ann", "Ann Lee", "ann"),
    ("u-nameless", "", "nameless"),
    ("u-sam1", "Sam", "sam1"),
    ("u-sam2", "Sam", "sam2"),
];

fn users(matches: impl Fn(&(&str, &str, &str)) -> bool) -> Vec<User> {
    USERS
        .iter()
        .filter(|user| matches(user))
        .map(|(id, display_name, username)| User {
            id: id.to_string(),
            display_name: display_name.to_string(),
            username: username.to_string(),
        })
        .collect()
}

struct Directory {
    finds: AtomicUsize,
}

#[tonic::async_trait]
impl Service for Directory {
    async fn user_get(
        &self,
        request: tonic::Request<UserGetRequest>,
    ) -> Result<tonic::Response<UserGetResponse>, tonic::Status> {
        let request = request.into_inner();
        assert_eq!(request.partition.len(), 16, "the repository id bytes");
        let user = users(|(id, _, _)| request.id.iter().any(|wanted| wanted == id));
        Ok(tonic::Response::new(UserGetResponse { user }))
    }

    async fn user_find(
        &self,
        request: tonic::Request<UserFindRequest>,
    ) -> Result<tonic::Response<UserFindResponse>, tonic::Status> {
        self.finds.fetch_add(1, Ordering::AcqRel);
        let user = match request.into_inner().query {
            Some(Query::Username(name)) => users(|(_, _, username)| *username == name),
            Some(Query::DisplayName(name)) => users(|(_, display_name, _)| *display_name == name),
            None => Vec::new(),
        };
        Ok(tonic::Response::new(UserFindResponse { user }))
    }
}

async fn serve() -> (String, Arc<Directory>) {
    let directory = Arc::new(Directory {
        finds: AtomicUsize::new(0),
    });
    let address =
        crate::grpc::serve_on_loopback(Routes::new(UserServiceServer::from_arc(directory.clone())))
            .await;
    (format!("http://{address}"), directory)
}

#[tokio::test]
async fn user_get_reads_the_display_name_or_the_login_name() {
    let (url, _) = serve().await;
    let users = UserServiceV1
        .get_user_info(
            &url,
            "token",
            RepositoryId::default(),
            &[
                "u-ann".to_string(),
                "u-nameless".to_string(),
                "u-gone".to_string(),
            ],
            "corr",
        )
        .await
        .unwrap();
    let names: Vec<_> = users.iter().map(|u| (&*u.user_id, &*u.user_name)).collect();
    assert_eq!(names, [("u-ann", "Ann Lee"), ("u-nameless", "nameless")]);
}

async fn id_of(url: &str, name: &str) -> Option<String> {
    UserServiceV1
        .get_user_id(url, "token", RepositoryId::default(), name, "corr")
        .await
        .unwrap()
        .map(|u| u.user_id)
}

#[tokio::test]
async fn a_username_resolves_in_one_find() {
    let (url, directory) = serve().await;
    assert_eq!(id_of(&url, "ann").await.as_deref(), Some("u-ann"));
    assert_eq!(directory.finds.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn a_display_name_resolves_after_a_second_find() {
    let (url, directory) = serve().await;
    assert_eq!(id_of(&url, "Ann Lee").await.as_deref(), Some("u-ann"));
    assert_eq!(directory.finds.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn a_shared_display_name_and_an_unknown_name_resolve_to_none() {
    let (url, _) = serve().await;
    assert_eq!(id_of(&url, "Sam").await, None);
    assert_eq!(id_of(&url, "nobody").await, None);
}
