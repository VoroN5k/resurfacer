use serde::{Deserialize, Serialize};

/// Messages received from the Chrome extension via native messaging stdin.
#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtensionMessage {
    TabCreated {
        tab_id: i64,
        url: String,
        title: Option<String>,
        #[serde(default)]
        opener_tab_id: Option<i64>,
        /// Milliseconds since Unix epoch, as reported by Date.now() in the extension.
        created_at: i64,
    },
    TabActivated {
        tab_id: i64,
    },
    TabRemoved {
        tab_id: i64,
    },
    TabUpdated {
        tab_id: i64,
        url: Option<String>,
        title: Option<String>,
        status: Option<String>,
    },
    /// Sent by the extension in response to a RequestContent command.
    TabContent {
        tab_id: i64,
        text: String,
        title: Option<String>,
    },
}

/// Commands sent to the Chrome extension via native messaging stdout.
#[derive(Serialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonCommand {
    RequestContent { tab_id: i64 },
    CloseTab { tab_id: i64 },
    ReopenUrls { urls: Vec<String> },
}
