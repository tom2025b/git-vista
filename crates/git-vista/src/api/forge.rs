use super::{encode_component, history_json, send_read};
use crate::features::forge::core::PullDetails;
use git_vista_protocol::forge::ForgePage;

pub async fn fetch_forge_page(repo: &str, page: u32) -> Result<ForgePage, String> {
    let url = format!(
        "/api/forge/pulls?repo={}&page={page}&t={}",
        encode_component(repo),
        js_sys::Date::now()
    );
    let response = send_read(&url).await.map_err(|e| e.to_string())?;
    history_json(response).await.map_err(|e| e.to_string())
}

pub async fn fetch_pull_details(repo: &str, number: u64) -> Result<PullDetails, String> {
    let url = format!(
        "/api/forge/pulls?repo={}&number={number}&t={}",
        encode_component(repo),
        js_sys::Date::now()
    );
    let response = send_read(&url).await.map_err(|e| e.to_string())?;
    history_json(response).await.map_err(|e| e.to_string())
}
