// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Serde, straight to and from an [`Item`].
//!
//! A struct's field names are `&'static str`, so writing a row borrows them rather than allocating
//! a copy of each, and reading a row hands its attribute values over without building an
//! intermediate map.
//!
//! Both halves report that they are human-readable, which is the answer every row in production was
//! encoded under. That answer is load bearing in two opposite directions at once:
//!
//! - `Address::serialize` writes a hyphenated hex string when the answer is yes, and raw bytes when
//!   it is no. Production rows hold the string.
//! - `serialize_hex` writes an identifier as hex text when the answer is yes. Production rows hold
//!   the bytes.
//!
//! One answer cannot serve both, so `lore-base` resolves the identifier case with an explicit
//! exception for this module by name, `lore_base::types::LORE_DYNAMO_SERIALIZER`.
//!
//! On the read side `deserialize_raw` asks the same question of the deserializer and accepts either
//! bytes or hex text when the answer is yes — which rows in production need, because they hold both
//! spellings.
//!
//! Getting any of this wrong is silent, so `tests/unit/aws/differential.rs` holds all of it against
//! `serde_dynamo` for every row shape the stores write.
use std::fmt;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::de::IntoDeserializer;
use serde::forward_to_deserialize_any;
use serde::ser::Impossible;

use crate::aws::item::AttributeValue;
use crate::aws::item::ByteStr;
use crate::aws::item::Item;
use crate::aws::item::NumStr;

/// Why a value could not be converted.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{message}")]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

/// What kind of conversion failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// An attribute name that is not text.
    NotAKey,
    /// A row that is not a struct or a map.
    NotARow,
    /// A shape this bridge does not write: a tuple or struct enum variant.
    Unsupported,
    /// A number with no decimal form: an infinity or NaN.
    NotFinite,
    /// An attribute of a type the reader cannot take.
    WrongType,
    /// What a `Serialize` or `Deserialize` implementation reported itself.
    Custom,
}

impl Error {
    fn new(kind: ErrorKind, message: impl fmt::Display) -> Self {
        Self {
            kind,
            message: message.to_string(),
        }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl serde::ser::Error for Error {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self::new(ErrorKind::Custom, message)
    }
}

impl serde::de::Error for Error {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self::new(ErrorKind::Custom, message)
    }
}

/// Serialize `value` as a row.
pub fn to_item<T: Serialize>(value: T) -> Result<Item, Error> {
    value.serialize(RowSerializer)
}

/// Serialize `value` as one attribute.
pub fn to_attribute_value<T: Serialize>(value: T) -> Result<AttributeValue, Error> {
    value.serialize(ValueSerializer)
}

/// Deserialize a row into `T`.
pub fn from_item<T: DeserializeOwned>(item: Item) -> Result<T, Error> {
    T::deserialize(ItemDeserializer { item })
}

/// Turns any `Serialize` into one [`AttributeValue`].
#[lore_macro::test_pub]
struct ValueSerializer;

impl serde::Serializer for ValueSerializer {
    type Ok = AttributeValue;
    type Error = Error;

    type SerializeSeq = SeqSerializer;
    type SerializeTuple = SeqSerializer;
    type SerializeTupleStruct = SeqSerializer;
    type SerializeMap = MapSerializer<AttributeValue>;
    type SerializeStruct = MapSerializer<AttributeValue>;
    type SerializeTupleVariant = Impossible<AttributeValue, Error>;
    type SerializeStructVariant = Impossible<AttributeValue, Error>;

    /// Yes — the answer `serde_dynamo` gives, and the one the format needs. `Address::serialize`
    /// asks this question and writes a hyphenated hex string when the answer is yes, which is what
    /// production rows holding one contain. `serialize_hex` asks it too and wants the opposite for
    /// an identifier, so it carries an explicit exception for this serializer by name instead.
    /// Answering `false` here would keep identifiers binary and silently turn every `Address` into
    /// bytes.
    fn is_human_readable(&self) -> bool {
        true
    }

    fn serialize_bool(self, value: bool) -> Result<Self::Ok, Self::Error> {
        Ok(AttributeValue::Bool(value))
    }

    fn serialize_i8(self, value: i8) -> Result<Self::Ok, Self::Error> {
        self.serialize_i64(i64::from(value))
    }

