// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Generated protobuf/gRPC bindings for the Bazel Remote Execution API v2, ByteStream, and the
//! internal scheduler <-> worker protocol.
//!
//! Kept in its own crate so the server and the worker share one copy of the generated code.
//!
//! The module tree below mirrors the proto package names exactly. That is not cosmetic:
//! prost refers to types from other packages by relative `super::` paths, so a message in
//! `lore.rbe.worker.v1` that holds a `build.bazel.remote.execution.v2.Digest` only resolves if
//! both sit at their proper depth under the crate root. Flattening these into one module per
//! package breaks every cross-package reference. The short aliases at the bottom are what the
//! rest of the code actually uses.

// The generated doc comments are the protos' own comments, copied as their authors wrapped them.
#![allow(clippy::doc_lazy_continuation)]

pub mod build {
    pub mod bazel {
        pub mod remote {
            pub mod execution {
                pub mod v2 {
                    tonic::include_proto!("build.bazel.remote.execution.v2");
                }
            }
        }
        pub mod semver {
            tonic::include_proto!("build.bazel.semver");
        }
    }
}

pub mod google {
    pub mod bytestream {
        tonic::include_proto!("google.bytestream");
    }
    pub mod longrunning {
        tonic::include_proto!("google.longrunning");
    }
    pub mod rpc {
        tonic::include_proto!("google.rpc");
    }
}

pub mod lore {
    pub mod rbe {
        pub mod worker {
            pub mod v1 {
                tonic::include_proto!("lore.rbe.worker.v1");
            }
        }
    }
}

pub use build::bazel::remote::execution::v2 as reapi;
pub use build::bazel::semver;
pub use google::bytestream;
pub use google::longrunning;
pub use google::rpc;
pub use lore::rbe::worker::v1 as worker;

/// Type URLs for the `Any`-wrapped payloads in `google.longrunning.Operation`.
pub mod type_url {
    pub const EXECUTE_OPERATION_METADATA: &str =
        "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteOperationMetadata";
    pub const EXECUTE_RESPONSE: &str =
        "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse";
}
