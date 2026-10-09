// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Bitcode encoding for the C API types that describe memory by a pointer and a length.
//!
//! Bitcode's derive cannot encode a raw pointer, so these implement the traits its derive macro
//! builds on, which bitcode exports without covering them by its semver guarantees. The workspace
//! pins bitcode for that reason.

use std::num::NonZeroUsize;

use bitcode::__private::Buffer;
use bitcode::__private::Decoder;
use bitcode::__private::Encoder;
use bitcode::__private::Result;
use bitcode::__private::View;
use bitcode::Decode;
use bitcode::Encode;

use super::LoreArray;
use super::LoreBinary;
use super::LoreString;
use crate::event::LoreBytes;

/// Bytes a length takes in a [`BytesEncoder`] column.
const LENGTH_SIZE: usize = size_of::<u64>();

/// Encodes byte strings as a column of little-endian `u64` lengths followed by the bytes, so that
/// [`BytesDecoder`] can hand out views of its input without allocating.
#[derive(Default)]
pub struct BytesEncoder {
    lengths: Vec<u8>,
    bytes: Vec<u8>,
}

impl BytesEncoder {
    fn push(&mut self, bytes: &[u8]) {
        self.lengths
            .extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        self.bytes.extend_from_slice(bytes);
    }
}

impl Buffer for BytesEncoder {
    fn collect_into(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.lengths);
        out.extend_from_slice(&self.bytes);
        self.lengths.clear();
        self.bytes.clear();
    }

    fn reserve(&mut self, additional: NonZeroUsize) {
        self.lengths.reserve(additional.get() * LENGTH_SIZE);
    }
}

impl Encoder<LoreString> for BytesEncoder {
    fn encode(&mut self, string: &LoreString) {
        self.push(string.as_bytes());
    }
}

impl Encoder<LoreBinary> for BytesEncoder {
    fn encode(&mut self, binary: &LoreBinary) {
        self.push(binary.as_bytes());
    }
}

impl Encoder<LoreBytes> for BytesEncoder {
    fn encode(&mut self, view: &LoreBytes) {
        // SAFETY: a view is encoded while its emitter keeps the bytes alive, as for `Serialize`.
        self.push(unsafe { view.as_slice() });
    }
}

/// Reads a [`BytesEncoder`] column, checking up front that the lengths fit the input, so that
/// each value it decodes is a slice of that input.
#[derive(Default)]
pub struct BytesDecoder<'a> {
    lengths: &'a [[u8; LENGTH_SIZE]],
    bytes: &'a [u8],
}

impl<'a> BytesDecoder<'a> {
    fn next(&mut self) -> &'a [u8] {
        let (length, lengths) = self
            .lengths
            .split_first()
            .expect("populate counted every length");
        self.lengths = lengths;
        let (bytes, rest) = self.bytes.split_at(u64::from_le_bytes(*length) as usize);
        self.bytes = rest;
        bytes
    }
}

/// Splits `count` bytes off the front of `input`.
fn take<'a>(input: &mut &'a [u8], count: usize) -> Result<&'a [u8]> {
    let (taken, rest) = input.split_at_checked(count).ok_or_else(truncated)?;
    *input = rest;
    Ok(taken)
}

/// The error for input that ends before the values it describes. Bitcode constructs its error
/// only inside its own decoders, so it is taken from a decode that fails the same way.
#[cold]
fn truncated() -> bitcode::Error {
    bitcode::decode::<u8>(&[]).expect_err("an empty input holds no byte")
}

impl<'a> View<'a> for BytesDecoder<'a> {
    fn populate(&mut self, input: &mut &'a [u8], length: usize) -> Result<()> {
        let (lengths, _) = take(
            input,
            length.checked_mul(LENGTH_SIZE).ok_or_else(truncated)?,
        )?
        .as_chunks::<LENGTH_SIZE>();
        let total = lengths
            .iter()
            .try_fold(0u64, |total, length| {
                total.checked_add(u64::from_le_bytes(*length))
            })
            .and_then(|total| usize::try_from(total).ok())
            .ok_or_else(truncated)?;
        self.bytes = take(input, total)?;
        self.lengths = lengths;
        Ok(())
    }
}

impl<'a> Decoder<'a, LoreString> for BytesDecoder<'a> {
    fn decode(&mut self) -> LoreString {
        LoreString::from_bytes(self.next())
    }
}

impl<'a> Decoder<'a, LoreBinary> for BytesDecoder<'a> {
    fn decode(&mut self) -> LoreBinary {
        LoreBinary::from_bytes(self.next())
    }
}

impl<'a> Decoder<'a, LoreBytes> for BytesDecoder<'a> {
    fn decode(&mut self) -> LoreBytes {
        let bytes = self.next();
        LoreBytes {
            ptr: bytes.as_ptr().cast(),
            len: bytes.len(),
        }
    }
}

/// The bytes as they are: text is checked where a call is run, not where it is decoded, so that a
/// relayed call refuses the text the in-process path refuses, with the same error.
impl Encode for LoreString {
    type Encoder = BytesEncoder;
}

impl<'a> Decode<'a> for LoreString {
    type Decoder = BytesDecoder<'a>;
}

impl Encode for LoreBinary {
    type Encoder = BytesEncoder;
}

impl<'a> Decode<'a> for LoreBinary {
    type Decoder = BytesDecoder<'a>;
}

/// A decoded view points into the input it was decoded from, which its receiver keeps alive for
/// as long as the view is read, as an emitter does for the views it hands a callback.
impl Encode for LoreBytes {
    type Encoder = BytesEncoder;
}

impl<'a> Decode<'a> for LoreBytes {
    type Decoder = BytesDecoder<'a>;
}

/// Encodes an array as the slice it holds.
pub struct LoreArrayEncoder<T: Encode>(<[T] as Encode>::Encoder);

impl<T: Encode> Default for LoreArrayEncoder<T> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<T: Encode> Buffer for LoreArrayEncoder<T> {
    fn collect_into(&mut self, out: &mut Vec<u8>) {
        self.0.collect_into(out);
    }

    fn reserve(&mut self, additional: NonZeroUsize) {
        self.0.reserve(additional);
    }
}

impl<T: Encode> Encoder<LoreArray<T>> for LoreArrayEncoder<T> {
    fn encode(&mut self, array: &LoreArray<T>) {
        self.0.encode(array.as_slice());
    }
}

/// Decodes an array as a boxed slice, whose allocation the array takes over.
pub struct LoreArrayDecoder<'a, T: Decode<'a>>(<Box<[T]> as Decode<'a>>::Decoder);

impl<'a, T: Decode<'a>> Default for LoreArrayDecoder<'a, T> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<'a, T: Decode<'a>> View<'a> for LoreArrayDecoder<'a, T> {
    fn populate(&mut self, input: &mut &'a [u8], length: usize) -> Result<()> {
        self.0.populate(input, length)
    }
}

impl<'a, T: Decode<'a>> Decoder<'a, LoreArray<T>> for LoreArrayDecoder<'a, T> {
    fn decode(&mut self) -> LoreArray<T> {
        Decoder::<Box<[T]>>::decode(&mut self.0).into()
    }
}

impl<T: Encode> Encode for LoreArray<T> {
    type Encoder = LoreArrayEncoder<T>;
}

impl<'a, T: Decode<'a>> Decode<'a> for LoreArray<T> {
    type Decoder = LoreArrayDecoder<'a, T>;
}
