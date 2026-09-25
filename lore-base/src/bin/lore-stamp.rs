// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Writes the build version into finished Lore artifacts.
//!
//! ```text
//! lore-stamp --build <name> <file>...
//! ```
//!
//! Writes the build name `<name>` into the version slot of each file, a binary, shared library or
//! static library that links `lore-base`. The file then reports `<package version>+<name>`, with
//! the package version it was built with. Each file is changed in place and keeps its size, and a
//! stamped file can be stamped again. No file is written unless every file holds exactly one slot.
//!
//! Stamp before signing: rewriting any byte invalidates a signature. On macOS, a file the linker
//! signed ad-hoc, as it signs every arm64 binary, is signed ad-hoc again once stamped, and a file
//! signed with an identity is refused. Other systems sign nothing, so a macOS artifact stamped
//! elsewhere must be signed on macOS before it runs.
use std::ffi::OsString;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;

use lore_base::version::STAMP_BUILD_CAPACITY;
use lore_base::version::STAMP_MARKER;
use thiserror::Error;

const USAGE: &str = "usage: lore-stamp --build <name> <file>...";

/// Size of the buffer an artifact is read through while its slot is searched for.
const READ_BUFFER_SIZE: usize = 1 << 20;

#[derive(Debug, Error)]
enum StampError {
    #[error("{USAGE}")]
    Usage,
    #[error(
        "build name {0:?} is empty or holds a character other than a letter, a digit or one of !#$%&'*+-.^_`|~"
    )]
    InvalidBuildName(String),
    #[error("build name {0:?} does not fit the {STAMP_BUILD_CAPACITY}-byte build name field")]
    BuildNameTooLong(String),
    #[error("no version slot found")]
    NoSlot,
    #[error("{0} version slots found, expected one")]
    SeveralSlots(usize),
    #[error("the version slot is cut off by the end of the file")]
    TruncatedSlot,
    #[error("already signed with an identity; stamp before signing, or remove the signature first")]
    SignedWithIdentity,
    #[error("codesign failed: {0}")]
    CodesignFailed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, PartialEq)]
struct Arguments {
    build: String,
    files: Vec<PathBuf>,
}

/// How to stamp one file, worked out before any file is written.
struct Plan {
    /// Where the build name field starts.
    offset: u64,
    /// Whether to sign the file ad-hoc again once it is stamped.
    resign: bool,
}

