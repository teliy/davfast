//! davfast — fast WebDAV client and cloud.mail.ru bridge.
//!
//! Built for Linux CLI usage where a filesystem tree with thousands of small
//! files has to be pushed to / pulled from a WebDAV server or the
//! cloud.mail.ru web API with maximum throughput.

pub mod dav;
pub mod mailru;
pub mod pipeline;
pub mod server;
pub mod util;

pub use dav::{DavClient, DavItem};
pub use pipeline::{Stats, TransferOpts};
