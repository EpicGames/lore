// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::mem::size_of;

use lore_fuse::abi::CreateIn;
use lore_fuse::abi::FallocateIn;
use lore_fuse::abi::FlushIn;
use lore_fuse::abi::ForgetOne;
use lore_fuse::abi::FsyncIn;
use lore_fuse::abi::InHeader;
use lore_fuse::abi::LinkIn;
use lore_fuse::abi::MkdirIn;
use lore_fuse::abi::MknodIn;
use lore_fuse::abi::Opcode;
use lore_fuse::abi::Rename2In;
use lore_fuse::abi::RenameIn;
use lore_fuse::abi::SetattrIn;
use lore_fuse::abi::WriteIn;
use lore_fuse::codec::request::DecodeError;
use lore_fuse::codec::request::Request;
use lore_fuse::codec::request::decode;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

fn header_and_body(opcode: u32, body: &[u8]) -> Vec<u8> {
    let header = InHeader {
        len: (size_of::<InHeader>() + body.len()) as u32,
        opcode,
        unique: 7,
        nodeid: 1,
        uid: 0,
        gid: 0,
        pid: 0,
        total_extlen: 0,
        padding: 0,
    };
    let mut buffer = header.as_bytes().to_vec();
    buffer.extend_from_slice(body);
    buffer
}

#[test]
fn decodes_lookup_name_without_nul() {
    let buffer = header_and_body(Opcode::Lookup as u32, b"foo\0");
    let decoded = decode(&buffer).expect("valid lookup request");

    match decoded.request {
        Request::Lookup { name } => assert_eq!(name, b"foo"),
        other => panic!("expected Lookup, got {other:?}"),
    }
}

#[test]
fn rejects_truncated_header() {
    let buffer = vec![0_u8; 10];
    let error = decode(&buffer).expect_err("header is too short to decode");
    assert!(matches!(error, DecodeError::Truncated));
}

#[test]
fn rejects_len_mismatch() {
    let mut buffer = header_and_body(Opcode::Getattr as u32, &[0_u8; 16]);
    // Claim a length one byte longer than what was actually sent.
    let claimed_len = buffer.len() as u32 + 1;
    buffer[0..4].copy_from_slice(&claimed_len.to_ne_bytes());

    let error = decode(&buffer).expect_err("declared len does not match buffer");
    assert!(matches!(error, DecodeError::LengthMismatch));
}

#[test]
fn rejects_unterminated_lookup_name() {
    let buffer = header_and_body(Opcode::Lookup as u32, b"no-nul-here");
    let error = decode(&buffer).expect_err("name has no NUL terminator");
    assert!(matches!(error, DecodeError::BadName(_)));
}

#[test]
fn rejects_name_over_255() {
    let mut name = vec![b'a'; 256];
    name.push(0);
    let buffer = header_and_body(Opcode::Lookup as u32, &name);

    let error = decode(&buffer).expect_err("name exceeds 255 bytes");
    assert!(matches!(error, DecodeError::BadName(_)));
}

#[test]
fn batch_forget_count_bounded_by_len() {
    // Header claims 1000 entries, but the buffer only has room for 2.
    let mut body = 1000_u32.to_ne_bytes().to_vec();
    body.extend_from_slice(&0_u32.to_ne_bytes()); // dummy
    for entry in [
        ForgetOne {
            nodeid: 1,
            nlookup: 1,
        },
        ForgetOne {
            nodeid: 2,
            nlookup: 2,
        },
    ] {
        body.extend_from_slice(entry.as_bytes());
    }
    let buffer = header_and_body(Opcode::BatchForget as u32, &body);

    let decoded = decode(&buffer).expect("bounded batch forget still decodes");
    match decoded.request {
        Request::BatchForget(entries) => assert_eq!(entries.len(), 2),
        other => panic!("expected BatchForget, got {other:?}"),
    }
}

