#![cfg(test)]

#[cfg(feature = "proptest")]
pub const PROPTEST_FACTOR_10: usize = 10;
#[cfg(not(feature = "proptest"))]
pub const PROPTEST_FACTOR_10: usize = 1;

#[cfg(feature = "proptest")]
pub const PROPTEST_FACTOR_1: usize = 1;
#[cfg(not(feature = "proptest"))]
pub const PROPTEST_FACTOR_1: usize = 0;

#[test]
fn size_of() {
    use crate::change::Change;
    use crate::{
        container::{map::MapSet, text::text_content::ListSlice, ContainerID},
        id::ID,
        op::{InsertContent, Op},
        span::IdSpan,
        Container, InsertValue, InternalString,
    };
    use std::ops::Range;

    use rle::rle_tree::{node::Node, tree_trait::CumulateTreeTrait};

    println!("Change {}", std::mem::size_of::<Change>());
    println!("Op {}", std::mem::size_of::<Op>());
    println!("InsertContent {}", std::mem::size_of::<InsertContent>());
    println!("MapSet {}", std::mem::size_of::<MapSet>());
    println!("ListSlice {}", std::mem::size_of::<ListSlice>());
    println!("Box {}", std::mem::size_of::<Box<dyn Container>>());
    println!("InsertValue {}", std::mem::size_of::<InsertValue>());
    println!("ID {}", std::mem::size_of::<ID>());
    println!("Vec {}", std::mem::size_of::<Vec<ID>>());
    println!("IdSpan {}", std::mem::size_of::<IdSpan>());
    println!("ContainerID {}", std::mem::size_of::<ContainerID>());
    println!("InternalString {}", std::mem::size_of::<InternalString>());
    println!(
        "Node<Range<u32>, Cumulated> {}",
        std::mem::size_of::<Node<'static, Range<u32>, CumulateTreeTrait<Range<u32>, 8>>>()
    );
}
