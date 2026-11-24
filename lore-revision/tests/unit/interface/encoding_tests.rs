// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::encoding::decode_position;
use lore_base::encoding::encode_position;
use lore_base::wire_schema::WireSchema;
use lore_revision::event::LoreBytes;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreBinary;
use lore_revision::interface::LoreString;

/// Text crosses as its bytes, empty and not UTF-8 alike: the service checks it where it runs the
/// call. An empty string comes back as the null pointer the type documents.
#[test]
fn a_string_round_trips_its_bytes() {
    for bytes in [&b""[..], b"doc.md", b"a\xff\xfeb"] {
        let decoded: LoreString = bitcode::decode(&bitcode::encode(&LoreString::from_bytes(bytes)))
            .expect("a string must decode");
        assert_eq!(decoded.as_bytes(), bytes);
        assert_eq!(decoded.string.is_null(), bytes.is_empty());
    }
}

#[test]
fn a_binary_block_round_trips_its_bytes() {
    for bytes in [&b""[..], b"raw\x00bytes"] {
        let decoded: LoreBinary = bitcode::decode(&bitcode::encode(&LoreBinary::from_bytes(bytes)))
            .expect("a block must decode");
        assert_eq!(decoded.as_bytes(), bytes);
        assert_eq!(decoded.payload.is_null(), bytes.is_empty());
    }
}

/// A decoded view points into the input it was decoded from, not at a copy.
#[test]
fn a_decoded_view_points_into_its_input() {
    let contents = b"payload bytes";
    let encoded = bitcode::encode(&LoreBytes {
        ptr: contents.as_ptr().cast(),
        len: contents.len(),
    });

    let decoded: LoreBytes = bitcode::decode(&encoded).expect("a view must decode");

    assert!(encoded.as_ptr_range().contains(&decoded.ptr.cast()));
    // SAFETY: `encoded` is alive, and the view points into it.
    assert_eq!(unsafe { decoded.as_slice() }, contents);
}

/// The strings of an array share one column, so each has to come back with its own length.
#[test]
fn an_array_of_strings_round_trips() {
    let strings = LoreArray::from_vec(vec![
        LoreString::from_str("first"),
        LoreString::default(),
        LoreString::from_str("third"),
    ]);

    let decoded: LoreArray<LoreString> =
        bitcode::decode(&bitcode::encode(&strings)).expect("an array must decode");

    assert_eq!(decoded.as_slice(), strings.as_slice());
}

/// Input that ends before the bytes its lengths describe fails the decode instead of reading past
/// the end.
#[test]
fn a_truncated_column_fails_to_decode() {
    let encoded = bitcode::encode(&LoreString::from_str("doc.md"));
    for end in 0..encoded.len() {
        assert!(
            bitcode::decode::<LoreString>(&encoded[..end]).is_err(),
            "{end} of {} bytes must not decode",
            encoded.len()
        );
    }

    let mut too_long = u64::MAX.to_le_bytes().to_vec();
    too_long.extend_from_slice(b"doc.md");
    assert!(bitcode::decode::<LoreString>(&too_long).is_err());
}

/// An array built from a boxed slice takes over its allocation rather than copying the elements.
#[test]
fn an_array_takes_over_a_boxed_slice() {
    let boxed: Box<[u32]> = vec![1, 2, 3].into_boxed_slice();
    let elements = boxed.as_ptr();

    let array = LoreArray::from(boxed);

    assert_eq!(array.as_slice().as_ptr(), elements);
    assert_eq!(array.as_slice(), [1, 2, 3]);
}

