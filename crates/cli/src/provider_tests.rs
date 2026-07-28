use crate::{parse_discovered_models, provider_models_url, suggest_models};

#[test]
fn provider_models_url_joins_without_duplicate_slash() {
    assert_eq!(
        provider_models_url("https://api.opper.ai/v3/compat"),
        "https://api.opper.ai/v3/compat/models"
    );
    assert_eq!(
        provider_models_url("https://api.opper.ai/v3/compat/"),
        "https://api.opper.ai/v3/compat/models"
    );
}

#[test]
fn parse_discovered_models_reads_the_standard_list_shape() {
    let body = br#"{"object":"list","data":[
        {"id":"openai/gpt-5.6-terra","object":"model","created":1704067200,"owned_by":"openai"},
        {"id":"anthropic/claude-sonnet-4-6","object":"model","created":1704067200,"owned_by":"anthropic"},
        {"id":"openai/gpt-5.6-terra","object":"model","created":1704067200,"owned_by":"openai"}
    ]}"#;

    let models = parse_discovered_models(body).unwrap();

    // Sorted and de-duplicated.
    assert_eq!(
        models,
        vec![
            "anthropic/claude-sonnet-4-6".to_string(),
            "openai/gpt-5.6-terra".to_string(),
        ]
    );
}

#[test]
fn parse_discovered_models_rejects_non_list_bodies() {
    assert!(parse_discovered_models(br#"{"error":"nope"}"#).is_err());
}

#[test]
fn suggest_models_matches_on_last_path_segment() {
    let models = vec![
        "openai/gpt-5.6-terra".to_string(),
        "openai/gpt-5.6-sol".to_string(),
        "anthropic/claude-sonnet-4-6".to_string(),
    ];

    // Bare model id finds the provider/model form.
    assert_eq!(
        suggest_models(&models, "gpt-5.6-terra"),
        vec!["openai/gpt-5.6-terra".to_string()]
    );
    // Wrong prefix still matches by segment.
    assert_eq!(
        suggest_models(&models, "wrong/claude-sonnet-4-6"),
        vec!["anthropic/claude-sonnet-4-6".to_string()]
    );
    assert!(suggest_models(&models, "nonexistent-model").is_empty());
}
