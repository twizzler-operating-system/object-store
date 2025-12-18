#![feature(iterator_try_collect)]
//mod fs;
//mod kms;
//mod lethe_object_store;
//mod wrapped_extent;

mod ext4;
pub use ext4::Ext4Store;
pub mod paged_object_store;

//pub use lethe_object_store::{
//    key_fprint, LetheObjectStore, LetheState, PerObjLetheState, StdIoWrapper as LetheIoWrapper,
//};
pub use paged_object_store::*;

pub const PAGE_SIZE: usize = 4096;
