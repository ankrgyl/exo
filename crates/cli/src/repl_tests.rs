use crate::{format_repl_failure, pick_repl_model, repl_agent_model_needs_update};

#[test]
fn pick_repl_model_prefers_an_explicit_request() {
    let registered = vec!["gpt-5.4".to_string(), "claude".to_string()];
    assert_eq!(
        pick_repl_model(&registered, Some("claude".to_string()))
            .expect("a registered request resolves"),
        "claude"
    );
}

#[test]
fn pick_repl_model_falls_back_to_the_first_registered() {
    let registered = vec!["gpt-5.4".to_string(), "claude".to_string()];
    assert_eq!(
        pick_repl_model(&registered, None).expect("the first registered model resolves"),
        "gpt-5.4"
    );
}

#[test]
fn pick_repl_model_rejects_an_unregistered_request() {
    let registered = vec!["gpt-5.4".to_string()];
    let error = pick_repl_model(&registered, Some("missing".to_string())).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("`missing` is not registered"));
    assert!(message.contains("exo model register missing --secret openai"));
    assert!(!message.contains("typescript harness failed"));
}

#[test]
fn pick_repl_model_requires_a_registered_model() {
    let error = pick_repl_model(&[], None).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("`gpt-5.6-terra` is not registered"));
    assert!(message.contains("exo secret set openai --env OPENAI_API_KEY"));
}

#[test]
fn missing_model_repl_errors_skip_the_turn_failed_prefix() {
    let error = anyhow::Error::new(executor::UnregisteredModelError::new("gpt-5.6-terra"))
        .context("typescript harness failed");
    let rendered = format_repl_failure(&error, "turn failed");
    assert!(rendered.starts_with("model `gpt-5.6-terra` is not registered."));
    assert!(!rendered.contains("turn failed"));
    assert!(!rendered.contains("typescript harness failed"));
    assert_eq!(
        format_repl_failure(&anyhow::anyhow!("sandbox exploded"), "turn failed"),
        "turn failed: sandbox exploded"
    );
}

#[test]
fn repl_agent_model_update_repairs_a_blank_model() {
    assert!(repl_agent_model_needs_update("", None));
    assert!(repl_agent_model_needs_update("   ", None));
}

#[test]
fn repl_agent_model_update_honors_an_explicit_request() {
    assert!(repl_agent_model_needs_update("gpt-5.4", Some("gpt-5.5")));
}

#[test]
fn repl_agent_model_update_keeps_an_existing_model_without_request() {
    assert!(!repl_agent_model_needs_update("gpt-5.4", None));
}
