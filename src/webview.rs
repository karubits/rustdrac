//! The web view that shows a BMC console page: WebKitGTK on Linux, Microsoft WebView2 on
//! Windows. `console.rs` only sees [`ConsoleView`], which has the same API on both.

use std::cell::Cell;
use std::fmt::Display;
use std::time::Duration;

#[cfg(unix)]
#[path = "webview/webkit.rs"]
mod imp;
#[cfg(windows)]
#[path = "webview/webview2.rs"]
mod imp;

pub use imp::ConsoleView;

/// What the view reports back to its tab.
pub struct Hooks {
    /// The page called `window.close()` (the viewer's "Disconnect Viewer" button).
    pub on_close_requested: Box<dyn Fn()>,
    /// The page cannot be shown at all (engine failure, or the load kept failing). WebKit
    /// never calls this: it keeps its own error page, as it always did.
    #[cfg_attr(unix, allow(dead_code))]
    pub on_load_failed: Box<dyn Fn(String)>,
}

/// BMC web servers occasionally drop connections; the console page is reloaded this often.
pub const MAX_LOAD_RETRIES: u32 = 3;
pub const LOAD_RETRY_DELAY: Duration = Duration::from_secs(2);

/// The retry budget of one view.
pub(crate) struct LoadRetry(Cell<u32>);

impl LoadRetry {
    pub fn new() -> LoadRetry {
        LoadRetry(Cell::new(0))
    }

    /// Whether the caller should reload `uri` after [`LOAD_RETRY_DELAY`].
    pub fn should_retry(&self, uri: &str, err: &dyn Display) -> bool {
        if self.0.get() >= MAX_LOAD_RETRIES || uri.is_empty() {
            return false;
        }
        self.0.set(self.0.get() + 1);
        eprintln!("rustdrac: loading console failed ({err}), retrying");
        true
    }
}

/// One-time engine setup, after `gtk::init`. A no-op on Linux; on Windows it checks for the
/// WebView2 runtime and creates the shared browser environment.
pub fn init() -> anyhow::Result<()> {
    imp::init()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_are_limited() {
        let retry = LoadRetry::new();
        assert!(!retry.should_retry("", &"empty uri"));
        for _ in 0..MAX_LOAD_RETRIES {
            assert!(retry.should_retry("https://bmc/", &"dropped"));
        }
        assert!(!retry.should_retry("https://bmc/", &"dropped"));
    }
}