    fn serialize_i16(self, value: i16) -> Result<Self::Ok, Self::Error> {
        self.serialize_i64(i64::from(value))
    }

    fn serialize_i32(self, value: i32) -> Result<Self::Ok, Self::Error> {
        self.serialize_i64(i64::from(value))
    }

    fn serialize_i64(self, value: i64) -> Result<Self::Ok, Self::Error> {
        Ok(AttributeValue::N(NumStr::from(value)))
    }

    fn serialize_u8(self, value: u8) -> Result<Self::Ok, Self::Error> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u16(self, value: u16) -> Result<Self::Ok, Self::Error> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u32(self, value: u32) -> Result<Self::Ok, Self::Error> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u64(self, value: u64) -> Result<Self::Ok, Self::Error> {
        Ok(AttributeValue::N(NumStr::from(value)))
    }

    /// The digits `f32`'s own `Display` writes, which are the shortest that read back as the same
    /// `f32`: widening to `f64` first would write the widened value's digits instead.
    fn serialize_f32(self, value: f32) -> Result<Self::Ok, Self::Error> {
        if !value.is_finite() {
            return Err(not_finite(value));
        }

        Ok(AttributeValue::N(NumStr::from_display(value)))
    }

    fn serialize_f64(self, value: f64) -> Result<Self::Ok, Self::Error> {
        if !value.is_finite() {
            return Err(not_finite(value));
        }

        Ok(AttributeValue::N(NumStr::from_display(value)))
    }

    fn serialize_char(self, value: char) -> Result<Self::Ok, Self::Error> {
        Ok(AttributeValue::string(value.encode_utf8(&mut [0u8; 4])))
    }

    fn serialize_str(self, value: &str) -> Result<Self::Ok, Self::Error> {
        Ok(AttributeValue::string(value))
    }

    fn serialize_bytes(self, value: &[u8]) -> Result<Self::Ok, Self::Error> {
        Ok(AttributeValue::blob(value))
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        Ok(AttributeValue::Null)
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Ok(AttributeValue::Null)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        Ok(AttributeValue::Null)
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        Ok(AttributeValue::S(ByteStr::from_static(variant)))
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        value.serialize(self)
    }

    /// `{"Variant": value}`, as `serde_dynamo` and `serde_json` both write an externally tagged
    /// enum.
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        Ok(AttributeValue::M(Box::new(variant_item(variant, value)?)))
    }

    fn serialize_seq(self, length: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Ok(SeqSerializer {
            values: Vec::with_capacity(length.unwrap_or(0)),
        })
    }

    fn serialize_tuple(self, length: usize) -> Result<Self::SerializeTuple, Self::Error> {
        self.serialize_seq(Some(length))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        length: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        self.serialize_seq(Some(length))
    }

    fn serialize_tuple_variant(
        self,
        name: &'static str,
        _index: u32,
        variant: &'static str,
        _length: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Err(unsupported("tuple variant", name, variant))
    }

    fn serialize_map(self, length: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Ok(MapSerializer::new(length.unwrap_or(0)))
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        length: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Ok(MapSerializer::new(length))
    }

    fn serialize_struct_variant(
        self,
        name: &'static str,
        _index: u32,
        variant: &'static str,
        _length: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Err(unsupported("struct variant", name, variant))
    }
}

/// `DynamoDB` numbers are decimal text with no infinity and no NaN, so neither is written rather
/// than written as something the service will reject or, worse, accept as a string.
fn not_finite(value: impl fmt::Display) -> Error {
    Error::new(
        ErrorKind::NotFinite,
        format_args!("DynamoDB holds no {value}, so it cannot be written as a number"),
    )
}

/// Nothing Lore stores has this shape, and a row that grew one silently would be worse than one
/// that refuses to be written.
fn unsupported(kind: &str, name: &str, variant: &str) -> Error {
    Error::new(
        ErrorKind::Unsupported,
        format_args!(
            "this client does not serialize a {kind} ({name}::{variant}); nothing Lore stores needs one"
        ),
    )
}

