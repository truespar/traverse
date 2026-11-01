//! Byte-for-byte compatibility of the legacy decoder with bincode 2.0.1.
//!
//! Every hex vector below was produced by
//! `bincode::serde::encode_to_vec(&value, bincode::config::standard())`
//! with bincode 2.0.1 (the version Traverse 0.8.x shipped) and pasted in
//! verbatim. If the decoder drifts, these fail - which is the point:
//! `.tvmodel` files and auth catalogs written by 0.8.x must keep
//! loading after bincode left the dependency tree.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use traverse_codec::legacy;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
enum E {
    Unit,
    Newtype(i32),
    Tuple(u8, String),
    Struct { a: u64, b: Option<bool> },
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Newtype(u32);

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Unit;

/// Every serde data-model shape Traverse's persisted types use, plus the
/// varint boundaries (250 / 251 / 65535 / 65536 / 2^32 / 2^64).
#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Sink {
    b_t: bool,
    b_f: bool,
    u8v: u8,
    i8v: i8,
    u16s: u16,
    u16l: u16,
    u32s: u32,
    u32m: u32,
    u32l: u32,
    u64v: u64,
    u64big: u64,
    u128v: u128,
    usz: usize,
    i16n: i16,
    i32n: i32,
    i64n: i64,
    i64big: i64,
    isz: isize,
    i128v: i128,
    f32v: f32,
    f64v: f64,
    ch: char,
    ch2: char,
    s: String,
    bytes: Vec<u8>,
    opt_none: Option<u32>,
    opt_some: Option<String>,
    seq: Vec<u32>,
    nested: Vec<Vec<String>>,
    map: BTreeMap<String, i64>,
    tuple: (u8, String, f64),
    arr: [u16; 3],
    e_unit: E,
    e_new: E,
    e_tup: E,
    e_struct: E,
    nt: Newtype,
    unit: Unit,
    unit_val: (),
    nested_opt: Option<Option<u8>>,
    empty_s: String,
    empty_v: Vec<u8>,
}

fn sink() -> Sink {
    Sink {
        b_t: true,
        b_f: false,
        u8v: 255,
        i8v: -2,
        u16s: 7,
        u16l: 300,
        u32s: 250,
        u32m: 65535,
        u32l: 70000,
        u64v: 251,
        u64big: 1u64 << 40,
        u128v: 1u128 << 100,
        usz: 123456,
        i16n: -300,
        i32n: -1,
        i64n: -126,
        i64big: -(1i64 << 40),
        isz: -5,
        i128v: -(1i128 << 70),
        f32v: 1.5,
        f64v: -2.25,
        ch: 'A',
        ch2: 'é',
        s: "héllo".to_string(),
        bytes: vec![1, 2, 3, 250, 251, 255],
        opt_none: None,
        opt_some: Some("x".to_string()),
        seq: vec![0, 250, 251, 65536],
        nested: vec![vec![], vec!["a".into(), "bb".into()]],
        map: BTreeMap::from([("k1".to_string(), -1i64), ("k2".to_string(), 300)]),
        tuple: (9, "t".into(), 0.5),
        arr: [1, 2, 3],
        e_unit: E::Unit,
        e_new: E::Newtype(-7),
        e_tup: E::Tuple(3, "tt".into()),
        e_struct: E::Struct {
            a: 1 << 33,
            b: Some(true),
        },
        nt: Newtype(42),
        unit: Unit,
        unit_val: (),
        nested_opt: Some(None),
        empty_s: String::new(),
        empty_v: vec![],
    }
}

// bincode 2.0.1, config::standard(), 187 bytes.
const SINK_HEX: &str = "0100fffe07fb2c01fafbfffffc70110100fbfb00fd0000000000010000fe00000000000000000000000010000000fc40e20100fb570201fbfb00fdffffffffff01000009feffffffffffffffff7f000000000000000000c03f00000000000002c041c3a90668c3a96c6c6f06010203fafbff000101780400fafbfb00fc00000100020002016102626202026b3101026b32fb5802090174000000000000e03f01020300010d020302747403fd000000000200000001012a01000000";

#[test]
fn kitchen_sink_matches_bincode_2_0_1() {
    let bytes = unhex(SINK_HEX);
    assert_eq!(bytes.len(), 187);
    let (decoded, consumed): (Sink, usize) = legacy::from_slice(&bytes).expect("decode");
    assert_eq!(consumed, bytes.len(), "every byte must be consumed");
    assert_eq!(decoded, sink());
}

