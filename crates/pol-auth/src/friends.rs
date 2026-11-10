//! The friends blob's shape: polcore's two retained 0xb0-byte Get Friend List
//! (2,3) tables (installed polcore 73b1864b), as the game host reports them.

pub const RECORD_LEN: usize = crate::wire::FRIEND_RECORD_LEN;
pub const A_SLOTS: usize = 200;
pub const B_SLOTS: usize = 100;
pub const MAGIC: &[u8; 8] = crate::wire::FRIENDS_MAGIC;
pub const BLOB_LEN: usize = crate::wire::FRIENDS_LEN;
const _: () = assert!(A_SLOTS + B_SLOTS == crate::wire::FRIEND_SLOTS);
