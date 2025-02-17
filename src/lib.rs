#![feature(iterator_try_collect)]
#![feature(slice_as_chunks)]
#![feature(unsigned_is_multiple_of)]
mod fs;
mod kms;
mod lethe_object_store;
mod wrapped_extent;

mod ext2;
pub mod paged_object_store;

pub use ext2::Ext2ObjectStore;
pub use lethe_object_store::LetheObjectStore;
pub use paged_object_store::*;
