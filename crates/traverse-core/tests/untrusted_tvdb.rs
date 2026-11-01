//! Loading damaged or crafted `.tvdb` bytes gives an error, never a crash: invalid archives,
//! records that don't fit together, impossible counts and sizes, and random damage that keeps the
//! checksums valid (as a crafted file would).

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use traverse_core::error::Error;
use traverse_core::graph::Graph;
use traverse_core::graph::edge::Edge;
use traverse_core::graph::node::Node;
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::{EdgeId, EdgeTypeId, LabelId, NodeId};
use traverse_core::storage::catalog::Catalog;
use traverse_core::storage::format::{FileHeader, HEADER_SIZE, PAGE_HEADER_SIZE, PAGE_SIZE};
use traverse_core::storage::page::{PageType, SlottedPage, SlottedPageRef};
use traverse_core::storage::page_manager::PageManager;
use traverse_core::storage::{checksum, codec, compression};

/// A small valid file: three nodes with properties, two relationships.
fn sample() -> Vec<u8> {
    let mut g = Graph::new();
    let mut props = BTreeMap::new();
    props.insert("name".to_string(), PropertyValue::String("hello".into()));
    props.insert(
        "tags".to_string(),
        PropertyValue::List(vec![PropertyValue::Int(1), PropertyValue::Float(2.5)]),
    );
    let a = g.create_node(&["Person"], props.clone());
    let b = g.create_node(&["Person", "Admin"], props);
    let c = g.create_node(&["City"], BTreeMap::new());
    let mut since = BTreeMap::new();
    since.insert("since".to_string(), PropertyValue::Int(2020));
    g.create_edge(a, b, "KNOWS", since).unwrap();
    g.create_edge(b, c, "LIVES_IN", BTreeMap::new()).unwrap();
    let mut pm = PageManager::create_in_memory();
    codec::write_graph(&mut pm, &g, &[]).unwrap();
    pm.to_bytes()
}

fn load(bytes: Vec<u8>) -> Result<Graph, Error> {
    codec::load_graph(PageManager::open_from_bytes(bytes)?).map(|(g, ..)| g)
}

/// Loads without letting a panic escape; `Err(())` if loading panicked.
fn load_no_panic(bytes: Vec<u8>) -> Result<Result<Graph, Error>, ()> {
    catch_unwind(AssertUnwindSafe(|| load(bytes))).map_err(drop)
}

fn rkyv_bytes<T>(value: &T) -> Vec<u8>
where
    T: for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                rkyv::rancor::Error,
            >,
        >,
{
    rkyv::to_bytes::<rkyv::rancor::Error>(value)
        .unwrap()
        .to_vec()
}

/// Rewrites the first page of type `ty`: `edit` gets its records in slot order (`None` for
/// tombstones, as stored, flag byte included). The page is rebuilt with valid checksums.
fn rewrite_page(
    bytes: Vec<u8>,
    ty: PageType,
    edit: impl FnOnce(&mut Vec<Option<Vec<u8>>>),
) -> Vec<u8> {
    let mut pm = PageManager::open_from_bytes(bytes).unwrap();
    let id = (0..pm.total_pages())
        .find(|&id| SlottedPageRef::new(pm.get_page(id).unwrap(), id).page_type() == Some(ty))
        .expect("page of that type");
    let mut records: Vec<Option<Vec<u8>>> = {
        let page = SlottedPageRef::new(pm.get_page(id).unwrap(), id);
        (0..page.slot_count())
            .map(|s| (!page.is_tombstone(s)).then(|| page.read_record(s).unwrap().to_vec()))
            .collect()
    };
    edit(&mut records);
    let mut page = SlottedPage::new(pm.get_page_mut(id).unwrap(), id);
    page.init(ty);
    for (slot, record) in records.iter().enumerate() {
        match record {
            Some(r) => {
                page.insert_record(r).unwrap();
            }
            None => {
                page.insert_record(&[0]).unwrap();
                page.tombstone_slot(slot as u16).unwrap();
            }
        }
    }
    pm.to_bytes()
}

/// Replaces the first live record of a `ty` page with the rkyv bytes `archive`.
fn replace_first(bytes: Vec<u8>, ty: PageType, archive: &[u8]) -> Vec<u8> {
    rewrite_page(bytes, ty, |records| {
        let slot = records.iter().position(Option::is_some).unwrap();
        records[slot] = Some(compression::encode_record(archive));
    })
}

fn assert_invalid(result: Result<Graph, Error>, expect: &str) {
    match result {
        Err(Error::InvalidFormat(msg)) => assert!(msg.contains(expect), "{msg}"),
        Err(other) => panic!("expected an invalid-format error about {expect:?}, got {other}"),
        Ok(_) => panic!("expected an invalid-format error about {expect:?}, it loaded"),
    }
}

