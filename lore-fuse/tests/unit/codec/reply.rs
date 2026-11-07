// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::mem::size_of;

use lore_fuse::abi::AttrOut;
use lore_fuse::abi::CreateOut;
use lore_fuse::abi::EntryOut;
use lore_fuse::abi::GetattrIn;
use lore_fuse::abi::InitOut;
use lore_fuse::abi::OpenOut;
use lore_fuse::abi::OutHeader;
use lore_fuse::abi::WriteOut;
use lore_fuse::codec::reply::MAX_INLINE_BODY;
use lore_fuse::codec::reply::ReplyHeaderBuf;
use lore_fuse::codec::reply::encode_data;
use lore_fuse::codec::reply::encode_error;
use lore_fuse::codec::reply::encode_struct;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

#[test]
fn error_reply_is_header_only_with_negative_errno() {
    let mut buf = ReplyHeaderBuf::new();
    let iov = encode_error(5, 2, &mut buf);

    assert_eq!(iov[0].len(), size_of::<OutHeader>());
    assert_eq!(buf.header.error, -2);
    assert_eq!(buf.header.unique, 5);
    assert_eq!(buf.header.len as usize, size_of::<OutHeader>());
}

#[test]
fn struct_reply_len_is_header_plus_body() {
    let mut buf = ReplyHeaderBuf::new();
    let body = GetattrIn {
        getattr_flags: 1,
        dummy: 0,
        fh: 9,
    };
    let iov = encode_struct(11, &body, &mut buf);

    let expected = size_of::<OutHeader>() + size_of::<GetattrIn>();
    assert_eq!(iov[0].len(), expected);
    assert_eq!(buf.header.len as usize, expected);
    assert_eq!(&buf.body[..size_of::<GetattrIn>()], body.as_bytes());
}

#[test]
fn data_reply_second_iov_points_at_payload() {
    let mut buf = ReplyHeaderBuf::new();
    let payload = b"hello, kernel";
    let iov = encode_data(13, &mut buf, payload);

    assert_eq!(iov[0].len(), size_of::<OutHeader>());
    assert_eq!(iov[1].as_ptr(), payload.as_ptr());
    assert_eq!(iov[1].len(), payload.len());
}

#[test]
fn create_reply_round_trips_entry_then_open() {
    let mut buf = ReplyHeaderBuf::new();
    let body = CreateOut {
        entry: EntryOut {
            nodeid: 5,
            entry_valid: 7,
            ..EntryOut::default()
        },
        open: OpenOut {
            fh: 9,
            open_flags: 2,
            backing_id: 0,
        },
    };
    let iov = encode_struct(3, &body, &mut buf);

    let expected = size_of::<OutHeader>() + size_of::<CreateOut>();
    assert_eq!(iov[0].len(), expected);
    let (header, rest) = OutHeader::ref_from_prefix(&iov[0]).expect("header");
    assert_eq!(header.len as usize, expected);
    assert_eq!(header.error, 0);
    assert_eq!(header.unique, 3);
    let (decoded, _) = CreateOut::ref_from_prefix(rest).expect("create body");
    assert_eq!(decoded.entry.nodeid, 5);
    assert_eq!(decoded.entry.entry_valid, 7);
    assert_eq!(decoded.open.fh, 9);
    assert_eq!(decoded.open.open_flags, 2);
}

#[test]
fn write_reply_round_trips_size() {
    let mut buf = ReplyHeaderBuf::new();
    let iov = encode_struct(
        4,
        &WriteOut {
            size: 5,
            padding: 0,
        },
        &mut buf,
    );

    let (header, rest) = OutHeader::ref_from_prefix(&iov[0]).expect("header");
    assert_eq!(
        header.len as usize,
        size_of::<OutHeader>() + size_of::<WriteOut>()
    );
    let (decoded, _) = WriteOut::ref_from_prefix(rest).expect("write body");
    assert_eq!(decoded.size, 5);
}

#[test]
fn inline_body_holds_the_largest_fixed_reply() {
    assert!(MAX_INLINE_BODY >= size_of::<CreateOut>());
    assert!(MAX_INLINE_BODY >= size_of::<InitOut>());
    assert!(MAX_INLINE_BODY >= size_of::<AttrOut>());
}
