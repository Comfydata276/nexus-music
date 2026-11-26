//! How much a song is played, from ListenBrainz.
//!
//! The questions and the reading of the answers are in the library
//! (`nexus_core::listenbrainz`), for the Android app to use too. This is the Windows app's
//! end: the connection they're asked over. Nothing here is used unless requests are
//! switched on.

use std::sync::OnceLock;
use std::time::Duration;

use nexus_core::listenbrainz::ListenBrainz;

pub(crate) fn service() -> &'static ListenBrainz {
    static SERVICE: OnceLock<ListenBrainz> = OnceLock::new();
    SERVICE.get_or_init(|| {
        // In the pinned tests, the stand-in (see `pins`): never the real service.
        #[cfg(test)]
        if crate::pins::entered() {
            return ListenBrainz::new(crate::pins::world());
        }
        ListenBrainz::new(crate::corehttp::public(Duration::from_secs(50)))
    })
}
