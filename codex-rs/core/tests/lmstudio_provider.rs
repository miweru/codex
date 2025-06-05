use codex_core::{WireApi, built_in_model_providers};

#[test]
fn lmstudio_defaults() {
    unsafe {
        std::env::set_var("LMSTUDIO_BASE_URL", "http://localhost:1234/v1");
    }
    let providers = built_in_model_providers();
    unsafe {
        std::env::remove_var("LMSTUDIO_BASE_URL");
    }
    let lm = providers.get("lmstudio").expect("lmstudio provider");
    assert_eq!(lm.base_url, "http://localhost:1234/v1");
    assert_eq!(lm.env_key.as_deref(), Some("LMSTUDIO_API_KEY"));
    assert_eq!(lm.wire_api, WireApi::Responses);
}
