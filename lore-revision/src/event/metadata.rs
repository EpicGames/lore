// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use crate::event;
use crate::event::LoreMetadataEventData;
use crate::metadata::Metadata;
use crate::metadata::MetadataType;

pub(crate) fn send(metadata: &Metadata) {
    let mut entries = vec![];

    metadata.walk(
        |key_slice: &[u8], value_slice: &[u8], value_type: MetadataType| {
            if let Ok(key) = std::str::from_utf8(key_slice)
                && let Ok(entry) = LoreMetadataEventData::new(key, value_slice, value_type)
            {
                entries.push(entry);
            }
        },
    );

    for entry in entries {
        event::LoreEvent::Metadata(entry).send();
    }
}

pub(crate) fn send_keyed(metadata: &Metadata, key: &str) {
    let mut entries = vec![];

    // TODO: Need to be able to get an item from Metadata and also know its type!
    metadata.walk(
        |key_slice: &[u8], value_slice: &[u8], value_type: MetadataType| {
            if let Ok(stored_key) = std::str::from_utf8(key_slice)
                && stored_key == key
                && let Ok(entry) = LoreMetadataEventData::new(key, value_slice, value_type)
            {
                entries.push(entry);
            }
        },
    );

    for entry in entries {
        event::LoreEvent::Metadata(entry).send();
    }
}
