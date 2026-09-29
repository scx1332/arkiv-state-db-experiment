//! The milestone-1 flow: two namespaces, ordered metered blocks, an ownership
//! transfer that moves no data, rejected operations, a historical read, one
//! block that touches both namespaces atomically, and the state root that
//! commits to each block's whole state.

use arkiv_db::{
    BlockMap, BlockNumber, Budget, Cost, Engine, EngineError, OwnerId, PriceSchedule, Rejection,
    Revision, WriteOp,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let alice = OwnerId::new([0xa1; 20]);
    let bob = OwnerId::new([0xb0; 20]);
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut blocks = BlockMap::new();
    blocks.record(BlockNumber(0), engine.head())?;

    // Block 1: create two namespaces.
    let mut branch = engine.begin(engine.head(), Some(Cost::new(1_000)))?;
    let shop = branch.execute(
        alice,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("shop")?,
        },
        Budget::limit(100),
    )?;
    let blog = branch.execute(
        bob,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("blog")?,
        },
        Budget::limit(100),
    )?;
    let arkiv_db::Effect::NamespaceCreated(shop) = shop.effect else {
        unreachable!()
    };
    let arkiv_db::Effect::NamespaceCreated(blog) = blog.effect else {
        unreachable!()
    };
    println!("block 1: created {shop} for alice and {blog} for bob");
    let sealed = branch.seal();
    let snapshot = engine.publish(&sealed)?;
    blocks.record(BlockNumber(1), snapshot)?;

    // Block 2: ordered operations on the same key, with receipts.
    let mut branch = engine.begin(engine.head(), Some(Cost::new(1_000)))?;
    let puts: [(&[u8], &[u8]); 3] = [
        (b"item/1", b"apples"),
        (b"item/1", b"pears"),
        (b"item/2", b"plums"),
    ];
    for (key, value) in puts {
        let put = WriteOp::Put {
            namespace: shop,
            key: key.to_vec(),
            value: value.to_vec(),
        };
        let executed = branch.execute(alice, put, Budget::limit(100))?;
        println!(
            "block 2: put {} = {} cost {}",
            String::from_utf8_lossy(key),
            String::from_utf8_lossy(value),
            executed.receipt.spent
        );
    }
    let sealed = branch.seal();
    println!("block 2: aggregate write cost {}", sealed.spent());
    blocks.record(BlockNumber(2), engine.publish(&sealed)?)?;
    let after_block_2 = blocks.at(&engine, BlockNumber(2))?;

    // Block 3: transfer the shop to bob without touching its records.
    let mut branch = engine.begin(engine.head(), Some(Cost::new(1_000)))?;
    let transfer = WriteOp::TransferNamespace {
        namespace: shop,
        expected_revision: Revision(0),
        new_owner: bob,
    };
    let executed = branch.execute(alice, transfer, Budget::limit(100))?;
    println!(
        "block 3: transferred {shop} to bob for {} touching {} record nodes",
        executed.receipt.spent, executed.physical.record_nodes
    );

    // Rejections: alice no longer owns the shop, and a tiny budget aborts.
    let stale_put = WriteOp::Put {
        namespace: shop,
        key: b"item/3".to_vec(),
        value: b"unauthorized".to_vec(),
    };
    let rejected = branch
        .execute(alice, stale_put, Budget::limit(100))
        .expect_err("alice is no longer the owner");
    println!("block 3: alice's put rejected: {rejected}");
    assert_eq!(rejected.reason, Rejection::Unauthorized);
    let small_put = WriteOp::Put {
        namespace: shop,
        key: b"item/3".to_vec(),
        value: b"over budget".to_vec(),
    };
    let rejected = branch
        .execute(bob, small_put, Budget::limit(5))
        .expect_err("five units is not enough");
    println!("block 3: bob's underfunded put rejected: {rejected}");
    assert_eq!(rejected.reason, Rejection::OutOfBudget);
    let sealed = branch.seal();
    blocks.record(BlockNumber(3), engine.publish(&sealed)?)?;

    // Historical read: block 2 still shows alice as owner and the old data.
    let owner_then = after_block_2.descriptor(shop, Budget::unlimited())?.value;
    let owner_now = engine
        .head_snapshot()
        .descriptor(shop, Budget::unlimited())?
        .value;
    println!(
        "history: owner at block 2 is {:?}, at head is {:?}",
        owner_then.owner == alice,
        owner_now.owner == bob
    );
    let head = engine.head_snapshot();
    assert!(after_block_2.records_shared_with(&head, shop));

    // Block 4: one block touches both namespaces; readers see all or nothing.
    let before = engine.head_snapshot();
    let mut branch = engine.begin(engine.head(), Some(Cost::new(1_000)))?;
    branch.execute(
        bob,
        WriteOp::Delete {
            namespace: shop,
            key: b"item/1".to_vec(),
        },
        Budget::limit(100),
    )?;
    branch.execute(
        bob,
        WriteOp::Put {
            namespace: blog,
            key: b"post/1".to_vec(),
            value: b"hello".to_vec(),
        },
        Budget::limit(100),
    )?;
    let sealed = branch.seal();
    // A competing candidate built on the same base loses once this publishes.
    let competitor = engine.begin(engine.head(), None)?.seal();
    blocks.record(BlockNumber(4), engine.publish(&sealed)?)?;
    match engine.publish(&competitor) {
        Err(EngineError::StaleHead { .. }) => println!("block 4: competing candidate rejected"),
        other => panic!("unexpected {other:?}"),
    }
    let after = engine.head_snapshot();
    let deleted_then = before.get(shop, b"item/1", Budget::unlimited())?.value;
    let deleted_now = after.get(shop, b"item/1", Budget::unlimited())?.value;
    let post_then = before.get(blog, b"post/1", Budget::unlimited())?.value;
    let post_now = after.get(blog, b"post/1", Budget::unlimited())?.value;
    println!(
        "block 4: before publish shop/item1={deleted_then:?} blog/post1={post_then:?}; after shop/item1={deleted_now:?} blog/post1={post_now:?}"
    );
    let history = before
        .scan_prefix(shop, b"item/", Budget::unlimited())?
        .value;
    println!(
        "history: block 3 still lists {} shop items including the deleted one",
        history.len()
    );
    println!(
        "blocks 0..=4 map to snapshots {:?}",
        (0..=4)
            .map(|block| blocks.resolve(BlockNumber(block)).unwrap().sequence())
            .collect::<Vec<_>>()
    );

    // State roots: each block's root commits to its whole logical state and
    // never changes; the empty competing candidate has its base's root.
    for block in 0..=4 {
        let root = blocks.at(&engine, BlockNumber(block))?.state_root();
        println!("block {block}: state root {root}");
    }
    assert_eq!(competitor.state_root(), before.state_root());
    assert_ne!(after.state_root(), before.state_root());
    Ok(())
}
