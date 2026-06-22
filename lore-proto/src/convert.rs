// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::types::Address;
use lore_base::types::BranchId;
use lore_base::types::BranchMetadata;
use lore_base::types::BranchPoint;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::LockData;
use lore_base::types::LockResource;

impl From<Address> for crate::model::Address {
    fn from(address: Address) -> Self {
        Self {
            hash: address.hash.into(),
            context: address.context.into(),
        }
    }
}

impl From<&Address> for crate::model::Address {
    fn from(address: &Address) -> Self {
        Self {
            hash: address.hash.into(),
            context: address.context.into(),
        }
    }
}

impl From<crate::model::Address> for Address {
    fn from(address: crate::model::Address) -> Self {
        Self {
            hash: Hash::from(address.hash),
            context: Context::from(address.context),
        }
    }
}

impl From<&crate::model::Address> for Address {
    fn from(address: &crate::model::Address) -> Self {
        Self {
            hash: address.hash.as_ref().into(),
            context: address.context.as_ref().into(),
        }
    }
}

impl From<Fragment> for crate::model::Fragment {
    fn from(fragment: Fragment) -> Self {
        Self {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

impl From<&Fragment> for crate::model::Fragment {
    fn from(fragment: &Fragment) -> Self {
        Self {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

impl From<crate::model::Fragment> for Fragment {
    fn from(fragment: crate::model::Fragment) -> Self {
        Self {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

impl From<&crate::model::Fragment> for Fragment {
    fn from(fragment: &crate::model::Fragment) -> Self {
        Self {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

// Branch type conversions

impl From<&BranchPoint> for crate::model::BranchPoint {
    fn from(point: &BranchPoint) -> Self {
        Self {
            branch: point.branch.into(),
            revision: point.revision.into(),
        }
    }
}

impl From<crate::model::BranchPoint> for BranchPoint {
    fn from(point: crate::model::BranchPoint) -> Self {
        Self {
            branch: point.branch.into(),
            revision: point.revision.into(),
        }
    }
}

impl From<BranchMetadata> for crate::model::Branch {
    fn from(metadata: BranchMetadata) -> Self {
        let stack: Vec<crate::model::BranchPoint> = metadata
            .stack
            .iter()
            .map(crate::model::BranchPoint::from)
            .collect();
        let branch_point_deprecated = stack
            .first()
            .map(|parent| parent.revision.clone())
            .or(Some(Hash::default().into()));
        let parent_deprecated = stack
            .first()
            .map(|parent| parent.branch.clone())
            .or(Some(Context::default().into()));

        Self {
            id: metadata.id.into(),
            name: metadata.name,
            category: metadata.category,
            latest: metadata.latest.into(),
            creator: metadata.creator,
            created: metadata.created,
            branch_point_deprecated,
            parent_deprecated,
            stack,
        }
    }
}

impl From<crate::model::Branch> for BranchMetadata {
    fn from(metadata: crate::model::Branch) -> Self {
        let mut stack: Vec<BranchPoint> =
            metadata.stack.into_iter().map(BranchPoint::from).collect();
        if stack.is_empty()
            && let Some(parent) = metadata.parent_deprecated
            && let Some(branch_point) = metadata.branch_point_deprecated
        {
            stack.push(BranchPoint {
                branch: parent.into(),
                revision: branch_point.into(),
            });
        }
        Self {
            id: metadata.id.into(),
            name: metadata.name.clone(),
            category: metadata.category.clone(),
            latest: metadata.latest.into(),
            creator: metadata.creator,
            created: metadata.created,
            stack,
        }
    }
}

// lore.model.v1 conversions

impl From<&BranchPoint> for crate::lore::model::v1::BranchPoint {
    fn from(point: &BranchPoint) -> Self {
        Self {
            branch_id: point.branch.into(),
            revision_signature: point.revision.into(),
        }
    }
}

impl From<crate::lore::model::v1::BranchPoint> for BranchPoint {
    fn from(point: crate::lore::model::v1::BranchPoint) -> Self {
        Self {
            branch: point.branch_id.into(),
            revision: point.revision_signature.into(),
        }
    }
}

impl From<Address> for crate::lore::model::v1::Address {
    fn from(address: Address) -> Self {
        Self {
            hash: address.hash.into(),
            context: address.context.into(),
        }
    }
}

impl From<&Address> for crate::lore::model::v1::Address {
    fn from(address: &Address) -> Self {
        Self {
            hash: address.hash.into(),
            context: address.context.into(),
        }
    }
}

impl From<crate::lore::model::v1::Address> for Address {
    fn from(address: crate::lore::model::v1::Address) -> Self {
        Self {
            hash: Hash::from(address.hash),
            context: Context::from(address.context),
        }
    }
}

impl From<&crate::lore::model::v1::Address> for Address {
    fn from(address: &crate::lore::model::v1::Address) -> Self {
        Self {
            hash: address.hash.as_ref().into(),
            context: address.context.as_ref().into(),
        }
    }
}

impl From<Fragment> for crate::lore::model::v1::Fragment {
    fn from(fragment: Fragment) -> Self {
        Self {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

impl From<&Fragment> for crate::lore::model::v1::Fragment {
    fn from(fragment: &Fragment) -> Self {
        Self {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

impl From<crate::lore::model::v1::Fragment> for Fragment {
    fn from(fragment: crate::lore::model::v1::Fragment) -> Self {
        Self {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

impl From<&crate::lore::model::v1::Fragment> for Fragment {
    fn from(fragment: &crate::lore::model::v1::Fragment) -> Self {
        Self {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

// Lock type conversions

impl From<LockData> for crate::lock::Lock {
    fn from(lock: LockData) -> Self {
        Self {
            resource: Some(crate::lock::Resource {
                branch: lock.resource.branch.into(),
                hash: lock.resource.hash.into(),
                description: lock.resource.description,
            }),
            owner: lock.owner,
            locked_at: Some(prost_types::Timestamp {
                seconds: (lock.locked_at / 1000) as i64,
                nanos: ((lock.locked_at % 1_000) * 1_000_000) as i32,
            }),
        }
    }
}

impl From<&LockData> for crate::lock::Lock {
    fn from(lock: &LockData) -> Self {
        Self {
            resource: Some(crate::lock::Resource {
                branch: lock.resource.branch.into(),
                hash: lock.resource.hash.into(),
                description: lock.resource.description.clone(),
            }),
            owner: lock.owner.clone(),
            locked_at: Some(prost_types::Timestamp {
                seconds: (lock.locked_at / 1000) as i64,
                nanos: ((lock.locked_at % 1_000) * 1_000_000) as i32,
            }),
        }
    }
}

impl From<crate::lock::Lock> for LockData {
    fn from(lock: crate::lock::Lock) -> Self {
        let resource = lock.resource.unwrap_or_default();
        Self {
            resource: LockResource {
                branch: resource.branch.into(),
                hash: resource.hash.into(),
                description: resource.description,
            },
            owner: lock.owner,
            locked_at: lock
                .locked_at
                .map(|t| (t.seconds as u64 * 1000) + (t.nanos as u64 / 1_000_000))
                .unwrap_or_default(),
        }
    }
}

impl From<&crate::lock::Lock> for LockData {
    fn from(lock: &crate::lock::Lock) -> Self {
        let resource = lock
            .resource
            .as_ref()
            .map(|resource| LockResource {
                branch: resource.branch.as_ref().into(),
                hash: resource.hash.as_ref().into(),
                description: resource.description.clone(),
            })
            .unwrap_or_default();
        Self {
            resource,
            owner: lock.owner.clone(),
            locked_at: lock
                .locked_at
                .map(|t| (t.seconds as u64 * 1000) + (t.nanos as u64 / 1_000_000))
                .unwrap_or_default(),
        }
    }
}

impl From<LockResource> for crate::lock::Resource {
    fn from(resource: LockResource) -> Self {
        Self {
            branch: resource.branch.into(),
            hash: resource.hash.into(),
            description: resource.description,
        }
    }
}

impl From<&LockResource> for crate::lock::Resource {
    fn from(resource: &LockResource) -> Self {
        Self {
            branch: resource.branch.into(),
            hash: resource.hash.into(),
            description: resource.description.clone(),
        }
    }
}

impl From<crate::lock::Resource> for LockResource {
    fn from(resource: crate::lock::Resource) -> Self {
        Self {
            branch: resource.branch.into(),
            hash: resource.hash.into(),
            description: resource.description,
        }
    }
}

impl From<&crate::lock::Resource> for LockResource {
    fn from(resource: &crate::lock::Resource) -> Self {
        Self {
            branch: BranchId::from(resource.branch.as_ref()),
            hash: Hash::from(resource.hash.as_ref()),
            description: resource.description.clone(),
        }
    }
}
