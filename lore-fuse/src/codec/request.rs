// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Decodes a kernel request buffer into a borrowed [`Decoded`] view, without copying.

use std::mem::size_of;

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::KnownLayout;

use crate::abi::BatchForgetIn;
use crate::abi::CreateIn;
use crate::abi::FallocateIn;
use crate::abi::FlushIn;
use crate::abi::ForgetIn;
use crate::abi::ForgetOne;
use crate::abi::FsyncIn;
use crate::abi::GetattrIn;
use crate::abi::InHeader;
use crate::abi::InitIn;
use crate::abi::InterruptIn;
use crate::abi::LinkIn;
use crate::abi::MkdirIn;
use crate::abi::MknodIn;
use crate::abi::Opcode;
use crate::abi::OpenIn;
use crate::abi::ReadIn;
use crate::abi::ReleaseIn;
use crate::abi::Rename2In;
use crate::abi::RenameIn;
use crate::abi::SetattrIn;
use crate::abi::WriteIn;

/// Longest name a request may carry, excluding its NUL.
const NAME_MAX: usize = 255;
/// Longest symlink target a request may carry, excluding its NUL.
const SYMLINK_TARGET_MAX: usize = libc::PATH_MAX as usize - 1;

/// Errors that can occur while decoding a request buffer.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("message too short to contain a valid header or body")]
    Truncated,
    #[error("declared message length does not match the buffer received")]
    LengthMismatch,
    #[error("name is malformed: {0}")]
    BadName(&'static str),
    #[error("request body is malformed: {0}")]
    BadBody(&'static str),
}

/// A decoded request, borrowing directly from the buffer it was decoded from.
///
/// Not `PartialEq`: several ABI structs it borrows (e.g. [`InitIn`]) intentionally do not
/// derive it, since equality is not otherwise a meaningful operation on a wire struct.
#[derive(Debug)]
pub enum Request<'a> {
    Init(&'a InitIn),
    Lookup {
        name: &'a [u8],
    },
    Forget(&'a ForgetIn),
    BatchForget(&'a [ForgetOne]),
    Getattr(&'a GetattrIn),
    Open(&'a OpenIn),
    Read(&'a ReadIn),
    Release(&'a ReleaseIn),
    Opendir(&'a OpenIn),
    Readdir(&'a ReadIn),
    Readdirplus(&'a ReadIn),
    Releasedir(&'a ReleaseIn),
    Statfs,
    Interrupt(&'a InterruptIn),
    Destroy,
    Setattr(&'a SetattrIn),
    Readlink,
    Symlink {
        name: &'a [u8],
        target: &'a [u8],
    },
    Mknod {
        body: &'a MknodIn,
        name: &'a [u8],
    },
    Mkdir {
        body: &'a MkdirIn,
        name: &'a [u8],
    },
    Unlink {
        name: &'a [u8],
    },
    Rmdir {
        name: &'a [u8],
    },
    /// `RENAME` and `RENAME2`; `RENAME` carries no flags and decodes with `flags` 0.
    Rename {
        newdir: u64,
        flags: u32,
        name: &'a [u8],
        newname: &'a [u8],
    },
    Link {
        body: &'a LinkIn,
        name: &'a [u8],
    },
    Create {
        body: &'a CreateIn,
        name: &'a [u8],
    },
    /// `data` is the payload, borrowed from the request buffer.
    Write {
        body: &'a WriteIn,
        data: &'a [u8],
    },
    Flush(&'a FlushIn),
    Fsync(&'a FsyncIn),
    Fsyncdir(&'a FsyncIn),
    Fallocate(&'a FallocateIn),
    Unsupported(Opcode),
    Unknown(u32),
}

/// A decoded request, paired with the header it arrived under.
#[derive(Debug)]
pub struct Decoded<'a> {
    pub header: &'a InHeader,
    pub request: Request<'a>,
}

/// Decodes `buffer` into a [`Decoded`] view that borrows from it, performing no allocation
/// and no copy of any field.
pub fn decode(buffer: &[u8]) -> Result<Decoded<'_>, DecodeError> {
    let (header, rest) =
        InHeader::ref_from_prefix(buffer).map_err(|_err| DecodeError::Truncated)?;

    if header.len as usize != buffer.len() {
        return Err(DecodeError::LengthMismatch);
    }

    let request = match Opcode::try_from(header.opcode) {
        Ok(Opcode::Init) => Request::Init(body_ref(rest)?),
        Ok(Opcode::Lookup) => Request::Lookup {
            name: parse_name(rest)?,
        },
        Ok(Opcode::Forget) => Request::Forget(body_ref(rest)?),
        Ok(Opcode::BatchForget) => Request::BatchForget(parse_batch_forget(rest)?),
        Ok(Opcode::Getattr) => Request::Getattr(body_ref(rest)?),
        Ok(Opcode::Open) => Request::Open(body_ref(rest)?),
        Ok(Opcode::Read) => Request::Read(body_ref(rest)?),
        Ok(Opcode::Release) => Request::Release(body_ref(rest)?),
        Ok(Opcode::Opendir) => Request::Opendir(body_ref(rest)?),
        Ok(Opcode::Readdir) => Request::Readdir(body_ref(rest)?),
        Ok(Opcode::Readdirplus) => Request::Readdirplus(body_ref(rest)?),
        Ok(Opcode::Releasedir) => Request::Releasedir(body_ref(rest)?),
        Ok(Opcode::Statfs) => Request::Statfs,
        Ok(Opcode::Interrupt) => Request::Interrupt(body_ref(rest)?),
        Ok(Opcode::Destroy) => Request::Destroy,
        Ok(Opcode::Setattr) => Request::Setattr(body_ref(rest)?),
        Ok(Opcode::Readlink) => Request::Readlink,
        Ok(Opcode::Symlink) => {
            let (name, tail) = split_name(rest, NAME_MAX)?;
            let (target, _) = split_name(tail, SYMLINK_TARGET_MAX)?;
            Request::Symlink { name, target }
        }
        Ok(Opcode::Mknod) => {
            let (body, tail) = body_and_tail(rest)?;
            Request::Mknod {
                body,
                name: parse_name(tail)?,
            }
        }
        Ok(Opcode::Mkdir) => {
            let (body, tail) = body_and_tail(rest)?;
            Request::Mkdir {
                body,
                name: parse_name(tail)?,
            }
        }
        Ok(Opcode::Unlink) => Request::Unlink {
            name: parse_name(rest)?,
        },
        Ok(Opcode::Rmdir) => Request::Rmdir {
            name: parse_name(rest)?,
        },
        Ok(Opcode::Rename) => {
            let (body, tail) = body_and_tail::<RenameIn>(rest)?;
            let (name, newname) = parse_two_names(tail)?;
            Request::Rename {
                newdir: body.newdir,
                flags: 0,
                name,
                newname,
            }
        }
        Ok(Opcode::Rename2) => {
            let (body, tail) = body_and_tail::<Rename2In>(rest)?;
            let (name, newname) = parse_two_names(tail)?;
            Request::Rename {
                newdir: body.newdir,
                flags: body.flags,
                name,
                newname,
            }
        }
        Ok(Opcode::Link) => {
            let (body, tail) = body_and_tail(rest)?;
            Request::Link {
                body,
                name: parse_name(tail)?,
            }
        }
        Ok(Opcode::Create) => {
            let (body, tail) = body_and_tail(rest)?;
            Request::Create {
                body,
                name: parse_name(tail)?,
            }
        }
        Ok(Opcode::Write) => {
            let (body, data) = parse_write(rest)?;
            Request::Write { body, data }
        }
        Ok(Opcode::Flush) => Request::Flush(body_ref(rest)?),
        Ok(Opcode::Fsync) => Request::Fsync(body_ref(rest)?),
        Ok(Opcode::Fsyncdir) => Request::Fsyncdir(body_ref(rest)?),
        Ok(Opcode::Fallocate) => Request::Fallocate(body_ref(rest)?),
        Ok(other) => Request::Unsupported(other),
        Err(raw) => Request::Unknown(raw),
    };

    Ok(Decoded { header, request })
}

fn body_ref<T>(rest: &[u8]) -> Result<&T, DecodeError>
where
    T: FromBytes + KnownLayout + Immutable,
{
    body_and_tail(rest).map(|(value, _)| value)
}

/// Reads a fixed-size body from the front of `rest`, returning it and the bytes after it.
fn body_and_tail<T>(rest: &[u8]) -> Result<(&T, &[u8]), DecodeError>
where
    T: FromBytes + KnownLayout + Immutable,
{
    T::ref_from_prefix(rest)
        .map_err(|_err| DecodeError::BadBody("buffer too short for expected body"))
}

/// Reads a NUL-terminated name from `rest`, returning it without the terminator.
fn parse_name(rest: &[u8]) -> Result<&[u8], DecodeError> {
    split_name(rest, NAME_MAX).map(|(name, _)| name)
}

/// Reads two consecutive NUL-terminated names, as `RENAME` and `RENAME2` carry.
fn parse_two_names(rest: &[u8]) -> Result<(&[u8], &[u8]), DecodeError> {
    let (first, tail) = split_name(rest, NAME_MAX)?;
    Ok((first, parse_name(tail)?))
}

/// Reads a NUL-terminated string of at most `max` bytes, returning it without the terminator
/// and the bytes after the terminator.
fn split_name(rest: &[u8], max: usize) -> Result<(&[u8], &[u8]), DecodeError> {
    let nul_at = rest
        .iter()
        .position(|&byte| byte == 0)
        .ok_or(DecodeError::BadName("name is not NUL-terminated"))?;
    if nul_at > max {
        return Err(DecodeError::BadName("name is longer than its limit"));
    }
    Ok((&rest[..nul_at], &rest[nul_at + 1..]))
}

/// Reads a `WRITE` body and its `size`-byte payload, which must lie within `rest`.
fn parse_write(rest: &[u8]) -> Result<(&WriteIn, &[u8]), DecodeError> {
    let (body, tail) = body_and_tail::<WriteIn>(rest)?;
    let data = tail.get(..body.size as usize).ok_or(DecodeError::BadBody(
        "write payload is shorter than its size",
    ))?;
    Ok((body, data))
}

/// Reads a `BATCH_FORGET` body, clamping the kernel-supplied count to what the buffer can
/// actually hold so a lying `count` field cannot read past the end of `rest`.
fn parse_batch_forget(rest: &[u8]) -> Result<&[ForgetOne], DecodeError> {
    let (head, tail) = BatchForgetIn::ref_from_prefix(rest)
        .map_err(|_err| DecodeError::BadBody("buffer too short for batch forget header"))?;
    let max_entries = tail.len() / size_of::<ForgetOne>();
    let count = (head.count as usize).min(max_entries);
    let (entries, _) = <[ForgetOne]>::ref_from_prefix_with_elems(tail, count)
        .map_err(|_err| DecodeError::BadBody("batch forget entries do not fit"))?;
    Ok(entries)
}
