/*!
 * @file BrowserSession
 * @description Browser automation behind an async tab seam.
 *
 * Unwired by intent: zero dependents — not reachable from the
 * `wavecode` binary. Kept as a deliberate seed; see the "Wiring
 * status" section of docs/architecture.md before citing or wiring.
 *
 * Reserved for agent projects that need a browser; the seam has no
 * driver implementer or consumer yet (`wavecode-tools` owns the live
 * registry).
 * Responsibilities:
 * - Name tab state and page actions in driver-neutral types.
 * - Define the async session seam real drivers implement.
 * - Ship a scripted fake for tests and dry runs.
 * - Track open tabs in the fake so unknown ids fail like real drivers.
 *
 * This module must not depend on: any other workspace crate. Protocol
 * drivers (CDP, WebDriver) arrive behind this seam later.
 */

//! Browser automation as data plus a seam: scripted today, drivable later.

use std::collections::HashMap;

/// One browser tab snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabState {
    /// Tab identifier within the session.
    pub id: String,
    /// Current URL.
    pub url: String,
    /// Page title, if loaded.
    pub title: String,
}

/// Browser failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrowserError {
    /// No tab exists under the id.
    #[error("unknown tab: {0}")]
    UnknownTab(String),
    /// The driver is not connected.
    #[error("browser driver disconnected")]
    Disconnected,
    /// The selector matched nothing.
    #[error("no element matches: {0}")]
    NoMatch(String),
}

/// Browser session seam.
#[async_trait::async_trait]
pub trait BrowserSession: Send + Sync {
    /// Open a URL in a new tab, returning its tab state.
    async fn open(&self, url: &str) -> Result<TabState, BrowserError>;

    /// Snapshot visible text of one tab.
    async fn snapshot(&self, tab_id: &str) -> Result<String, BrowserError>;

    /// Click the first element matching the selector.
    async fn click(&self, tab_id: &str, selector: &str) -> Result<(), BrowserError>;

    /// Fill the first matching input with text.
    async fn fill(&self, tab_id: &str, selector: &str, text: &str) -> Result<(), BrowserError>;

    /// Close one tab.
    async fn close(&self, tab_id: &str) -> Result<(), BrowserError>;
}

/// Scripted fake: pages render canned text, actions append to a log.
#[derive(Debug, Default)]
pub struct FakeBrowser {
    pages: HashMap<String, String>,
    log: std::sync::Mutex<Vec<String>>,
    next_tab: std::sync::Mutex<u64>,
    tabs: std::sync::Mutex<HashMap<String, String>>,
}

impl FakeBrowser {
    /// Create a fake with scripted URL-to-text pages.
    pub fn new(pages: HashMap<String, String>) -> Self {
        Self {
            pages,
            log: Default::default(),
            next_tab: Default::default(),
            tabs: Default::default(),
        }
    }