#[test]
fn the_sample_loads() {
    let g = load(sample()).unwrap();
    assert_eq!((g.node_count(), g.edge_count()), (3, 2));
}

#[test]
fn an_invalid_archive_is_rejected() {
    let bytes = replace_first(sample(), PageType::NodeData, &[0xab; 48]);
    assert_invalid(load(bytes), "node record");
}

#[test]
fn every_single_byte_change_to_a_record_is_survived() {
    let node = Node {
        id: NodeId(0),
        labels: [LabelId(0)].into_iter().collect(),
        properties: [(
            traverse_core::graph::types::PropertyKeyId(1),
            PropertyValue::Int(5),
        )]
        .into_iter()
        .collect(),
    };
    let good = rkyv_bytes(&node);
    for i in 0..good.len() {
        for flip in [0x01u8, 0x80, 0xff] {
            let mut bad = good.clone();
            bad[i] ^= flip;
            let file = replace_first(sample(), PageType::NodeData, &bad);
            assert!(load_no_panic(file).is_ok(), "byte {i} ^ {flip:#x} panicked");
        }
    }
}

#[test]
fn records_that_dont_fit_together_are_rejected() {
    let node = |id, label| Node {
        id: NodeId(id),
        labels: [LabelId(label)].into_iter().collect(),
        properties: Default::default(),
    };
    // past the node counter
    assert_invalid(
        load(replace_first(
            sample(),
            PageType::NodeData,
            &rkyv_bytes(&node(1_000, 0)),
        )),
        "node counter",
    );
    // a label id with no string
    assert_invalid(
        load(replace_first(
            sample(),
            PageType::NodeData,
            &rkyv_bytes(&node(0, 900)),
        )),
        "names no string",
    );
    // a node stored twice
    assert_invalid(
        load(replace_first(
            sample(),
            PageType::NodeData,
            &rkyv_bytes(&node(1, 0)),
        )),
        "stored twice",
    );
    // a relationship to a node that isn't there
    let dangling = Edge::new(EdgeId(0), EdgeTypeId(0), NodeId(0), NodeId(2_000));
    assert_invalid(
        load(replace_first(
            sample(),
            PageType::EdgeData,
            &rkyv_bytes(&dangling),
        )),
        "isn't stored",
    );
}

#[test]
fn malformed_property_data_is_rejected() {
    // a valid archive whose property bytes hold invalid UTF-8
    let node = Node {
        id: NodeId(0),
        labels: [LabelId(0)].into_iter().collect(),
        properties: [(
            traverse_core::graph::types::PropertyKeyId(1),
            PropertyValue::String("hello".into()),
        )]
        .into_iter()
        .collect(),
    };
    let mut archive = rkyv_bytes(&node);
    let at = archive
        .windows(5)
        .position(|w| w == b"hello")
        .expect("string bytes in the archive");
    archive[at] = 0xff;
    assert_invalid(
        load(replace_first(sample(), PageType::NodeData, &archive)),
        "UTF-8",
    );
}

