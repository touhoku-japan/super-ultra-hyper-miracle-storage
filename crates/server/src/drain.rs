//! Reads what is left of a rejected request's body before the error response goes out.
//!
//! When the S3 service refuses a request without reading its body (rate limit, suspended
//! tenant, bad signature, no write quorum, ...), hyper closes the connection after the
//! response. The client, still sending the body, then often sees `BrokenPipe` or a reset
//! instead of the status, so it cannot tell `SlowDown` from a network failure. Reading the rest
//! of a small body first lets it get the response and keep the connection.

use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use axum::body::{Body, Bytes, HttpBody};
use http_body::{Frame, SizeHint};

/// Bodies with more left than this are not read; the connection is closed as before.
pub const DRAIN_MAX: u64 = 1024 * 1024;
/// Gives up on a client that sends the rest too slowly.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

enum Slot {
    Active(Body),
    /// Taken back by [`Drain::finish`]; a late reader gets an error, never a short body.
    Taken,
}

/// The request body as handed to the service, with a handle that can take it back.
pub struct Drain(Arc<Mutex<Slot>>);

struct SharedBody(Arc<Mutex<Slot>>);

impl Drain {
    /// Wraps `body`; pass the returned body on and keep the handle.
    pub fn wrap(body: Body) -> (Self, Body) {
        let slot = Arc::new(Mutex::new(Slot::Active(body)));
        (Self(Arc::clone(&slot)), Body::new(SharedBody(slot)))
    }

    /// Reads and discards what the service left unread, if it is at most [`DRAIN_MAX`] bytes.
    pub async fn finish(self) {
        let Slot::Active(mut body) = std::mem::replace(&mut *lock(&self.0), Slot::Taken) else {
            return;
        };
        if body.is_end_stream() || body.size_hint().lower() > DRAIN_MAX {
            return;
        }
        let read_rest = async {
            let mut read = 0u64;
            while let Some(Ok(frame)) =
                std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await
            {
                read += frame.data_ref().map_or(0, |d| d.len() as u64);
                if read > DRAIN_MAX {
                    break;
                }
            }
        };
        let _ = tokio::time::timeout(DRAIN_TIMEOUT, read_rest).await;
    }
}

fn lock(slot: &Mutex<Slot>) -> std::sync::MutexGuard<'_, Slot> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl HttpBody for SharedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        match &mut *lock(&self.0) {
            Slot::Active(body) => Pin::new(body).poll_frame(cx),
            Slot::Taken => Poll::Ready(Some(Err(axum::Error::new("request already answered")))),
        }
    }

    fn is_end_stream(&self) -> bool {
        match &*lock(&self.0) {
            Slot::Active(body) => body.is_end_stream(),
            Slot::Taken => false,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &*lock(&self.0) {
            Slot::Active(body) => body.size_hint(),
            Slot::Taken => SizeHint::default(),
        }
    }
}
