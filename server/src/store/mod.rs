//! The data and the store kept apart (design 11, 12.3, 12.4, 19.3; slice S3, with its tests,
//! experiment X2, in `tests/x2.rs`).
//!
//! - [`data`]: the data, one SQLite file.
//! - [`apart`]: the store kept apart, one small file.
//! - [`keeper`]: the two together: compared at every start before anything is served, every
//!   change written in the order 12.3 sets, and the owner's two commands.
//! - [`copies`]: the nightly copy, and the copies made before a change of form or a put-back.
//! - [`lock`]: one program at a time on a backend's folders.
//! - [`private`]: every file the owner's alone, from the moment it is made.
//! - [`crash`]: the points between durable steps, for X2's tests.

pub mod apart;
pub mod copies;
pub mod crash;
pub mod data;
pub mod keeper;
pub mod lock;
pub mod private;
