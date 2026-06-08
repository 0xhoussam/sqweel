//! Bridge between sqlx's tokio world and GTK's glib main loop.
//!
//! sqlx futures need a tokio reactor; the UI runs on the glib main context.
//! We keep one shared tokio runtime (its own worker threads), run DB work on it,
//! and hand the result back via an `async-channel` receiver that the UI awaits
//! with `glib::spawn_future_local`.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::Runtime;

static RT: OnceLock<Runtime> = OnceLock::new();

fn rt() -> &'static Runtime {
    RT.get_or_init(|| Runtime::new().expect("failed to start tokio runtime"))
}

/// Spawn `fut` on the tokio runtime. Await the returned receiver on the GTK side
/// (`glib::spawn_future_local`) to get the result back on the main thread.
pub fn spawn<F>(fut: F) -> async_channel::Receiver<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = async_channel::bounded(1);
    rt().spawn(async move {
        let out = fut.await;
        let _ = tx.send(out).await;
    });
    rx
}