/// The single-entry map an externally tagged enum's newtype variant is written as.
fn variant_item<T: ?Sized + Serialize>(variant: &'static str, value: &T) -> Result<Item, Error> {
    let mut item = Item::with_capacity(1);
    item.push(
        ByteStr::from_static(variant),
        value.serialize(ValueSerializer)?,
    );

    Ok(item)
}

/// Turns a struct or a map into the [`Item`] that is the row itself, with no attribute around it.
///
/// Any other shape is refused: a row is a map on the wire.
struct RowSerializer;

impl serde::Serializer for RowSerializer {
    type Ok = Item;
    type Error = Error;

    type SerializeSeq = Impossible<Item, Error>;
    type SerializeTuple = Impossible<Item, Error>;
    type SerializeTupleStruct = Impossible<Item, Error>;
    type SerializeMap = MapSerializer<Item>;
    type SerializeStruct = MapSerializer<Item>;
    type SerializeTupleVariant = Impossible<Item, Error>;
    type SerializeStructVariant = Impossible<Item, Error>;

    /// The answer [`ValueSerializer`] gives, so a row and its attributes see one format.
    fn is_human_readable(&self) -> bool {
        true
    }

    fn serialize_bool(self, _value: bool) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("BOOL"))
    }

    fn serialize_i8(self, _value: i8) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("N"))
    }

    fn serialize_i16(self, _value: i16) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("N"))
    }

    fn serialize_i32(self, _value: i32) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("N"))
    }

    fn serialize_i64(self, _value: i64) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("N"))
    }

    fn serialize_u8(self, _value: u8) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("N"))
    }

    fn serialize_u16(self, _value: u16) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("N"))
    }

    fn serialize_u32(self, _value: u32) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("N"))
    }

    fn serialize_u64(self, _value: u64) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("N"))
    }

    fn serialize_f32(self, _value: f32) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("N"))
    }

    fn serialize_f64(self, _value: f64) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("N"))
    }

    fn serialize_char(self, _value: char) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("S"))
    }

    fn serialize_str(self, _value: &str) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("S"))
    }

    fn serialize_bytes(self, _value: &[u8]) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("B"))
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("NULL"))
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("NULL"))
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("NULL"))
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        Err(not_a_row("S"))
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        variant_item(variant, value)
    }

    fn serialize_seq(self, _length: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Err(not_a_row("L"))
    }

    fn serialize_tuple(self, _length: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Err(not_a_row("L"))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _length: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Err(not_a_row("L"))
    }

    fn serialize_tuple_variant(
        self,
        name: &'static str,
        _index: u32,
        variant: &'static str,
        _length: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Err(unsupported("tuple variant", name, variant))
    }

    fn serialize_map(self, length: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Ok(MapSerializer::new(length.unwrap_or(0)))
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        length: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Ok(MapSerializer::new(length))
    }

    fn serialize_struct_variant(
        self,
        name: &'static str,
        _index: u32,
        variant: &'static str,
        _length: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Err(unsupported("struct variant", name, variant))
    }
}

fn not_a_row(kind: &str) -> Error {
    Error::new(
        ErrorKind::NotARow,
        format_args!("a row must serialize as a map, got {kind}"),
    )
}

#[lore_macro::test_pub]
struct SeqSerializer {
    values: Vec<AttributeValue>,
}

impl serde::ser::SerializeSeq for SeqSerializer {
    type Ok = AttributeValue;
    type Error = Error;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.values.push(value.serialize(ValueSerializer)?);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(AttributeValue::L(self.values))
    }
}

impl serde::ser::SerializeTuple for SeqSerializer {
    type Ok = AttributeValue;
    type Error = Error;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        serde::ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        serde::ser::SerializeSeq::end(self)
    }
}

impl serde::ser::SerializeTupleStruct for SeqSerializer {
    type Ok = AttributeValue;
    type Error = Error;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        serde::ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        serde::ser::SerializeSeq::end(self)
    }
}

/// What a finished map becomes: the row itself at the top, an attribute anywhere below it.
#[lore_macro::test_pub]
trait FromMap {
    fn from_map(item: Item) -> Self;
}

impl FromMap for Item {
    fn from_map(item: Item) -> Self {
        item
    }
}

impl FromMap for AttributeValue {
    fn from_map(item: Item) -> Self {
        Self::M(Box::new(item))
    }
}

