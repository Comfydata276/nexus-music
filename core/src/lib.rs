//! The parts of Nexus Music that every app on an account must do identically.
//!
//! The Windows app is built on this, and the Android app will be: a rule that lives here is
//! written once and gives both the same answer. So nothing here draws anything, keeps a
//! file, starts a thread or opens a connection; whatever it needs from the world, the app
//! hands it.

pub mod http;
pub mod listenbrainz;
pub mod musicbrainz;
pub mod notes;
pub mod offer;
pub mod sync;
pub mod text;
