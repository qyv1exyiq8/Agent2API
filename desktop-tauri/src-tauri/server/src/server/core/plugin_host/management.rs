use serde_json::{json, Value};

pub fn snapshot() -> Value {
    snapshot_with_packs(&super::list_packs())
}

pub fn snapshot_with_packs(packs: &[super::PackSummary]) -> Value {
    let host = super::runtime::status().map(|status| {
        json!({
            "running": status.running, "ready": status.ready, "providers": status.providers, "crashes": status.crashes,
            "disabled": status.disabled, "lastError": status.last_error,
        })
    });
    json!({
        "packs": packs, "host": host, "wasm": super::wasm::contract(),
        "adminPanels": super::admin_panels(), "accountFeatures": super::account_features(),
        "desktopLaunch": super::wasm::desktop_launch::summaries(),
    })
}