/// An enum of more variants than bitcode's own derive supports.
#[rustfmt::skip]
#[derive(Debug, PartialEq, lore_macro::PositionBitcode)]
enum Wide {
    V0(u32), V1(u32), V2(u32), V3(u32), V4(u32), V5(u32), V6(u32), V7(u32),
    V8(u32), V9(u32), V10(u32), V11(u32), V12(u32), V13(u32), V14(u32), V15(u32),
    V16(u32), V17(u32), V18(u32), V19(u32), V20(u32), V21(u32), V22(u32), V23(u32),
    V24(u32), V25(u32), V26(u32), V27(u32), V28(u32), V29(u32), V30(u32), V31(u32),
    V32(u32), V33(u32), V34(u32), V35(u32), V36(u32), V37(u32), V38(u32), V39(u32),
    V40(u32), V41(u32), V42(u32), V43(u32), V44(u32), V45(u32), V46(u32), V47(u32),
    V48(u32), V49(u32), V50(u32), V51(u32), V52(u32), V53(u32), V54(u32), V55(u32),
    V56(u32), V57(u32), V58(u32), V59(u32), V60(u32), V61(u32), V62(u32), V63(u32),
    V64(u32), V65(u32), V66(u32), V67(u32), V68(u32), V69(u32), V70(u32), V71(u32),
    V72(u32), V73(u32), V74(u32), V75(u32), V76(u32), V77(u32), V78(u32), V79(u32),
    V80(u32), V81(u32), V82(u32), V83(u32), V84(u32), V85(u32), V86(u32), V87(u32),
    V88(u32), V89(u32), V90(u32), V91(u32), V92(u32), V93(u32), V94(u32), V95(u32),
    V96(u32), V97(u32), V98(u32), V99(u32), V100(u32), V101(u32), V102(u32), V103(u32),
    V104(u32), V105(u32), V106(u32), V107(u32), V108(u32), V109(u32), V110(u32), V111(u32),
    V112(u32), V113(u32), V114(u32), V115(u32), V116(u32), V117(u32), V118(u32), V119(u32),
    V120(u32), V121(u32), V122(u32), V123(u32), V124(u32), V125(u32), V126(u32), V127(u32),
    V128(u32), V129(u32), V130(u32), V131(u32), V132(u32), V133(u32), V134(u32), V135(u32),
    V136(u32), V137(u32), V138(u32), V139(u32), V140(u32), V141(u32), V142(u32), V143(u32),
    V144(u32), V145(u32), V146(u32), V147(u32), V148(u32), V149(u32), V150(u32), V151(u32),
    V152(u32), V153(u32), V154(u32), V155(u32), V156(u32), V157(u32), V158(u32), V159(u32),
    V160(u32), V161(u32), V162(u32), V163(u32), V164(u32), V165(u32), V166(u32), V167(u32),
    V168(u32), V169(u32), V170(u32), V171(u32), V172(u32), V173(u32), V174(u32), V175(u32),
    V176(u32), V177(u32), V178(u32), V179(u32), V180(u32), V181(u32), V182(u32), V183(u32),
    V184(u32), V185(u32), V186(u32), V187(u32), V188(u32), V189(u32), V190(u32), V191(u32),
    V192(u32), V193(u32), V194(u32), V195(u32), V196(u32), V197(u32), V198(u32), V199(u32),
    V200(u32), V201(u32), V202(u32), V203(u32), V204(u32), V205(u32), V206(u32), V207(u32),
    V208(u32), V209(u32), V210(u32), V211(u32), V212(u32), V213(u32), V214(u32), V215(u32),
    V216(u32), V217(u32), V218(u32), V219(u32), V220(u32), V221(u32), V222(u32), V223(u32),
    V224(u32), V225(u32), V226(u32), V227(u32), V228(u32), V229(u32), V230(u32), V231(u32),
    V232(u32), V233(u32), V234(u32), V235(u32), V236(u32), V237(u32), V238(u32), V239(u32),
    V240(u32), V241(u32), V242(u32), V243(u32), V244(u32), V245(u32), V246(u32), V247(u32),
    V248(u32), V249(u32), V250(u32), V251(u32), V252(u32), V253(u32), V254(u32), V255(u32),
    V256(u32),
}

/// An enum encoded by position.
#[derive(lore_macro::PositionBitcode)]
enum ByPosition {
    A(u32),
    B(String),
}

/// The same enum, encoded by bitcode's derive.
#[derive(bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
enum ByIndex {
    A(u32),
    B(String),
}

/// An enum encoded by position has a schema of its own, so a client and the service that encode
/// events in different ways refuse each other.
#[test]
fn an_enum_encoded_by_position_has_a_schema_of_its_own() {
    assert_ne!(ByPosition::SCHEMA, ByIndex::SCHEMA);
}

/// A value encodes as its variant's position in the declaration, followed by its payload, and
/// reads back as itself: the first variant, those either side of the positions that take another
/// byte, and the 257th.
#[test]
fn a_variant_past_the_256th_round_trips() {
    for (position, value) in [
        (0, Wide::V0(7)),
        (127, Wide::V127(8)),
        (128, Wide::V128(9)),
        (255, Wide::V255(10)),
        (256, Wide::V256(11)),
    ] {
        let encoded = bitcode::encode(&value);
        let mut expected = Vec::new();
        encode_position(position, &mut expected);
        assert!(
            encoded.starts_with(&expected),
            "{value:?} must lead with its position"
        );
        assert_eq!(
            bitcode::decode::<Wide>(&encoded).expect("a value must decode"),
            value
        );
    }
}

