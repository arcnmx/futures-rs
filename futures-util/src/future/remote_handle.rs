use {
    crate::future::{CatchUnwind, FutureExt},
    futures_channel::oneshot::{self, Sender, Receiver},
    futures_core::{
        future::Future,
        task::{LocalWaker, Poll},
    },
    pin_utils::{unsafe_pinned, unsafe_unpinned},
    std::{
        any::Any,
        fmt,
        marker::Unpin,
        panic::{self, AssertUnwindSafe},
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    },
};

/// The handle to a remote future returned by
/// [`remote_handle`](crate::future::FutureExt::remote_handle).
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct RemoteHandle<T> {
    rx: Receiver<thread::Result<T>>,
    keep_running: Arc<AtomicBool>,
}

impl<T> RemoteHandle<T> {
    /// Drops this handle *without* canceling the underlying future.
    ///
    /// This method can be used if you want to drop the handle, but let the
    /// execution continue.
    pub fn forget(self) {
        self.keep_running.store(true, Ordering::SeqCst);
    }
}

impl<T: Send + 'static> Future for RemoteHandle<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<T> {
        match self.rx.poll_unpin(lw) {
            Poll::Ready(Ok(Ok(output))) => Poll::Ready(output),
            Poll::Ready(Ok(Err(e))) => panic::resume_unwind(e),
            Poll::Ready(Err(e)) => panic::resume_unwind(Box::new(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

type SendMsg<Fut> = Result<<Fut as Future>::Output, Box<(dyn Any + Send + 'static)>>;

/// A future which sends its output to the corresponding `RemoteHandle`.
/// Created by [`remote_handle`](crate::future::FutureExt::remote_handle).
#[must_use = "futures do nothing unless polled"]
pub struct Remote<Fut: Future> {
    tx: Option<Sender<SendMsg<Fut>>>,
    keep_running: Arc<AtomicBool>,
    future: CatchUnwind<AssertUnwindSafe<Fut>>,
}

impl<Fut: Future + fmt::Debug> fmt::Debug for Remote<Fut> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("Remote")
            .field(&self.future)
            .finish()
    }
}

impl<Fut: Future + Unpin> Unpin for Remote<Fut> {}

impl<Fut: Future> Remote<Fut> {
    unsafe_pinned!(future: CatchUnwind<AssertUnwindSafe<Fut>>);
    unsafe_unpinned!(tx: Option<Sender<SendMsg<Fut>>>);
}

impl<Fut: Future> Future for Remote<Fut> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<()> {
        if let Poll::Ready(_) = self.as_mut().tx().as_mut().unwrap().poll_cancel(lw) {
            if !self.keep_running.load(Ordering::SeqCst) {
                // Cancelled, bail out
                return Poll::Ready(())
            }
        }

        let output = match self.as_mut().future().poll(lw) {
            Poll::Ready(output) => output,
            Poll::Pending => return Poll::Pending,
        };

        // if the receiving end has gone away then that's ok, we just ignore the
        // send error here.
        drop(self.as_mut().tx().take().unwrap().send(output));
        Poll::Ready(())
    }
}

pub(super) fn remote_handle<Fut: Future>(future: Fut) -> (Remote<Fut>, RemoteHandle<Fut::Output>) {
    let (tx, rx) = oneshot::channel();
    let keep_running = Arc::new(AtomicBool::new(false));

    // AssertUnwindSafe is used here because `Send + 'static` is basically
    // an alias for an implementation of the `UnwindSafe` trait but we can't
    // express that in the standard library right now.
    let wrapped = Remote {
        future: AssertUnwindSafe(future).catch_unwind(),
        tx: Some(tx),
        keep_running: keep_running.clone(),
    };

    (wrapped, RemoteHandle { rx, keep_running })
}