#[test]
fn unknown_opcode_decodes_as_unknown() {
    let buffer = header_and_body(9_999, &[]);
    let decoded = decode(&buffer).expect("unknown opcode still decodes a header");

    match decoded.request {
        Request::Unknown(code) => assert_eq!(code, 9_999),
        other => panic!("expected Unknown, got {other:?}"),
    }
}

fn name_body<T: IntoBytes + Immutable>(head: &T, names: &[&[u8]]) -> Vec<u8> {
    let mut body = head.as_bytes().to_vec();
    for name in names {
        body.extend_from_slice(name);
        body.push(0);
    }
    body
}

fn names_only(names: &[&[u8]]) -> Vec<u8> {
    let mut body = Vec::new();
    for name in names {
        body.extend_from_slice(name);
        body.push(0);
    }
    body
}

#[test]
fn decodes_write_with_payload_borrowed_from_the_buffer() {
    let write = WriteIn {
        fh: 9,
        offset: 4096,
        size: 5,
        write_flags: 2,
        lock_owner: 3,
        flags: 2,
        padding: 0,
    };
    let mut body = write.as_bytes().to_vec();
    body.extend_from_slice(b"hello");
    let buffer = header_and_body(Opcode::Write as u32, &body);

    let decoded = decode(&buffer).expect("valid write request");
    match decoded.request {
        Request::Write { body, data } => {
            assert_eq!(body.fh, 9);
            assert_eq!(body.offset, 4096);
            assert_eq!(body.write_flags, 2);
            assert_eq!(body.lock_owner, 3);
            assert_eq!(data, b"hello");
            let payload_start = size_of::<InHeader>() + size_of::<WriteIn>();
            assert_eq!(data.as_ptr(), buffer[payload_start..].as_ptr());
        }
        other => panic!("expected Write, got {other:?}"),
    }
}

#[test]
fn write_payload_stops_at_size_before_trailing_bytes() {
    let write = WriteIn {
        size: 3,
        ..WriteIn::default()
    };
    let mut body = write.as_bytes().to_vec();
    body.extend_from_slice(b"abcdefgh");
    let buffer = header_and_body(Opcode::Write as u32, &body);

    match decode(&buffer).expect("valid write request").request {
        Request::Write { data, .. } => assert_eq!(data, b"abc"),
        other => panic!("expected Write, got {other:?}"),
    }
}

#[test]
fn rejects_write_whose_size_exceeds_the_payload() {
    let write = WriteIn {
        size: 10,
        ..WriteIn::default()
    };
    let mut body = write.as_bytes().to_vec();
    body.extend_from_slice(b"short");
    let buffer = header_and_body(Opcode::Write as u32, &body);

    let error = decode(&buffer).expect_err("size runs past the buffer");
    assert!(matches!(error, DecodeError::BadBody(_)));
}

#[test]
fn decodes_create_body_and_name() {
    let create = CreateIn {
        flags: (libc::O_CREAT | libc::O_RDWR) as u32,
        mode: libc::S_IFREG | 0o644,
        umask: 0o022,
        open_flags: 0,
    };
    let buffer = header_and_body(Opcode::Create as u32, &name_body(&create, &[b"new.txt"]));

    match decode(&buffer).expect("valid create request").request {
        Request::Create { body, name } => {
            assert_eq!(body.flags, create.flags);
            assert_eq!(body.mode, create.mode);
            assert_eq!(body.umask, 0o022);
            assert_eq!(name, b"new.txt");
        }
        other => panic!("expected Create, got {other:?}"),
    }
}

#[test]
fn decodes_mknod_body_and_name() {
    let mknod = MknodIn {
        mode: libc::S_IFIFO | 0o600,
        rdev: 7,
        umask: 0o077,
        padding: 0,
    };
    let buffer = header_and_body(Opcode::Mknod as u32, &name_body(&mknod, &[b"fifo"]));

    match decode(&buffer).expect("valid mknod request").request {
        Request::Mknod { body, name } => {
            assert_eq!(body.mode, mknod.mode);
            assert_eq!(body.rdev, 7);
            assert_eq!(body.umask, 0o077);
            assert_eq!(name, b"fifo");
        }
        other => panic!("expected Mknod, got {other:?}"),
    }
}

