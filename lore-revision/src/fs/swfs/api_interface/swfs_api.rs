//! Rust bindings for the SWFS C API from swfs.h
//! If cfg(feature = "swfs") this will include the whole header.
//! If cfg(not(feature = "swfs")) this will only include the data types, and thus not require the
//! swfs library to be linked.
//! `generated/swfs_api_full.rs` should be generated with the command
//! bindgen -o lore-revision/src/fs/swfs/api_interface/generated/swfs_api_full.rs swfs.h -- -x c++
//! `generated/swfs_api_types.rs` should be generated with the command
//! bindgen -o lore-revision/src/fs/swfs/api_interface/generated/swfs_api_types.rs
//!     --ignore-methods --ignore-functions swfs.h -- -x c++

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]

#[cfg(feature = "swfs")]
include!("generated/swfs_api_full.rs");
#[cfg(not(feature = "swfs"))]
include!("generated/swfs_api_types.rs");
