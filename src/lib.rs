#![feature(iterator_try_collect)]
mod ext4;
mod extents;
pub use ext4::Ext4Store;
pub mod paged_object_store;

pub use paged_object_store::*;

pub const PAGE_SIZE: usize = 4096;
