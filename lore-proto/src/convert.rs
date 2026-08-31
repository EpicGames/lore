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
        crate::model::Address {
            hash: address.hash.into(),
            context: address.context.into(),
        }
    }
}

impl From<&Address> for crate::model::Address {
    fn from(address: &Address) -> Self {
        crate::model::Address {
            hash: address.hash.into(),
            context: address.context.into(),
        }
    }
}

impl From<crate::model::Address> for Address {
    fn from(address: crate::model::Address) -> Self {
        Address {
            hash: Hash::from(address.hash),
            context: Context::from(address.context),
        }
    }
}

impl From<&crate::model::Address> for Address {
    fn from(address: &crate::model::Address) -> Self {
        Address {
            hash: address.hash.as_ref().into(),
            context: address.context.as_ref().into(),
        }
    }
}

impl From<Fragment> for crate::model::Fragment {
    fn from(fragment: Fragment) -> Self {
        crate::model::Fragment {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

impl From<&Fragment> for crate::model::Fragment {
    fn from(fragment: &Fragment) -> Self {
        crate::model::Fragment {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

impl From<crate::model::Fragment> for Fragment {
    fn from(fragment: crate::model::Fragment) -> Self {
        Fragment {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

impl From<&crate::model::Fragment> for Fragment {
    fn from(fragment: &crate::model::Fragment) -> Self {
        Fragment {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

// Branch type conversions

impl From<&BranchPoint> for crate::model::BranchPoint {
    fn from(point: &BranchPoint) -> Self {
        crate::model::BranchPoint {
            branch: point.branch.into(),
            revision: point.revision.into(),
        }
    }
}

impl From<crate::model::BranchPoint> for BranchPoint {
    fn from(point: crate::model::BranchPoint) -> Self {
        BranchPoint {
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

        crate::model::Branch {
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
        BranchMetadata {
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
        crate::lore::model::v1::BranchPoint {
            branch_id: point.branch.into(),
            revision_signature: point.revision.into(),
        }
    }
}

impl From<crate::lore::model::v1::BranchPoint> for BranchPoint {
    fn from(point: crate::lore::model::v1::BranchPoint) -> Self {
        BranchPoint {
            branch: point.branch_id.into(),
            revision: point.revision_signature.into(),
        }
    }
}

impl From<Address> for crate::lore::model::v1::Address {
    fn from(address: Address) -> Self {
        crate::lore::model::v1::Address {
            hash: address.hash.into(),
            context: address.context.into(),
        }
    }
}

impl From<&Address> for crate::lore::model::v1::Address {
    fn from(address: &Address) -> Self {
        crate::lore::model::v1::Address {
            hash: address.hash.into(),
            context: address.context.into(),
        }
    }
}

impl From<crate::lore::model::v1::Address> for Address {
    fn from(address: crate::lore::model::v1::Address) -> Self {
        Address {
            hash: Hash::from(address.hash),
            context: Context::from(address.context),
        }
    }
}

impl From<&crate::lore::model::v1::Address> for Address {
    fn from(address: &crate::lore::model::v1::Address) -> Self {
        Address {
            hash: address.hash.as_ref().into(),
            context: address.context.as_ref().into(),
        }
    }
}

impl From<Fragment> for crate::lore::model::v1::Fragment {
    fn from(fragment: Fragment) -> Self {
        crate::lore::model::v1::Fragment {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

impl From<&Fragment> for crate::lore::model::v1::Fragment {
    fn from(fragment: &Fragment) -> Self {
        crate::lore::model::v1::Fragment {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

impl From<crate::lore::model::v1::Fragment> for Fragment {
    fn from(fragment: crate::lore::model::v1::Fragment) -> Self {
        Fragment {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

impl From<&crate::lore::model::v1::Fragment> for Fragment {
    fn from(fragment: &crate::lore::model::v1::Fragment) -> Self {
        Fragment {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        }
    }
}

// Lock type conversions

impl From<LockData> for crate::lock::Lock {
    fn from(lock: LockData) -> Self {
        crate::lock::Lock {
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
        crate::lock::Lock {
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
        LockData {
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
        LockData {
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
        crate::lock::Resource {
            branch: resource.branch.into(),
            hash: resource.hash.into(),
            description: resource.description,
        }
    }
}

impl From<&LockResource> for crate::lock::Resource {
    fn from(resource: &LockResource) -> Self {
        crate::lock::Resource {
            branch: resource.branch.into(),
            hash: resource.hash.into(),
            description: resource.description.clone(),
        }
    }
}

impl From<crate::lock::Resource> for LockResource {
    fn from(resource: crate::lock::Resource) -> Self {
        LockResource {
            branch: resource.branch.into(),
            hash: resource.hash.into(),
            description: resource.description,
        }
    }
}

impl From<&crate::lock::Resource> for LockResource {
    fn from(resource: &crate::lock::Resource) -> Self {
        LockResource {
            branch: BranchId::from(resource.branch.as_ref()),
            hash: Hash::from(resource.hash.as_ref()),
            description: resource.description.clone(),
        }
    }
}

impl crate::lore::model::v1::ItemStatus {
    /// The success value, for responses whose outcome is carried by the status itself.
    pub fn ok() -> Self {
        Self {
            code: i32::from(tonic::Code::Ok) as u32,
            message: String::new(),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.code == i32::from(tonic::Code::Ok) as u32
    }
}

/// Streaming storage handlers build a per-item failure as a `tonic::Status` — that
/// keeps the existing `MessageHandleError` mapping and server-error logging intact —
/// then convert here to put it on the wire in-band. `details` and metadata are
/// dropped deliberately: the address that used to be smuggled through `details` is
/// now a field on the response message itself.
impl From<&tonic::Status> for crate::lore::model::v1::ItemStatus {
    fn from(status: &tonic::Status) -> Self {
        Self {
            code: i32::from(status.code()) as u32,
            message: status.message().to_string(),
        }
    }
}

/// Rebuild a `Status` from an in-band item error so both sides can keep using the
/// established `Status` conversions — `ProtocolError::from` on the client, status-code
/// logging and metrics on the server. An unrecognised code decodes to `Code::Unknown`.
impl From<&crate::lore::model::v1::ItemStatus> for tonic::Status {
    fn from(item: &crate::lore::model::v1::ItemStatus) -> Self {
        tonic::Status::new(tonic::Code::from(item.code as i32), item.message.clone())
    }
}
