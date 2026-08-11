// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::any::Any;
use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use lore_revision::lore::RepositoryId;
use tracing::warn;

use crate::auth::jwt::AuthorizationToken;
use crate::util::get_user_id_from_token;

type AnyMap = HashMap<TypeId, Arc<dyn Any + Send + Sync>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionId(pub usize);

#[derive(Default)]
pub struct AttributeMap {
    map: Arc<RwLock<AnyMap>>,
}

impl AttributeMap {
    pub fn insert<T: Send + Sync + 'static>(&self, val: T) {
        match self.map.write() {
            Ok(mut m) => {
                m.insert(TypeId::of::<T>(), Arc::new(val));
            }
            Err(e) => {
                warn!("Failed to get write lock when writing to attribute map: {e:?}");
            }
        }
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        match self.map.read() {
            Ok(m) => m
                .get(&TypeId::of::<T>())
                .and_then(|boxed| boxed.clone().downcast().ok()),
            Err(e) => {
                warn!("Failed to get read lock when reading from attribute map: {e:?}");
                None
            }
        }
    }

    pub fn get_or<T: Send + Sync + 'static, E>(&self, err: E) -> Result<Arc<T>, E> {
        match self.get::<T>() {
            Some(v) => Ok(v),
            None => Err(err),
        }
    }
}

pub fn get_user_id_from_context(context: &Arc<AttributeMap>) -> String {
    let token = context
        .get::<AuthorizationToken>()
        .as_ref()
        .map(|token| (**token).clone());
    get_user_id_from_token(token)
}

pub fn repository_id_from_context(context: &Arc<AttributeMap>) -> String {
    context
        .get::<RepositoryId>()
        .map_or_else(|| "<no_repo_id>".to_string(), |id| id.to_string())
}

#[cfg(test)]
mod tests {
    use lore_base::lore_spawn;

    use super::*;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct TestData {
        foo: &'static str,
        bar: Vec<u8>,
    }

    #[tokio::test]
    async fn test_attribute_map() {
        let map = Arc::new(AttributeMap::default());

        let m: Arc<AttributeMap> = Arc::clone(&map);
        lore_spawn!(async move {
            m.insert(42);
        })
        .await
        .expect("failed to await");

        assert_eq!(&42, &*map.get::<i32>().unwrap());

        let m: Arc<AttributeMap> = Arc::clone(&map);
        lore_spawn!(async move {
            m.insert(834);
        })
        .await
        .expect("failed to await");

        assert_eq!(&834, &*map.get::<i32>().unwrap());

        let data = TestData {
            foo: "bar",
            bar: b"hello".to_vec(),
        };
        let data_clone = data.clone();

        let m: Arc<AttributeMap> = Arc::clone(&map);
        lore_spawn!(async move {
            m.insert(data_clone);
        })
        .await
        .expect("failed to await");

        assert_eq!(&data, &*map.get::<TestData>().unwrap());
    }

    #[test]
    fn test_get_or() {
        let map = AttributeMap::default();

        map.insert(42);

        assert_eq!(
            &42,
            &*map.get_or::<i32, &str>("Not Found").expect("failed to get")
        );

        assert_eq!(
            "Not Found",
            map.get_or::<TestData, &str>("Not Found")
                .expect_err("should have returned an error")
        );
    }
}