    /// Recorded actions in call order.
    pub fn actions(&self) -> Vec<String> {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn record(&self, action: impl Into<String>) {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(action.into());
    }

    /// Fail with `UnknownTab` unless the tab is currently open.
    fn require_open_tab(&self, tab_id: &str) -> Result<(), BrowserError> {
        let tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
        if tabs.contains_key(tab_id) {
            Ok(())
        } else {
            Err(BrowserError::UnknownTab(tab_id.to_string()))
        }
    }
}

#[async_trait::async_trait]
impl BrowserSession for FakeBrowser {
    async fn open(&self, url: &str) -> Result<TabState, BrowserError> {
        let mut next = self.next_tab.lock().unwrap_or_else(|e| e.into_inner());
        *next += 1;
        let id = format!("tab-{next}");
        self.tabs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), url.to_string());
        self.record(format!("open {url}"));
        Ok(TabState {
            id,
            url: url.to_string(),
            title: format!("Fake: {url}"),
        })
    }

    async fn snapshot(&self, tab_id: &str) -> Result<String, BrowserError> {
        self.require_open_tab(tab_id)?;
        self.record(format!("snapshot {tab_id}"));
        // Fake tabs render every scripted page concatenated: tests assert
        // structure (calls succeed, text flows), not URL routing.
        Ok(self.pages.values().cloned().collect::<Vec<_>>().join("\n"))
    }

    async fn click(&self, tab_id: &str, selector: &str) -> Result<(), BrowserError> {
        self.require_open_tab(tab_id)?;
        self.record(format!("click {tab_id} {selector}"));
        if selector.trim().is_empty() {
            return Err(BrowserError::NoMatch(selector.to_string()));
        }
        Ok(())
    }

    async fn fill(&self, tab_id: &str, selector: &str, text: &str) -> Result<(), BrowserError> {
        self.require_open_tab(tab_id)?;
        self.record(format!("fill {tab_id} {selector}"));
        if selector.trim().is_empty() {
            return Err(BrowserError::NoMatch(selector.to_string()));
        }
        let _ = text;
        Ok(())
    }

    async fn close(&self, tab_id: &str) -> Result<(), BrowserError> {
        self.require_open_tab(tab_id)?;
        self.tabs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(tab_id);
        self.record(format!("close {tab_id}"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browser() -> FakeBrowser {
        FakeBrowser::new(HashMap::from([(
            "https://example.com".to_string(),
            "Example Domain".to_string(),
        )]))
    }

    #[tokio::test]
    async fn full_tab_lifecycle_flows() {
        let browser = browser();
        let tab = browser.open("https://example.com").await.unwrap();
        assert!(tab.id.starts_with("tab-"));
        let text = browser.snapshot(&tab.id).await.unwrap();
        assert!(text.contains("Example Domain"));
        browser.click(&tab.id, "a.more").await.unwrap();
        browser.fill(&tab.id, "input.q", "hello").await.unwrap();
        browser.close(&tab.id).await.unwrap();
        assert_eq!(browser.actions().len(), 5);
    }

    #[tokio::test]
    async fn empty_selectors_fail_explicitly() {
        let browser = browser();
        let tab = browser.open("https://example.com").await.unwrap();
        assert!(matches!(
            browser.click(&tab.id, "  ").await.unwrap_err(),
            BrowserError::NoMatch(_)
        ));
    }

    #[tokio::test]
    async fn unknown_tabs_fail_explicitly() {
        let browser = browser();
        assert_eq!(
            browser.snapshot("tab-999").await.unwrap_err(),
            BrowserError::UnknownTab("tab-999".to_string())
        );
        assert_eq!(
            browser.click("tab-999", "a.more").await.unwrap_err(),
            BrowserError::UnknownTab("tab-999".to_string())
        );
        assert_eq!(
            browser.fill("tab-999", "input.q", "hi").await.unwrap_err(),
            BrowserError::UnknownTab("tab-999".to_string())
        );
        assert_eq!(
            browser.close("tab-999").await.unwrap_err(),
            BrowserError::UnknownTab("tab-999".to_string())
        );
    }

    #[tokio::test]
    async fn closed_tabs_stay_closed() {
        let browser = browser();
        let tab = browser.open("https://example.com").await.unwrap();
        browser.close(&tab.id).await.unwrap();
        assert_eq!(
            browser.snapshot(&tab.id).await.unwrap_err(),
            BrowserError::UnknownTab(tab.id.clone())
        );
        assert_eq!(
            browser.close(&tab.id).await.unwrap_err(),
            BrowserError::UnknownTab(tab.id)
        );
    }

    #[tokio::test]
    async fn open_tabs_are_independent() {
        let browser = browser();
        let first = browser.open("https://example.com").await.unwrap();
        let second = browser.open("https://example.com").await.unwrap();
        assert_ne!(first.id, second.id);
        browser.close(&first.id).await.unwrap();
        // Second tab remains usable after the first closes.
        let text = browser.snapshot(&second.id).await.unwrap();
        assert!(text.contains("Example Domain"));
    }
}