#[test]
fn impossible_counters_are_rejected() {
    let with_counter = |next_node_id: u64| {
        rewrite_page(sample(), PageType::Catalog, |records| {
            let decoded = compression::decode_record(records[0].as_ref().unwrap()).unwrap();
            let mut catalog = Catalog::from_bytes(&decoded).unwrap();
            catalog.next_node_id = next_node_id;
            records[0] = Some(compression::encode_record(&catalog.to_bytes().unwrap()));
        })
    };
    assert_invalid(load(with_counter(u64::MAX)), "supported");
    // in range but far larger than the machine: refused before allocating, whatever the ceiling
    traverse_core::memory::set_memory_ceiling(1 << 30);
    match load(with_counter(u32::MAX as u64)) {
        Err(Error::MemoryLimitExceeded { .. } | Error::AllocationFailed(_)) => {}
        other => panic!("expected a memory error, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn a_page_count_past_the_file_is_rejected() {
    let mut bytes = sample();
    let mut header = FileHeader::from_bytes(bytes[..HEADER_SIZE].try_into().unwrap()).unwrap();
    header.page_count = u64::MAX / 2;
    bytes[..HEADER_SIZE].copy_from_slice(&header.to_bytes());
    match PageManager::open_from_bytes(bytes) {
        Err(Error::InvalidFormat(msg)) => assert!(msg.contains("pages"), "{msg}"),
        other => panic!(
            "expected an invalid-format error, got {:?}",
            other.map(|_| ())
        ),
    }
}

#[test]
fn a_slot_past_its_page_is_rejected() {
    let mut pm = PageManager::open_from_bytes(sample()).unwrap();
    let id = (0..pm.total_pages())
        .find(|&id| {
            SlottedPageRef::new(pm.get_page(id).unwrap(), id).page_type()
                == Some(PageType::NodeData)
        })
        .unwrap();
    let page = pm.get_page_mut(id).unwrap();
    // slot 0: offset 0xfff0, length 0x0100
    page[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + 4].copy_from_slice(&[0xf0, 0xff, 0x00, 0x01]);
    match PageManager::open_from_bytes(pm.to_bytes()) {
        Err(Error::InvalidFormat(msg)) => assert!(msg.contains("slot 0"), "{msg}"),
        other => panic!(
            "expected an invalid-format error, got {:?}",
            other.map(|_| ())
        ),
    }
}

#[test]
fn an_oversized_decompression_claim_is_rejected() {
    // LZ4 flag, a 2 GiB size prefix, a few bytes of block
    let bomb = [0x01, 0x00, 0x00, 0x00, 0x80, 0x1f, 0x00, 0x00, 0x00];
    let bytes = rewrite_page(sample(), PageType::NodeData, |records| {
        records[0] = Some(bomb.to_vec());
    });
    match load(bytes) {
        Err(Error::Decompression(msg)) => assert!(msg.contains("claims"), "{msg}"),
        other => panic!(
            "expected a decompression error, got {:?}",
            other.map(|_| ())
        ),
    }
}

#[test]
fn random_damage_with_valid_checksums_never_panics() {
    let good = sample();
    let pages = (good.len() - HEADER_SIZE) / PAGE_SIZE;
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut rejected = 0;
    for _ in 0..3_000 {
        let mut bytes = good.clone();
        let page = (next() as usize) % pages;
        let base = HEADER_SIZE + page * PAGE_SIZE;
        // damage 1-4 bytes of the page, never the checksum field itself, then re-seal the page
        for _ in 0..1 + next() % 4 {
            let mut at = (next() as usize) % PAGE_SIZE;
            if (40..44).contains(&at) {
                at = 44;
            }
            bytes[base + at] ^= (next() as u8) | 1;
        }
        checksum::write_page_checksum(&mut bytes[base..base + PAGE_SIZE]);
        match load_no_panic(bytes) {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => rejected += 1,
            Err(()) => panic!("loading damaged bytes panicked"),
        }
    }
    assert!(
        rejected > 0,
        "the damage should be noticed at least sometimes"
    );
}

/// A file with a node larger than a page, so its record continues on a chain of overflow pages.
fn sample_with_overflow() -> Vec<u8> {
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let text: String = (0..40_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            char::from(b'a' + (x % 26) as u8)
        })
        .collect();
    let mut g = Graph::new();
    let mut props = BTreeMap::new();
    props.insert("text".to_string(), PropertyValue::String(text));
    let a = g.create_node(&["Doc"], props);
    let b = g.create_node(&["Doc"], BTreeMap::new());
    g.create_edge(a, b, "CITES", BTreeMap::new()).unwrap();
    let mut pm = PageManager::create_in_memory();
    codec::write_graph(&mut pm, &g, &[]).unwrap();
    pm.to_bytes()
}

/// Pages of `bytes` of type `ty`.
fn pages_of(bytes: &[u8], ty: PageType) -> Vec<usize> {
    (0..(bytes.len() - HEADER_SIZE) / PAGE_SIZE)
        .filter(|&p| {
            let base = HEADER_SIZE + p * PAGE_SIZE;
            SlottedPageRef::new(&bytes[base..base + PAGE_SIZE], p as u64).page_type() == Some(ty)
        })
        .collect()
}

#[test]
fn a_record_on_overflow_pages_loads() {
    let bytes = sample_with_overflow();
    assert!(pages_of(&bytes, PageType::Overflow).len() >= 2);
    let g = load(bytes).unwrap();
    assert_eq!((g.node_count(), g.edge_count()), (2, 1));
}

#[test]
fn damage_to_an_overflow_chain_never_panics() {
    let good = sample_with_overflow();
    // the chain and the page holding its stub
    let mut targets = pages_of(&good, PageType::Overflow);
    targets.extend(pages_of(&good, PageType::NodeData));
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut rejected = 0;
    for _ in 0..3_000 {
        let mut bytes = good.clone();
        let page = targets[(next() as usize) % targets.len()];
        let base = HEADER_SIZE + page * PAGE_SIZE;
        // header, slot directory and the first bytes of the records: where the chain is described
        for _ in 0..1 + next() % 4 {
            let mut at = (next() as usize) % 160;
            if (40..44).contains(&at) {
                at = 44;
            }
            if next() % 2 == 0 {
                at = PAGE_SIZE - 1 - (next() as usize) % 64;
            }
            bytes[base + at] ^= (next() as u8) | 1;
        }
        checksum::write_page_checksum(&mut bytes[base..base + PAGE_SIZE]);
        match load_no_panic(bytes) {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => rejected += 1,
            Err(()) => panic!("loading a damaged chain panicked"),
        }
    }
    assert!(
        rejected > 0,
        "the damage should be noticed at least sometimes"
    );
}