/// A struct or a map, collected into an [`Item`] and handed over as `O`.
#[lore_macro::test_pub]
struct MapSerializer<O> {
    item: Item,
    key: Option<ByteStr>,
    output: PhantomData<O>,
}

impl<O> MapSerializer<O> {
    fn new(capacity: usize) -> Self {
        Self {
            item: Item::with_capacity(capacity),
            key: None,
            output: PhantomData,
        }
    }
}

impl<O: FromMap> serde::ser::SerializeMap for MapSerializer<O> {
    type Ok = O;
    type Error = Error;

    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), Self::Error> {
        self.key = Some(key.serialize(KeySerializer)?);
        Ok(())
    }

    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        let key = self
            .key
            .take()
            .ok_or_else(|| Error::new(ErrorKind::Custom, "a map value arrived before its key"))?;

        self.item.insert(key, value.serialize(ValueSerializer)?);

        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(O::from_map(self.item))
    }
}

impl<O: FromMap> serde::ser::SerializeStruct for MapSerializer<O> {
    type Ok = O;
    type Error = Error;

    /// A field name is `&'static str`, so it costs nothing to keep — which is most of what this
    /// bridge exists to save — and a struct cannot repeat one, so appending skips the scan `insert`
    /// does.
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.item
            .push(ByteStr::from_static(key), value.serialize(ValueSerializer)?);

        Ok(())
    }

    fn skip_field(&mut self, _key: &'static str) -> Result<(), Self::Error> {
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(O::from_map(self.item))
    }
}

/// An attribute name has to be text, which is all this accepts.
struct KeySerializer;

impl serde::Serializer for KeySerializer {
    type Ok = ByteStr;
    type Error = Error;

    type SerializeSeq = Impossible<ByteStr, Error>;
    type SerializeTuple = Impossible<ByteStr, Error>;
    type SerializeTupleStruct = Impossible<ByteStr, Error>;
    type SerializeTupleVariant = Impossible<ByteStr, Error>;
    type SerializeMap = Impossible<ByteStr, Error>;
    type SerializeStruct = Impossible<ByteStr, Error>;
    type SerializeStructVariant = Impossible<ByteStr, Error>;

    fn is_human_readable(&self) -> bool {
        false
    }

    fn serialize_str(self, value: &str) -> Result<Self::Ok, Self::Error> {
        Ok(ByteStr::copy_from_str(value))
    }

    fn serialize_char(self, value: char) -> Result<Self::Ok, Self::Error> {
        Ok(ByteStr::copy_from_str(value.encode_utf8(&mut [0u8; 4])))
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        Ok(ByteStr::from_static(variant))
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        value.serialize(self)
    }

    fn serialize_bool(self, _value: bool) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("a bool"))
    }

    fn serialize_i8(self, _value: i8) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("a number"))
    }

    fn serialize_i16(self, _value: i16) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("a number"))
    }

    fn serialize_i32(self, _value: i32) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("a number"))
    }

    fn serialize_i64(self, _value: i64) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("a number"))
    }

    fn serialize_u8(self, _value: u8) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("a number"))
    }

    fn serialize_u16(self, _value: u16) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("a number"))
    }

    fn serialize_u32(self, _value: u32) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("a number"))
    }

    fn serialize_u64(self, _value: u64) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("a number"))
    }

    fn serialize_f32(self, _value: f32) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("a number"))
    }

    fn serialize_f64(self, _value: f64) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("a number"))
    }

    fn serialize_bytes(self, _value: &[u8]) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("bytes"))
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("nothing"))
    }

    fn serialize_some<T: ?Sized + Serialize>(self, _value: &T) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("an option"))
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("a unit"))
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("a unit struct"))
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        Err(not_a_key("a newtype variant"))
    }

    fn serialize_seq(self, _length: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Err(not_a_key("a sequence"))
    }

    fn serialize_tuple(self, _length: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Err(not_a_key("a tuple"))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _length: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Err(not_a_key("a tuple struct"))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _length: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Err(not_a_key("a tuple variant"))
    }

    fn serialize_map(self, _length: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Err(not_a_key("a map"))
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _length: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Err(not_a_key("a struct"))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _length: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Err(not_a_key("a struct variant"))
    }
}

