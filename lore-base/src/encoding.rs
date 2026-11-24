// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Bitcode encoding of an enum as each value's position among the enum's variants, followed by the
//! value the variant holds, which `lore_macro::PositionBitcode` derives.
//!
//! An enum bitcode derives for keeps a column for every field of every variant, so encoding or
//! decoding a single value builds and walks the columns of all of them. These encode only the
//! variant a value holds, and take an enum of fewer than 2^32 variants.
//!
//! They implement the traits bitcode's derive macro builds on, which bitcode exports without
//! covering them by its semver guarantees. The workspace pins bitcode for that reason.

use std::num::NonZeroUsize;

use bitcode::__private::Buffer;
use bitcode::__private::Decoder;
use bitcode::__private::Encoder;
use bitcode::__private::Result;
use bitcode::__private::View;
use bitcode::Decode;
use bitcode::Encode;

/// Bits of a position each byte of its encoding carries.
const POSITION_BITS: u32 = 7;
/// Set in each byte of a position's encoding that another byte follows.
const POSITION_CONTINUES: u8 = 0x80;

/// Appends `position`, seven bits a byte from the lowest, each byte but the last with
/// [`POSITION_CONTINUES`] set: a position under 128 takes one byte.
pub fn encode_position(position: u32, out: &mut Vec<u8>) {
    let mut rest = position;
    while rest >= u32::from(POSITION_CONTINUES) {
        out.push(rest as u8 | POSITION_CONTINUES);
        rest >>= POSITION_BITS;
    }
    out.push(rest as u8);
}

/// Reads a position [`encode_position`] wrote off the front of `input`. Fails on input that ends
/// inside the position, on a position past `u32::MAX`, and on a position in more bytes than
/// [`encode_position`] writes for it, so that each position has one encoding.
pub fn decode_position(input: &mut &[u8]) -> Result<u32> {
    let mut position = 0u32;
    for shift in (0..u32::BITS).step_by(POSITION_BITS as usize) {
        let (&byte, rest) = input.split_first().ok_or_else(truncated)?;
        *input = rest;
        let bits = u32::from(byte & !POSITION_CONTINUES);
        if (bits << shift) >> shift != bits {
            return invalid_position();
        }
        position |= bits << shift;
        if byte & POSITION_CONTINUES == 0 {
            return if bits == 0 && shift > 0 {
                invalid_position()
            } else {
                Ok(position)
            };
        }
    }
    invalid_position()
}

/// The error for input that ends before the values it describes. Bitcode exports no constructor
/// for it, so it is taken from a decode that fails the same way.
#[cold]
pub fn truncated() -> bitcode::Error {
    bitcode::decode::<u8>(&[]).expect_err("an empty input holds no byte")
}

/// The error for a position no variant has: the error bitcode's derive returns for a variant
/// index past an enum's last.
#[cold]
pub fn invalid_position<T>() -> Result<T> {
    bitcode::__private::invalid_enum_variant()
}

/// An enum whose values encode as their position among its variants, followed by the value the
/// variant holds. `lore_macro::PositionBitcode` derives it, with bitcode's `Encode` and `Decode`
/// through [`PositionEncoder`] and [`PositionDecoder`].
pub trait Positioned: Sized {
    /// The encoder of the value one variant holds, by variant.
    type Payload: Send + Sync;

    /// The encoder of the value `self` holds, with that value encoded, by [`payload_encoder`].
    fn encode_payload(&self) -> Self::Payload;

    /// Appends the position of the variant `payload` encodes for, and the value it encoded, with
    /// [`collect_payload`].
    fn collect_payload(payload: Self::Payload, out: &mut Vec<u8>);

    /// Decodes the payload of the variant at `position` off the front of `input`, with
    /// [`decode_payload`], or fails for a position no variant has.
    fn decode_positioned(position: u32, input: &mut &[u8]) -> Result<Self>;
}

/// Encodes each value as its position and the value its variant holds, each with the encoder of
/// its own type.
///
/// Bitcode collects the encoding only after every value is encoded. The encoder keeps the encoder
/// of the first value's payload until then, and collects it into the encoding with no buffer
/// between them, so that a message that carries one value copies and allocates no more than the
/// encoding of its position and payload. Each later value is collected into a buffer as it is
/// encoded.
///
/// It depends on the workspace's pin of bitcode to 0.6.9: it implements bitcode's derive-support
/// traits, and drives the encoder of each payload's type through them.
pub struct PositionEncoder<T: Positioned> {
    first: Option<T::Payload>,
    rest: Vec<u8>,
}

impl<T: Positioned> Default for PositionEncoder<T> {
    fn default() -> Self {
        Self {
            first: None,
            rest: Vec::new(),
        }
    }
}

impl<T: Positioned> Buffer for PositionEncoder<T> {
    fn collect_into(&mut self, out: &mut Vec<u8>) {
        if let Some(first) = self.first.take() {
            T::collect_payload(first, out);
        }
        out.extend_from_slice(&self.rest);
        self.rest.clear();
    }

    fn reserve(&mut self, _additional: NonZeroUsize) {}
}

impl<T: Positioned> Encoder<T> for PositionEncoder<T> {
    #[inline]
    fn encode(&mut self, value: &T) {
        let payload = value.encode_payload();
        if self.first.is_none() {
            self.first = Some(payload);
        } else {
            T::collect_payload(payload, &mut self.rest);
        }
    }
}

/// The encoder of `T`, with `payload` encoded.
#[inline]
pub fn payload_encoder<T: Encode + ?Sized>(payload: &T) -> T::Encoder {
    let mut encoder = T::Encoder::default();
    encoder.reserve(NonZeroUsize::MIN);
    encoder.encode(payload);
    encoder
}

/// Appends `position`, and the value `encoder` encoded.
#[inline]
pub fn collect_payload(position: u32, encoder: &mut impl Buffer, out: &mut Vec<u8>) {
    encode_position(position, out);
    encoder.collect_into(out);
}

/// Decodes the values a [`PositionEncoder`] encoded.
///
/// Every value is decoded, and so checked, as the decoder is populated, so that decoding a value
/// can't fail. A lone value, as in a message that carries one, is held in place; more are held in
/// one allocation.
pub struct PositionDecoder<T> {
    first: Option<T>,
    rest: std::vec::IntoIter<T>,
}

impl<T> Default for PositionDecoder<T> {
    fn default() -> Self {
        Self {
            first: None,
            rest: Vec::new().into_iter(),
        }
    }
}

impl<'a, T: Positioned> View<'a> for PositionDecoder<T> {
    fn populate(&mut self, input: &mut &'a [u8], length: usize) -> Result<()> {
        *self = Self::default();
        let mut next = || {
            let position = decode_position(input)?;
            T::decode_positioned(position, input)
        };
        if length == 1 {
            self.first = Some(next()?);
        } else {
            self.rest = (0..length)
                .map(|_| next())
                .collect::<Result<Vec<_>>>()?
                .into_iter();
        }
        Ok(())
    }
}

impl<'a, T: Positioned + Send + Sync> Decoder<'a, T> for PositionDecoder<T> {
    #[inline]
    fn decode(&mut self) -> T {
        self.first
            .take()
            .or_else(|| self.rest.next())
            .expect("populate decoded every value")
    }
}

/// Decodes one value of type `T` off the front of `input`.
#[inline]
pub fn decode_payload<'a, T: Decode<'a>>(input: &mut &'a [u8]) -> Result<T> {
    let mut decoder = T::Decoder::default();
    decoder.populate(input, 1)?;
    Ok(decoder.decode())
}
