use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::models::Session;

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct PricingFile {
    #[serde(default)]
    pub(crate) web_search_per_1k: Option<f64>,
    #[serde(default)]
    pub(crate) models: HashMap<String, ModelPrice>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ModelPrice {
    pub(crate) input_per_m: f64,
    pub(crate) cached_input_per_m: f64,
    pub(crate) output_per_m: f64,
    #[serde(default)]
    pub(crate) long_context_threshold: Option<u64>,
    #[serde(default)]
    pub(crate) long_context_multiplier: Option<f64>,
    #[serde(default)]
    pub(crate) long_context_input_multiplier: Option<f64>,
    #[serde(default)]
    pub(crate) long_context_output_multiplier: Option<f64>,
}

#[derive(Clone, Debug)]
pub(crate) struct Pricing {
    pub(crate) web_search_per_1k: f64,
    pub(crate) models: HashMap<String, ModelPrice>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CostEstimate {
    pub(crate) token_cost: f64,
    pub(crate) web_search_cost: f64,
    pub(crate) total_cost: f64,
    pub(crate) uncached_input_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) long_context_applied: bool,
    pub(crate) known_model_price: bool,
}
impl Pricing {
    pub(crate) fn load(path: Option<&Path>) -> Result<Self> {
        let mut pricing = Self::default();
        if let Some(path) = path {
            let file = File::open(path)
                .with_context(|| format!("failed to open pricing file {}", path.display()))?;
            let override_pricing: PricingFile = serde_json::from_reader(file)
                .with_context(|| format!("failed to parse pricing file {}", path.display()))?;
            if let Some(web) = override_pricing.web_search_per_1k {
                pricing.web_search_per_1k = web;
            }
            for (model, price) in override_pricing.models {
                pricing.models.insert(model, price);
            }
        }
        Ok(pricing)
    }
}

impl Default for Pricing {
    fn default() -> Self {
        let mut models = HashMap::new();
        models.insert(
            "gpt-5.5".to_string(),
            ModelPrice {
                input_per_m: 5.0,
                cached_input_per_m: 0.5,
                output_per_m: 30.0,
                long_context_threshold: Some(272_000),
                long_context_multiplier: None,
                long_context_input_multiplier: Some(2.0),
                long_context_output_multiplier: Some(1.5),
            },
        );
        models.insert(
            "gpt-5.4".to_string(),
            ModelPrice {
                input_per_m: 2.5,
                cached_input_per_m: 0.25,
                output_per_m: 15.0,
                long_context_threshold: Some(272_000),
                long_context_multiplier: None,
                long_context_input_multiplier: Some(2.0),
                long_context_output_multiplier: Some(1.5),
            },
        );
        Self {
            web_search_per_1k: 10.0,
            models,
        }
    }
}
pub(crate) fn estimate_cost(
    session: &Session,
    pricing: &Pricing,
    include_web_cost: bool,
) -> CostEstimate {
    let usage = session.final_usage().cloned().unwrap_or_default();
    let cached = usage.cached_input_tokens.min(usage.input_tokens);
    let uncached = usage.input_tokens.saturating_sub(cached);
    let model = session.model.as_deref().unwrap_or_default();
    let mut estimate = CostEstimate {
        uncached_input_tokens: uncached,
        cached_input_tokens: cached,
        output_tokens: usage.output_tokens,
        known_model_price: false,
        ..CostEstimate::default()
    };

    if let Some(model_price) = pricing.models.get(model) {
        let long_context_applied = model_price
            .long_context_threshold
            .map(|threshold| session.max_request_input() > threshold)
            .unwrap_or(false);
        let multiplier = if long_context_applied {
            model_price.long_context_multiplier.unwrap_or(1.0)
        } else {
            1.0
        };
        let input_multiplier = if long_context_applied {
            model_price
                .long_context_input_multiplier
                .unwrap_or(multiplier)
        } else {
            1.0
        };
        let output_multiplier = if long_context_applied {
            model_price
                .long_context_output_multiplier
                .unwrap_or(multiplier)
        } else {
            1.0
        };
        estimate.token_cost = input_multiplier
            * ((uncached as f64 / 1_000_000.0) * model_price.input_per_m
                + (cached as f64 / 1_000_000.0) * model_price.cached_input_per_m)
            + output_multiplier
                * ((usage.output_tokens as f64 / 1_000_000.0) * model_price.output_per_m);
        estimate.long_context_applied = long_context_applied;
        estimate.known_model_price = true;
    }

    if include_web_cost {
        estimate.web_search_cost =
            (session.web_search_calls as f64 / 1_000.0) * pricing.web_search_per_1k;
    }
    estimate.total_cost = estimate.token_cost + estimate.web_search_cost;
    estimate
}