fn not_a_key(kind: &str) -> Error {
    Error::new(
        ErrorKind::NotAKey,
        format_args!("an attribute name must be text, and {kind} is not"),
    )
}

/// A whole row.
///
/// Separate from [`ValueDeserializer`] so reading a row does not have to box it into an
/// `AttributeValue::M` first.
struct ItemDeserializer {
    item: Item,
}

impl<'de> serde::Deserializer<'de> for ItemDeserializer {
    type Error = Error;

    fn deserialize_any<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_map(ItemAccess::new(self.item))
    }

    /// A newtype around a row reads the row.
    fn deserialize_newtype_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf option
        unit unit_struct seq tuple tuple_struct map struct enum identifier ignored_any
    }
}

/// One attribute.
struct ValueDeserializer {
    value: AttributeValue,
}

impl<'de> serde::Deserializer<'de> for ValueDeserializer {
    type Error = Error;

    /// Yes, the answer both serializers give: `lore-base`'s `deserialize_raw` only accepts *either*
    /// raw bytes or hex text when the answer is yes, and rows in production hold both.
    fn is_human_readable(&self) -> bool {
        true
    }

    fn deserialize_any<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.value {
            AttributeValue::B(bytes) => visitor.visit_bytes(&bytes),
            AttributeValue::S(text) => visitor.visit_str(text.as_str()),
            AttributeValue::N(number) => visit_number(&number, visitor),
            AttributeValue::Bool(value) => visitor.visit_bool(value),
            AttributeValue::Null => visitor.visit_unit(),
            AttributeValue::M(item) => visitor.visit_map(ItemAccess::new(*item)),
            AttributeValue::L(values) => visitor.visit_seq(SeqAccess::new(values)),
            AttributeValue::Bs(values) => visitor.visit_seq(SeqAccess::new(
                values.into_iter().map(AttributeValue::B).collect(),
            )),
            AttributeValue::Ss(values) => visitor.visit_seq(SeqAccess::new(
                values.into_iter().map(AttributeValue::S).collect(),
            )),
            AttributeValue::Ns(values) => visitor.visit_seq(SeqAccess::new(
                values.into_iter().map(AttributeValue::N).collect(),
            )),
        }
    }

    fn deserialize_option<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.value {
            AttributeValue::Null => visitor.visit_none(),
            value => visitor.visit_some(ValueDeserializer { value }),
        }
    }

    /// The attribute is the newtype's inner value, so the inner type reads it from here.
    fn deserialize_newtype_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    /// Binary only. A `#[serde(with = "serde_bytes")]` field asks for this by name, and reading hex
    /// text into it would put the text's own bytes where the value belongs.
    ///
    /// The bytes are lent rather than given: a `Hash`, a `Context` or a fixed-size array copies
    /// straight out of the response buffer, where taking a `Vec` first would allocate one per
    /// attribute per row. [`Self::deserialize_byte_buf`] is the hook that asks to own them.
    fn deserialize_bytes<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.value {
            AttributeValue::B(bytes) => visitor.visit_bytes(&bytes),
            other => Err(not_binary(&other)),
        }
    }

    fn deserialize_byte_buf<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.value {
            AttributeValue::B(bytes) => visitor.visit_byte_buf(bytes.to_vec()),
            other => Err(not_binary(&other)),
        }
    }

    /// An externally tagged enum: `{"Variant": value}`, or the variant's name alone for a unit
    /// variant.
    fn deserialize_enum<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.value {
            AttributeValue::S(variant) => visitor.visit_enum(variant.as_str().into_deserializer()),
            AttributeValue::M(item) => match single_entry(*item) {
                Some((variant, value)) => visitor.visit_enum(EnumAccess { variant, value }),
                None => Err(not_an_enum("a map without exactly one entry")),
            },
            other => Err(not_an_enum(other.type_tag())),
        }
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        unit unit_struct seq tuple tuple_struct map struct identifier ignored_any
    }
}

fn not_binary(value: &AttributeValue) -> Error {
    Error::new(
        ErrorKind::WrongType,
        format_args!("expected a binary attribute, got {}", value.type_tag()),
    )
}

