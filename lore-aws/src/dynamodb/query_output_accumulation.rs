// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use crate::aws::item::Item;

/// The outputs of multiple `QueryOutput`s combined
/// into 1 unified result. Partially mirrors the fields of `QueryOutput` for ease
/// of swapping `QueryOutput` for this type
#[derive(Debug, Default)]
pub struct QueryOutputAccumulation {
    pub items: Option<Vec<Item>>,
    pub count: i32,
}

impl QueryOutputAccumulation {
    /// Add one page's items and count.
    ///
    /// Takes the page's fields rather than the page, so the caller keeps its `last_evaluated_key`.
    /// The first page's items are taken whole rather than copied.
    pub fn extend(&mut self, new_items: Option<Vec<Item>>, count: i32) {
        if let Some(mut new_items) = new_items {
            if let Some(items) = self.items.as_mut() {
                items.append(&mut new_items);
            } else {
                self.items = Some(new_items);
            }
        }
        self.count += count;
    }
}

#[cfg(feature = "test-util")]
impl From<crate::aws::dynamodb::QueryOutput> for QueryOutputAccumulation {
    fn from(value: crate::aws::dynamodb::QueryOutput) -> Self {
        let mut result = QueryOutputAccumulation::default();
        result.extend(value.items, value.count);
        result
    }
}
