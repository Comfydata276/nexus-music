//! Accounts, and signing in (design 8; slice S8). What is allowed, and what an answer is, is
//! decided in the shared library (`nexus_core::nexus::{account, kept}`); this carries it out.
//!
//! - [`check`] (S8b): the two password workers and their three doors (8.2, 8.3).
//! - [`desk`]: the one thread that writes, and the line of stated length waiting for it (19.3).
//! - [`setup`] (S8a): setting up, recovery, one-time codes (8.4, 8.5).
//! - [`session`] (S8c): signing in and out, devices, forcing out, a password changed (8.6 to 8.9).
//! - [`music`] (S9): the music login typed in by an admin, checked, sealed and handed out (9).
//! - [`routes`]: those, as the calls the door hands on.
//! - [`rows`]: what they read from the data.

mod admin;
pub mod check;
pub mod desk;
pub mod music;
pub mod routes;
pub mod rows;
pub mod session;
pub mod setup;

#[cfg(test)]
mod bench;
#[cfg(test)]
mod tests;