fn main() -> ExitCode {
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    if arguments
        .first()
        .is_some_and(|argument| argument == "--help" || argument == "-h")
    {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match run(arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("lore-stamp: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Stamps every file. No file is written unless every file holds exactly one slot and, on macOS,
/// none is signed with an identity.
fn run(arguments: Vec<OsString>) -> Result<(), String> {
    let arguments = parse_arguments(arguments).map_err(|error| error.to_string())?;
    check_build_name(&arguments.build).map_err(|error| error.to_string())?;

    let mut buffer = vec![0; READ_BUFFER_SIZE];
    let plans = arguments
        .files
        .iter()
        .map(|file| plan_stamp(file, &mut buffer).map_err(in_file(file)))
        .collect::<Result<Vec<_>, _>>()?;

    let field = build_field(&arguments.build);
    for (file, plan) in arguments.files.iter().zip(plans) {
        write_build_field(file, plan.offset, &field).map_err(in_file(file))?;
        if plan.resign {
            sign_ad_hoc(file).map_err(in_file(file))?;
        }
        println!("{}: +{}", file.display(), arguments.build);
    }
    Ok(())
}

fn in_file(file: &Path) -> impl FnOnce(StampError) -> String + '_ {
    move |error| format!("{}: {error}", file.display())
}

fn parse_arguments(arguments: Vec<OsString>) -> Result<Arguments, StampError> {
    let mut arguments = arguments.into_iter();
    if arguments.next().is_none_or(|flag| flag != "--build") {
        return Err(StampError::Usage);
    }
    let build = arguments
        .next()
        .ok_or(StampError::Usage)?
        .into_string()
        .map_err(|build| StampError::InvalidBuildName(build.to_string_lossy().into_owned()))?;
    let files: Vec<PathBuf> = arguments.map(PathBuf::from).collect();
    if files.is_empty() {
        return Err(StampError::Usage);
    }
    Ok(Arguments { build, files })
}

/// Checks that `build` is a token that fits the build name field with its terminating NUL.
///
/// The version is the product version in the user agent, so the build name is held to the
/// characters of an RFC 9110 token.
fn check_build_name(build: &str) -> Result<(), StampError> {
    if build.is_empty() || !build.bytes().all(is_token_byte) {
        return Err(StampError::InvalidBuildName(build.to_owned()));
    }
    if build.len() >= STAMP_BUILD_CAPACITY {
        return Err(StampError::BuildNameTooLong(build.to_owned()));
    }
    Ok(())
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn plan_stamp(file: &Path, buffer: &mut [u8]) -> Result<Plan, StampError> {
    let mut artifact = File::open(file)?;
    let offset = locate_build_field(&mut artifact, buffer)?;
    let resign = cfg!(target_os = "macos") && is_mach_o(&mut artifact)? && signed_ad_hoc(file)?;
    Ok(Plan { offset, resign })
}

/// The offset of the build name field in the one version slot `artifact` holds.
///
/// `artifact` is read through `buffer`, which must be at least as long as the marker. The final
/// `STAMP_MARKER.len() - 1` bytes of each read are carried into the next, so a slot split between
/// two reads is found, and found once.
fn locate_build_field(artifact: &mut impl Read, buffer: &mut [u8]) -> Result<u64, StampError> {
    let carry = STAMP_MARKER.len() - 1;
    let mut buffer_offset = 0;
    let mut kept = 0;
    let mut first_slot = None;
    let mut slots = 0;
    loop {
        let filled = kept
            + match artifact.read(&mut buffer[kept..]) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
        for (index, window) in buffer[..filled].windows(STAMP_MARKER.len()).enumerate() {
            if window[0] == STAMP_MARKER[0] && window == STAMP_MARKER {
                slots += 1;
                first_slot.get_or_insert(buffer_offset + index as u64);
            }
        }
        kept = filled.min(carry);
        buffer.copy_within(filled - kept..filled, 0);
        buffer_offset += (filled - kept) as u64;
    }
    let slot = match (first_slot, slots) {
        (Some(slot), 1) => slot,
        (None, _) => return Err(StampError::NoSlot),
        (Some(_), count) => return Err(StampError::SeveralSlots(count)),
    };
    let field = slot + STAMP_MARKER.len() as u64;
    if buffer_offset + (kept as u64) < field + STAMP_BUILD_CAPACITY as u64 {
        return Err(StampError::TruncatedSlot);
    }
    Ok(field)
}

/// Whether `artifact` opens with the magic of a Mach-O file: 32-bit, 64-bit, or universal.
fn is_mach_o(artifact: &mut (impl Read + Seek)) -> Result<bool, StampError> {
    let mut magic = [0; 4];
    artifact.seek(SeekFrom::Start(0))?;
    artifact.read_exact(&mut magic)?;
    Ok(matches!(
        magic,
        [0xce | 0xcf, 0xfa, 0xed, 0xfe] | [0xca, 0xfe, 0xba, 0xbe | 0xbf]
    ))
}

/// Whether `file` is signed ad-hoc, or `false` if it is not signed.
///
/// `codesign --display` fails for an unsigned file and describes the signature of a signed one on
/// stderr. A file signed with an identity is refused: stamping invalidates that signature, and
/// signing the file ad-hoc in its place would hide that it was signed before it was stamped.
fn signed_ad_hoc(file: &Path) -> Result<bool, StampError> {
    let output = Command::new("codesign")
        .arg("--display")
        .arg("--verbose=1")
        .arg(file)
        .output()?;
    if !output.status.success() {
        return Ok(false);
    }
    if String::from_utf8_lossy(&output.stderr)
        .lines()
        .any(|line| line == "Signature=adhoc")
    {
        return Ok(true);
    }
    Err(StampError::SignedWithIdentity)
}

fn sign_ad_hoc(file: &Path) -> Result<(), StampError> {
    let output = Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(file)
        .output()?;
    if !output.status.success() {
        return Err(StampError::CodesignFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(())
}

fn write_build_field(
    file: &Path,
    offset: u64,
    field: &[u8; STAMP_BUILD_CAPACITY],
) -> Result<(), StampError> {
    let mut artifact = OpenOptions::new().write(true).open(file)?;
    artifact.seek(SeekFrom::Start(offset))?;
    artifact.write_all(field)?;
    artifact.sync_all()?;
    Ok(())
}

/// The build name field holding `build`, padded with NULs. `build` must be shorter than the field.
fn build_field(build: &str) -> [u8; STAMP_BUILD_CAPACITY] {
    let mut field = [0; STAMP_BUILD_CAPACITY];
    field[..build.len()].copy_from_slice(build.as_bytes());
    field
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use lore_base::test_util::TempDir;

    use super::*;

    const UNSTAMPED: [u8; STAMP_BUILD_CAPACITY] = [0; STAMP_BUILD_CAPACITY];
    const LEADING: &[u8] = b"leading bytes";
    const FIELD_OFFSET: u64 = (LEADING.len() + STAMP_MARKER.len()) as u64;

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    /// An artifact whose one slot holds `field`, with other bytes on either side.
    fn artifact(field: &[u8; STAMP_BUILD_CAPACITY]) -> Vec<u8> {
        [LEADING, &STAMP_MARKER[..], &field[..], b"trailing bytes"].concat()
    }

    fn locate(artifact: &[u8], buffer_size: usize) -> Result<u64, StampError> {
        locate_build_field(&mut &artifact[..], &mut vec![0; buffer_size])
    }

    fn stamp_file(file: &Path, build: &str) {
        let plan = plan_stamp(file, &mut vec![0; READ_BUFFER_SIZE]).unwrap();
        assert!(!plan.resign);
        write_build_field(file, plan.offset, &build_field(build)).unwrap();
    }

    #[test]
    fn arguments_take_a_build_name_and_files() {
        assert_eq!(
            parse_arguments(arguments(&["--build", "1171", "lore", "liblore.so"])).unwrap(),
            Arguments {
                build: "1171".to_owned(),
                files: vec![PathBuf::from("lore"), PathBuf::from("liblore.so")],
            }
        );
    }

    #[test]
    fn arguments_without_a_build_name_or_files_are_refused() {
        for values in [
            &[][..],
            &["lore"][..],
            &["--build"][..],
            &["--build", "1171"][..],
            &["--name", "1171", "lore"][..],
        ] {
            assert!(
                matches!(parse_arguments(arguments(values)), Err(StampError::Usage)),
                "{values:?}"
            );
        }
    }

    #[test]
    fn a_build_name_of_token_characters_is_accepted() {
        for build in ["1171", "20260924.3", "ci-1171", "A_b~c", "!#$%&'*+-.^_`|~"] {
            assert!(check_build_name(build).is_ok(), "{build}");
        }
    }

    #[test]
    fn a_build_name_outside_the_token_characters_is_refused() {
        for build in [
            "",
            "has space",
            "quote\"",
            "back\\slash",
            "slash/1",
            "semi;colon",
            "é",
            "tab\t",
            "nul\0",
        ] {
            assert!(
                matches!(
                    check_build_name(build),
                    Err(StampError::InvalidBuildName(_))
                ),
                "{build:?}"
            );
        }
    }

    #[test]
    fn a_build_name_must_leave_room_for_its_terminating_nul() {
        assert!(check_build_name(&"9".repeat(STAMP_BUILD_CAPACITY - 1)).is_ok());
        assert!(matches!(
            check_build_name(&"9".repeat(STAMP_BUILD_CAPACITY)),
            Err(StampError::BuildNameTooLong(_))
        ));
    }

    #[test]
    fn a_slot_is_found_once_wherever_the_reads_split_it() {
        let artifact = artifact(&UNSTAMPED);
        for buffer_size in STAMP_MARKER.len()..=artifact.len() + 1 {
            assert_eq!(
                locate(&artifact, buffer_size).unwrap(),
                FIELD_OFFSET,
                "{buffer_size}"
            );
        }
    }

    #[test]
    fn a_slot_opening_a_file_that_its_field_ends_is_found() {
        let artifact = [&STAMP_MARKER[..], &UNSTAMPED[..]].concat();
        assert_eq!(
            locate(&artifact, READ_BUFFER_SIZE).unwrap(),
            STAMP_MARKER.len() as u64
        );
    }

    #[test]
    fn an_artifact_without_a_slot_is_refused() {
        for artifact in [
            &b"no slot here"[..],
            &STAMP_MARKER[..STAMP_MARKER.len() - 1],
        ] {
            assert!(matches!(
                locate(artifact, READ_BUFFER_SIZE),
                Err(StampError::NoSlot)
            ));
        }
    }

    #[test]
    fn an_artifact_with_two_slots_is_refused_wherever_the_reads_split_it() {
        let artifact = artifact(&UNSTAMPED).repeat(2);
        for buffer_size in STAMP_MARKER.len()..=artifact.len() + 1 {
            assert!(
                matches!(
                    locate(&artifact, buffer_size),
                    Err(StampError::SeveralSlots(2))
                ),
                "{buffer_size}"
            );
        }
    }

    #[test]
    fn a_slot_cut_off_by_the_end_of_the_file_is_refused() {
        let artifact = [&STAMP_MARKER[..], &UNSTAMPED[1..]].concat();
        assert!(matches!(
            locate(&artifact, READ_BUFFER_SIZE),
            Err(StampError::TruncatedSlot)
        ));
    }

    #[test]
    fn mach_o_files_are_told_from_other_artifacts() {
        for magic in [
            [0xcf, 0xfa, 0xed, 0xfe],
            [0xce, 0xfa, 0xed, 0xfe],
            [0xca, 0xfe, 0xba, 0xbe],
            [0xca, 0xfe, 0xba, 0xbf],
        ] {
            assert!(is_mach_o(&mut Cursor::new(magic)).unwrap(), "{magic:x?}");
        }
        for other in [*b"\x7fELF", *b"MZ\x90\x00", *b"!<ar"] {
            assert!(!is_mach_o(&mut Cursor::new(other)).unwrap(), "{other:x?}");
        }
    }

    #[test]
    fn stamping_a_file_writes_only_the_build_name_field() {
        let dir = TempDir::new("lore-stamp-write-");
        let file = dir.child("artifact");
        std::fs::write(&file, artifact(&UNSTAMPED)).unwrap();

        stamp_file(&file, "1171");
        assert_eq!(
            std::fs::read(&file).unwrap(),
            artifact(&build_field("1171"))
        );
    }

    #[test]
    fn stamping_again_replaces_a_longer_build_name() {
        let dir = TempDir::new("lore-stamp-restamp-");
        let file = dir.child("artifact");
        std::fs::write(&file, artifact(&UNSTAMPED)).unwrap();

        stamp_file(&file, "a-long-build-name");
        stamp_file(&file, "2");
        assert_eq!(std::fs::read(&file).unwrap(), artifact(&build_field("2")));
    }
}
