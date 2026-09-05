//! Rename-aware file history and blame endpoints — `GET /api/file-history`,
//! `GET /api/blame` (M5.33, #86).

use git_vista_protocol::blame::{BlamePage, FileHistoryPage};

use super::{network_error, req_get};

fn encode(segment: &str) -> String {
    js_sys::encode_uri_component(segment)
        .as_string()
        .unwrap_or_default()
}

/// Fetch one page of a file's rename-aware history (`GET /api/file-history`).
/// `skip` is the cursor from the previous page's `FileHistoryPage::cursor`
/// (`None` for the first page).
pub async fn fetch_file_history(
    path: &str,
    rev: &str,
    skip: Option<usize>,
) -> Result<FileHistoryPage, String> {
    let mut url = format!(
        "/api/file-history?path={}&rev={}&t={}",
        encode(path),
        encode(rev),
        js_sys::Date::now()
    );
    if let Some(skip) = skip {
        url.push_str(&format!("&skip={skip}"));
    }
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<FileHistoryPage>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Fetch one line-range page of a file's blame (`GET /api/blame`). `start`/
/// `end` are 1-based and inclusive; `None` for both asks the server for its
/// default first page.
pub async fn fetch_blame(
    path: &str,
    rev: &str,
    start: Option<usize>,
    end: Option<usize>,
) -> Result<BlamePage, String> {
    let mut url = format!(
        "/api/blame?path={}&rev={}&t={}",
        encode(path),
        encode(rev),
        js_sys::Date::now()
    );
    if let Some(start) = start {
        url.push_str(&format!("&start={start}"));
    }
    if let Some(end) = end {
        url.push_str(&format!("&end={end}"));
    }
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<BlamePage>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}