#[test]
fn decodes_mkdir_body_and_name() {
    let mkdir = MkdirIn {
        mode: 0o755,
        umask: 0o022,
    };
    let buffer = header_and_body(Opcode::Mkdir as u32, &name_body(&mkdir, &[b"sub"]));

    match decode(&buffer).expect("valid mkdir request").request {
        Request::Mkdir { body, name } => {
            assert_eq!(body.mode, 0o755);
            assert_eq!(body.umask, 0o022);
            assert_eq!(name, b"sub");
        }
        other => panic!("expected Mkdir, got {other:?}"),
    }
}

#[test]
fn decodes_unlink_and_rmdir_names() {
    let unlink = header_and_body(Opcode::Unlink as u32, &names_only(&[b"gone.txt"]));
    match decode(&unlink).expect("valid unlink request").request {
        Request::Unlink { name } => assert_eq!(name, b"gone.txt"),
        other => panic!("expected Unlink, got {other:?}"),
    }

    let rmdir = header_and_body(Opcode::Rmdir as u32, &names_only(&[b"empty"]));
    match decode(&rmdir).expect("valid rmdir request").request {
        Request::Rmdir { name } => assert_eq!(name, b"empty"),
        other => panic!("expected Rmdir, got {other:?}"),
    }
}

#[test]
fn decodes_rename_as_rename_with_zero_flags() {
    let rename = RenameIn { newdir: 5 };
    let buffer = header_and_body(
        Opcode::Rename as u32,
        &name_body(&rename, &[b"old", b"new"]),
    );

    match decode(&buffer).expect("valid rename request").request {
        Request::Rename {
            newdir,
            flags,
            name,
            newname,
        } => {
            assert_eq!(newdir, 5);
            assert_eq!(flags, 0);
            assert_eq!(name, b"old");
            assert_eq!(newname, b"new");
        }
        other => panic!("expected Rename, got {other:?}"),
    }
}

#[test]
fn decodes_rename2_flags_and_names() {
    let rename = Rename2In {
        newdir: 6,
        flags: libc::RENAME_NOREPLACE,
        padding: 0,
    };
    let buffer = header_and_body(
        Opcode::Rename2 as u32,
        &name_body(&rename, &[b"from", b"to"]),
    );

    match decode(&buffer).expect("valid rename2 request").request {
        Request::Rename {
            newdir,
            flags,
            name,
            newname,
        } => {
            assert_eq!(newdir, 6);
            assert_eq!(flags, libc::RENAME_NOREPLACE);
            assert_eq!(name, b"from");
            assert_eq!(newname, b"to");
        }
        other => panic!("expected Rename, got {other:?}"),
    }
}

#[test]
fn rejects_rename_whose_new_name_is_unterminated() {
    let mut body = name_body(&RenameIn { newdir: 5 }, &[b"old"]);
    body.extend_from_slice(b"new");
    let buffer = header_and_body(Opcode::Rename as u32, &body);

    let error = decode(&buffer).expect_err("second name has no NUL");
    assert!(matches!(error, DecodeError::BadName(_)));
}

#[test]
fn decodes_setattr_body() {
    let setattr = SetattrIn {
        valid: lore_fuse::abi::entry::setattr_valid::SIZE
            | lore_fuse::abi::entry::setattr_valid::FH,
        fh: 3,
        size: 1000,
        mode: 0o600,
        ..SetattrIn::default()
    };
    let buffer = header_and_body(Opcode::Setattr as u32, setattr.as_bytes());

    match decode(&buffer).expect("valid setattr request").request {
        Request::Setattr(body) => {
            assert_eq!(body.valid, setattr.valid);
            assert_eq!(body.fh, 3);
            assert_eq!(body.size, 1000);
            assert_eq!(body.mode, 0o600);
        }
        other => panic!("expected Setattr, got {other:?}"),
    }
}