fn not_an_enum(kind: &str) -> Error {
    Error::new(
        ErrorKind::WrongType,
        format_args!("expected an enum as a name or a single-entry map, got {kind}"),
    )
}

/// The one attribute of a single-entry map, which is how an enum's newtype variant is stored.
fn single_entry(item: Item) -> Option<(ByteStr, AttributeValue)> {
    let mut entries = item.into_iter();
    let first = entries.next()?;

    entries.next().is_none().then_some(first)
}

/// A `DynamoDB` number is decimal text, so its Rust type is whatever fits it.
fn visit_number<'de, V: serde::de::Visitor<'de>>(
    number: &NumStr,
    visitor: V,
) -> Result<V::Value, Error> {
    let text = number.as_str();

    if let Ok(value) = text.parse::<u64>() {
        return visitor.visit_u64(value);
    }
    if let Ok(value) = text.parse::<i64>() {
        return visitor.visit_i64(value);
    }
    if let Ok(value) = text.parse::<f64>() {
        return visitor.visit_f64(value);
    }

    Err(Error::new(
        ErrorKind::WrongType,
        format_args!("{text:?} is not a number"),
    ))
}

struct ItemAccess {
    /// The item's own iterator, which for a row that fits inline moves no attribute to the heap.
    entries: <Item as IntoIterator>::IntoIter,
    value: Option<AttributeValue>,
}

impl ItemAccess {
    fn new(item: Item) -> Self {
        Self {
            entries: item.into_iter(),
            value: None,
        }
    }
}

impl<'de> serde::de::MapAccess<'de> for ItemAccess {
    type Error = Error;

    fn next_key_seed<K: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        match self.entries.next() {
            Some((name, value)) => {
                self.value = Some(value);
                seed.deserialize(KeyDeserializer { name }).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        let value = self.value.take().ok_or_else(|| {
            Error::new(ErrorKind::Custom, "a value was asked for before its name")
        })?;

        seed.deserialize(ValueDeserializer { value })
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len())
    }
}

struct SeqAccess {
    values: std::vec::IntoIter<AttributeValue>,
}

impl SeqAccess {
    fn new(values: Vec<AttributeValue>) -> Self {
        Self {
            values: values.into_iter(),
        }
    }
}

impl<'de> serde::de::SeqAccess<'de> for SeqAccess {
    type Error = Error;

    fn next_element_seed<T: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        match self.values.next() {
            Some(value) => seed.deserialize(ValueDeserializer { value }).map(Some),
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.values.len())
    }
}

/// An attribute name, which a struct reads as a field identifier.
struct KeyDeserializer {
    name: ByteStr,
}

impl<'de> serde::Deserializer<'de> for KeyDeserializer {
    type Error = Error;

    /// The name is handed over as a borrow: a struct matches a field identifier against its own
    /// static names, and a row that needs to own one copies it itself.
    fn deserialize_any<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_str(self.name.as_str())
    }

    /// A newtype around a name reads the name.
    fn deserialize_newtype_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf option
        unit unit_struct seq tuple tuple_struct map struct enum identifier ignored_any
    }
}

struct EnumAccess {
    variant: ByteStr,
    value: AttributeValue,
}

impl<'de> serde::de::EnumAccess<'de> for EnumAccess {
    type Error = Error;
    type Variant = VariantAccess;

    fn variant_seed<V: serde::de::DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Self::Error> {
        let variant = seed.deserialize(KeyDeserializer { name: self.variant })?;

        Ok((variant, VariantAccess { value: self.value }))
    }
}

struct VariantAccess {
    value: AttributeValue,
}

impl<'de> serde::de::VariantAccess<'de> for VariantAccess {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Self::Error> {
        Deserialize::deserialize(ValueDeserializer { value: self.value })
    }

    fn newtype_variant_seed<T: serde::de::DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<T::Value, Self::Error> {
        seed.deserialize(ValueDeserializer { value: self.value })
    }

    fn tuple_variant<V: serde::de::Visitor<'de>>(
        self,
        _length: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        serde::Deserializer::deserialize_seq(ValueDeserializer { value: self.value }, visitor)
    }

    fn struct_variant<V: serde::de::Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        serde::Deserializer::deserialize_map(ValueDeserializer { value: self.value }, visitor)
    }
}
