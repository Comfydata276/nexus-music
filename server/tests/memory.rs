//! What a body costs while it is read: no more than its length and a little, however many
//! pieces it comes in. Measured with an allocator that counts, in a test binary of its own so
//! that nothing else allocates beside it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use axum::body::Bytes;
use nexus_music_server::bounds::BODY_MAX_BYTES;
use nexus_music_server::serve::take_body;

struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let now = NOW.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
        PEAK.fetch_max(now, Ordering::SeqCst);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
        NOW.fetch_sub(layout.size(), Ordering::SeqCst);
        unsafe { System.dealloc(at, layout) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// A body of `0` bytes in as many pieces, each a fresh allocation, as a proxy's stream
/// cut fine would give.
struct Crumbs(usize);

impl hyper::body::Body for Crumbs {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        if this.0 == 0 {
            return Poll::Ready(None);
        }
        this.0 -= 1;
        Poll::Ready(Some(Ok(hyper::body::Frame::data(Bytes::from(vec![b'a'; 1])))))
    }
}

#[test]
fn a_body_in_a_million_pieces_costs_its_length() {
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let before = NOW.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    let bytes = runtime.block_on(take_body(Crumbs(BODY_MAX_BYTES), BODY_MAX_BYTES)).unwrap();
    let peak = PEAK.load(Ordering::SeqCst) - before;
    assert_eq!(bytes.len(), BODY_MAX_BYTES);
    // The buffer, and a few pieces at a time: an eighth more, at most.
    assert!(peak <= BODY_MAX_BYTES + BODY_MAX_BYTES / 8, "a 1 MiB body took {peak} bytes at its peak");
}