#[test]
fn decodes_symlink_name_and_a_target_longer_than_a_name() {
    let target = vec![b't'; 1000];
    let buffer = header_and_body(Opcode::Symlink as u32, &names_only(&[b"link", &target]));

    match decode(&buffer).expect("valid symlink request").request {
        Request::Symlink {
            name,
            target: decoded,
        } => {
            assert_eq!(name, b"link");
            assert_eq!(decoded, target.as_slice());
        }
        other => panic!("expected Symlink, got {other:?}"),
    }
}

#[test]
fn rejects_symlink_target_of_path_max_bytes() {
    let target = vec![b't'; libc::PATH_MAX as usize];
    let buffer = header_and_body(Opcode::Symlink as u32, &names_only(&[b"link", &target]));

    let error = decode(&buffer).expect_err("target needs PATH_MAX + 1 bytes with its NUL");
    assert!(matches!(error, DecodeError::BadName(_)));
}

#[test]
fn decodes_readlink_without_body() {
    let buffer = header_and_body(Opcode::Readlink as u32, &[]);

    let decoded = decode(&buffer).expect("valid readlink request");
    assert!(matches!(decoded.request, Request::Readlink));
}

#[test]
fn decodes_link_body_and_name() {
    let link = LinkIn { oldnodeid: 7 };
    let buffer = header_and_body(Opcode::Link as u32, &name_body(&link, &[b"hard"]));

    match decode(&buffer).expect("valid link request").request {
        Request::Link { body, name } => {
            assert_eq!(body.oldnodeid, 7);
            assert_eq!(name, b"hard");
        }
        other => panic!("expected Link, got {other:?}"),
    }
}

#[test]
fn decodes_flush_fsync_fsyncdir_and_fallocate_bodies() {
    let flush = FlushIn {
        fh: 11,
        lock_owner: 12,
        ..FlushIn::default()
    };
    match decode(&header_and_body(Opcode::Flush as u32, flush.as_bytes()))
        .expect("valid flush request")
        .request
    {
        Request::Flush(body) => {
            assert_eq!(body.fh, 11);
            assert_eq!(body.lock_owner, 12);
        }
        other => panic!("expected Flush, got {other:?}"),
    }

    let fsync = FsyncIn {
        fh: 13,
        fsync_flags: 1,
        padding: 0,
    };
    match decode(&header_and_body(Opcode::Fsync as u32, fsync.as_bytes()))
        .expect("valid fsync request")
        .request
    {
        Request::Fsync(body) => {
            assert_eq!(body.fh, 13);
            assert_eq!(body.fsync_flags, 1);
        }
        other => panic!("expected Fsync, got {other:?}"),
    }
    match decode(&header_and_body(Opcode::Fsyncdir as u32, fsync.as_bytes()))
        .expect("valid fsyncdir request")
        .request
    {
        Request::Fsyncdir(body) => assert_eq!(body.fh, 13),
        other => panic!("expected Fsyncdir, got {other:?}"),
    }

    let fallocate = FallocateIn {
        fh: 14,
        offset: 15,
        length: 16,
        mode: 1,
        padding: 0,
    };
    match decode(&header_and_body(
        Opcode::Fallocate as u32,
        fallocate.as_bytes(),
    ))
    .expect("valid fallocate request")
    .request
    {
        Request::Fallocate(body) => {
            assert_eq!(body.fh, 14);
            assert_eq!(body.offset, 15);
            assert_eq!(body.length, 16);
            assert_eq!(body.mode, 1);
        }
        other => panic!("expected Fallocate, got {other:?}"),
    }
}