/// Values encoded together read back in their order, none and one included.
#[test]
fn several_values_round_trip_in_order() {
    for values in [
        vec![],
        vec![Wide::V200(1)],
        vec![Wide::V3(1), Wide::V256(2), Wide::V3(3)],
    ] {
        let decoded: Vec<Wide> =
            bitcode::decode(&bitcode::encode(&values)).expect("values must decode");
        assert_eq!(decoded, values);
    }
}

/// A position no variant has fails the decode rather than reading as another variant.
#[test]
fn an_unknown_position_fails_to_decode() {
    let mut encoded = Vec::new();
    encode_position(LoreEvent::VARIANTS, &mut encoded);
    encoded.resize(4096, 0);
    for end in 0..encoded.len() {
        assert!(bitcode::decode::<LoreEvent>(&encoded[..end]).is_err());
    }
}

/// A position reads back as the value it was written for, up to `u32::MAX`, and consumes its
/// bytes alone. A position that ends early, runs past `u32::MAX` or takes more bytes than its
/// encoding fails to read, and so does a value that leads with one.
#[test]
fn a_position_reads_back_and_a_malformed_one_fails() {
    for position in [0, 127, 128, 16_383, 16_384, u32::MAX] {
        let mut encoded = Vec::new();
        encode_position(position, &mut encoded);
        encoded.push(0xaa);
        let mut input = encoded.as_slice();
        assert_eq!(
            decode_position(&mut input).expect("a position must read back"),
            position
        );
        assert_eq!(input, [0xaa]);
    }
    for encoded in [
        &[][..],
        &[0x80],
        &[0xff, 0xff, 0xff, 0xff, 0x10],
        &[0xff, 0xff, 0xff, 0xff, 0x8f, 0x01],
        &[0x80, 0x00],
        &[0xff, 0x80, 0x00],
    ] {
        assert!(
            decode_position(&mut &encoded[..]).is_err(),
            "{encoded:?} must not read"
        );
        assert!(
            bitcode::decode::<Wide>(&[encoded, &[0]].concat()).is_err(),
            "{encoded:?} must not decode"
        );
    }
}

/// An event whose encoding ends early, or carries bytes past its end, fails the decode.
#[test]
fn a_truncated_or_extended_event_fails_to_decode() {
    let event = LoreEvent::Log(lore_revision::relay::EventDispatcher::make_log(
        lore_base::log::LoreLogLevel::Info,
        "an event".to_string(),
    ));
    let encoded = bitcode::encode(&event);
    assert!(bitcode::decode::<LoreEvent>(&encoded).expect("an event must decode") == event);
    for end in 0..encoded.len() {
        assert!(
            bitcode::decode::<LoreEvent>(&encoded[..end]).is_err(),
            "{end} of {} bytes must not decode",
            encoded.len()
        );
    }
    let mut extended = encoded.clone();
    extended.push(0);
    assert!(bitcode::decode::<LoreEvent>(&extended).is_err());
}

/// The allocations `encode` makes on this thread.
fn allocations(encode: impl FnOnce() -> Vec<u8>) -> u64 {
    lore_base::allocator::set_allocation_counting(true);
    let before = lore_base::allocator::thread_allocation_count();
    drop(encode());
    let after = lore_base::allocator::thread_allocation_count();
    lore_base::allocator::set_allocation_counting(false);
    after - before
}

/// An event encodes with the allocations of a byte followed by its payload: the payload's encoder
/// collects its bytes into the encoding after the position, with no buffer between them, whatever
/// the payload's size.
#[test]
fn an_event_encodes_without_a_buffer_of_its_own() {
    use lore_revision::store::event::LoreStorageGetDataEventData;

    let contents = vec![7u8; 1 << 16];
    let data = LoreStorageGetDataEventData {
        id: 3,
        address: Default::default(),
        offset: 0,
        bytes: LoreBytes {
            ptr: contents.as_ptr().cast(),
            len: contents.len(),
        },
    };
    let event = LoreEvent::StorageGetData(data);

    assert_eq!(
        allocations(|| bitcode::encode(&event)),
        allocations(|| bitcode::encode(&(0u8, data)))
    );
}
