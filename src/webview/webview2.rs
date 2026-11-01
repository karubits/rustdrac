//! Microsoft WebView2 console view (Windows). Placeholder until the real implementation lands.

use anyhow::Result;

use super::Hooks;
use crate::bmc::ConsolePage;

pub fn init() -> Result<()> {
    Ok(())
}

#[derive(Clone)]
pub struct ConsoleView;

impl ConsoleView {
    pub fn new(_page: &ConsolePage, _host: &str, _hooks: Hooks) -> Result<ConsoleView> {
        anyhow::bail!("the console is not available on Windows yet")
    }
    pub fn widget(&self) -> gtk::Widget {
        unreachable!()
    }
    pub fn load_uri(&self, _uri: &str) {}
    pub fn evaluate_js(&self, _js: &str, done: impl FnOnce(Option<String>) + 'static) {
        done(None)
    }
    pub fn snapshot_png(&self, done: impl FnOnce(Result<Vec<u8>>) + 'static) {
        done(Err(anyhow::anyhow!("no console")))
    }
    pub fn focus(&self) {}
    pub fn close(&self) {}
}