#[test]
fn primitive_vectors_match_bincode_2_0_1() {
    fn one<'a, T: Deserialize<'a> + PartialEq + std::fmt::Debug>(hex: &'a str, expect: T) {
        let bytes = unhex(hex);
        let leaked: &'a [u8] = Box::leak(bytes.into_boxed_slice());
        let (v, n): (T, usize) = legacy::from_slice(leaked).unwrap();
        assert_eq!(v, expect, "{hex}");
        assert_eq!(n, leaked.len(), "{hex}: trailing bytes");
    }
    one("41", 'A');
    one("c3a9", 'é');
    one("e282ac", '€');
    one("f09f9880", '😀');
    one("fb2c01", 300u16);
    one("fbfb00", -126i64);
    one("fbfd00", -127i64);
    one("fe00000000000000000000000010000000", 1u128 << 100);
    one("0000c03f", 1.5f32);
}

/// `AuthCatalog { users: {"admin": {..., Admin}, "bob": {..., Reader}} }` as
/// Traverse 0.8.2 wrote it into the tvdb metadata page (map length,
/// strings, unit-variant index). The hashes are placeholders of bcrypt
/// length; the legacy decoder treats them as opaque strings.
const AUTH_HEX: &str = "020561646d696e0561646d696e43243262243132246162636465666768696a6b6c6d6e6f707172737475754142434445464748494a4b4c4d4e4f505152535455565758595a3031323334353637383961620203626f6203626f624324326224313224303132333435363738396162636465666768696a6b754142434445464748494a4b4c4d4e4f505152535455565758595a30313233343536373839636400";

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
enum Role {
    Reader,
    Editor,
    Admin,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct UserRecord {
    username: String,
    password_hash: String,
    role: Role,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct AuthCatalog {
    users: HashMap<String, UserRecord>,
}

#[test]
fn auth_catalog_written_by_0_8_2_decodes() {
    let bytes = unhex(AUTH_HEX);
    // Through the public entry point: no frame header -> legacy path.
    let cat: AuthCatalog =
        traverse_codec::decode(traverse_codec::Kind::AuthCatalog, &bytes).expect("decode");
    assert_eq!(cat.users.len(), 2);
    let admin = &cat.users["admin"];
    assert_eq!(admin.username, "admin");
    assert_eq!(admin.role, Role::Admin);
    assert_eq!(
        admin.password_hash,
        "$2b$12$abcdefghijklmnopqrstuuABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ab"
    );
    let bob = &cat.users["bob"];
    assert_eq!(bob.role, Role::Reader);
    assert!(bob.password_hash.starts_with("$2b$12$0123456789"));

    // And once re-saved it is framed, checksummed and still equal.
    let framed = traverse_codec::encode(traverse_codec::Kind::AuthCatalog, &cat).unwrap();
    assert!(traverse_codec::is_framed(&framed));
    let again: AuthCatalog =
        traverse_codec::decode(traverse_codec::Kind::AuthCatalog, &framed).unwrap();
    assert_eq!(again, cat);
}

#[test]
fn truncated_legacy_input_is_a_clean_error() {
    let bytes = unhex(SINK_HEX);
    for cut in [0, 1, 17, 100, 186] {
        let err = legacy::from_slice::<Sink>(&bytes[..cut]).unwrap_err();
        assert!(
            err.to_string().contains("unexpected end of input"),
            "cut {cut}: {err}"
        );
    }
}

#[test]
fn invalid_tags_are_rejected() {
    assert!(legacy::from_slice::<bool>(&[2]).is_err());
    assert!(legacy::from_slice::<Option<u8>>(&[2, 0]).is_err());
    assert!(legacy::from_slice::<u64>(&[255]).is_err());
    assert!(
        legacy::from_slice::<u16>(&unhex("fc70110100")).is_err(),
        "70000 does not fit u16"
    );
    assert!(
        legacy::from_slice::<String>(&[2, 0xff, 0xfe]).is_err(),
        "invalid UTF-8"
    );
    assert!(
        legacy::from_slice::<E>(&[9]).is_err(),
        "variant index out of range"
    );
}
