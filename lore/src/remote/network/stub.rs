// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use crate::remote::network::UdsAcceptError;
use crate::remote::network::UdsConnectionError;
use crate::remote::network::UdsListenerError;

pub fn uds_supported() -> bool {
    false
}

pub struct UdsListener {}

impl UdsListener {
    pub fn new() -> Result<Self, UdsListenerError> {
        panic!("Networking not supported on this OS")
    }

    pub fn accept(&self) -> Result<UdsStream, UdsAcceptError> {
        panic!("Networking not supported on this OS")
    }
}

pub struct UdsStream {}

impl UdsStream {
    #[allow(unreachable_code)]
    pub fn writer(&mut self) -> &mut impl std::io::Write {
        panic!("Networking not supported on this OS");
        Box::leak(Box::<Vec<u8>>::new(Vec::new()))
    }

    #[allow(unreachable_code)]
    pub fn reader(&mut self) -> &mut impl std::io::Read {
        panic!("Networking not supported on this OS");
        &mut Box::leak(Box::<Vec<u8>>::new(Vec::new())).as_slice()
    }

    pub fn try_clone(&self) -> std::io::Result<Self> {
        panic!("Networking not supported on this OS")
    }

    pub fn connect() -> Result<Self, UdsConnectionError> {
        panic!("Networking not supported on this OS")
    }
}
