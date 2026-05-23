use super::*;

#[test]
fn default_pricing_estimates_gpt_5_4_short_context() {
    let mut session = session_for_search("gpt-54-short", "pricing", "pricing");
    session.model = Some("gpt-5.4".to_string());
    session.cached_final_usage = Some(TokenUsage {
        input_tokens: 1_000_000,
        cached_input_tokens: 200_000,
        output_tokens: 1_000_000,
        total_tokens: 2_000_000,
        ..TokenUsage::default()
    });

    let estimate = estimate_cost(&session, &Pricing::default(), false);

    assert!(estimate.known_model_price);
    assert!(!estimate.long_context_applied);
    assert_cost_close(estimate.total_cost, 17.05);
}

#[test]
fn default_pricing_estimates_gpt_5_4_long_context_with_separate_output_rate() {
    let mut session = session_for_search("gpt-54-long", "pricing", "pricing");
    session.model = Some("gpt-5.4".to_string());
    session.max_request_input_tokens = 272_001;
    session.cached_final_usage = Some(TokenUsage {
        input_tokens: 1_000_000,
        output_tokens: 1_000_000,
        total_tokens: 2_000_000,
        ..TokenUsage::default()
    });

    let estimate = estimate_cost(&session, &Pricing::default(), false);

    assert!(estimate.known_model_price);
    assert!(estimate.long_context_applied);
    assert_cost_close(estimate.total_cost, 27.50);
}
