//! The shipped `config/models.yaml` must be internally consistent: every model the router can
//! return is a priced registry entry with the same prices. An unpriced model cannot be reserved
//! against, so the workflows would refuse it at call time.
use pair_models::classification::config::RoutingConfig;
use pair_models::provider::ProviderRegistry;
use std::path::Path;

const MODELS_YAML: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../config/models.yaml");

#[test]
fn shipped_routing_candidates_are_priced_registry_models_with_matching_prices() {
    let registry = ProviderRegistry::load(Path::new(MODELS_YAML)).expect("registry loads");
    let routing = RoutingConfig::from_path(Path::new(MODELS_YAML)).expect("routing loads");
    assert!(!routing.candidates.is_empty());
    for c in &routing.candidates {
        let entry = registry
            .get(&c.id)
            .unwrap_or_else(|| panic!("routing candidate {} is not in the registry", c.id));
        let price = entry
            .price
            .as_ref()
            .unwrap_or_else(|| panic!("routing candidate {} has no registry price", c.id));
        assert_eq!(
            price.input_per_mtok.0, c.input_price_micros_per_mtok,
            "{} input price",
            c.id
        );
        assert_eq!(
            price.output_per_mtok.0, c.output_price_micros_per_mtok,
            "{} output price",
            c.id
        );
    }
}