/// One well-formed request per write-family opcode.
fn write_family_requests() -> Vec<Vec<u8>> {
    let mut write = WriteIn {
        size: 4,
        ..WriteIn::default()
    }
    .as_bytes()
    .to_vec();
    write.extend_from_slice(b"data");
    vec![
        header_and_body(Opcode::Write as u32, &write),
        header_and_body(
            Opcode::Create as u32,
            &name_body(&CreateIn::default(), &[b"c"]),
        ),
        header_and_body(
            Opcode::Mknod as u32,
            &name_body(&MknodIn::default(), &[b"n"]),
        ),
        header_and_body(
            Opcode::Mkdir as u32,
            &name_body(&MkdirIn::default(), &[b"d"]),
        ),
        header_and_body(Opcode::Unlink as u32, &names_only(&[b"u"])),
        header_and_body(Opcode::Rmdir as u32, &names_only(&[b"r"])),
        header_and_body(
            Opcode::Rename as u32,
            &name_body(&RenameIn::default(), &[b"a", b"b"]),
        ),
        header_and_body(
            Opcode::Rename2 as u32,
            &name_body(&Rename2In::default(), &[b"a", b"b"]),
        ),
        header_and_body(Opcode::Setattr as u32, SetattrIn::default().as_bytes()),
        header_and_body(Opcode::Symlink as u32, &names_only(&[b"s", b"target"])),
        header_and_body(Opcode::Readlink as u32, &[]),
        header_and_body(Opcode::Link as u32, &name_body(&LinkIn::default(), &[b"l"])),
        header_and_body(Opcode::Flush as u32, FlushIn::default().as_bytes()),
        header_and_body(Opcode::Fsync as u32, FsyncIn::default().as_bytes()),
        header_and_body(Opcode::Fsyncdir as u32, FsyncIn::default().as_bytes()),
        header_and_body(Opcode::Fallocate as u32, FallocateIn::default().as_bytes()),
    ]
}

#[test]
fn rejects_every_write_family_request_cut_one_byte_short() {
    for base in write_family_requests() {
        if base.len() == size_of::<InHeader>() {
            continue;
        }
        let mut cut = base[..base.len() - 1].to_vec();
        let len = cut.len() as u32;
        cut[0..4].copy_from_slice(&len.to_ne_bytes());

        let opcode = u32::from_ne_bytes([base[4], base[5], base[6], base[7]]);
        let error = decode(&cut).expect_err("a request one byte short must not decode");
        assert!(
            matches!(error, DecodeError::BadBody(_)) || matches!(error, DecodeError::BadName(_)),
            "opcode {opcode}: {error:?}"
        );
    }
}

#[test]
fn random_mutations_of_write_family_requests_never_panic() {
    let bases = write_family_requests();
    for base in &bases {
        let decoded = decode(base).expect("every base request decodes");
        assert!(
            !matches!(
                decoded.request,
                Request::Unsupported(_) | Request::Unknown(_)
            ),
            "base request must reach a dedicated decoder: {:?}",
            decoded.request
        );
    }

    let mut state: u64 = 0x1319_8A2E_0370_7344;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..20_000 {
        let base = &bases[(next() as usize) % bases.len()];
        let mut mutated = base.clone();
        if next() % 4 == 0 {
            let keep = (next() as usize) % (mutated.len() + 1);
            mutated.truncate(keep);
            if mutated.len() >= 4 {
                let len = mutated.len() as u32;
                mutated[0..4].copy_from_slice(&len.to_ne_bytes());
            }
        }
        let body_start = size_of::<InHeader>().min(mutated.len());
        let body_len = mutated.len() - body_start;
        if body_len > 0 {
            for _ in 0..1 + (next() % 4) {
                let index = body_start + (next() as usize) % body_len;
                mutated[index] = (next() % 256) as u8;
            }
        }
        let _ = decode(&mutated);
    }
}

#[test]
fn random_mutations_never_panic() {
    let base = header_and_body(Opcode::Lookup as u32, b"foo\0");
    let mut state: u64 = 0x243F_6A88_85A3_08D3;
    let mut next = || {
        // xorshift64: deterministic, dependency-free PRNG for fuzz-style mutation.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for _ in 0..10_000 {
        let mut mutated = base.clone();
        let mutation_count = 1 + (next() % 4) as usize;
        for _ in 0..mutation_count {
            if mutated.is_empty() {
                break;
            }
            let index = (next() as usize) % mutated.len();
            mutated[index] = (next() % 256) as u8;
        }
        let _ = decode(&mutated);
    }
}
