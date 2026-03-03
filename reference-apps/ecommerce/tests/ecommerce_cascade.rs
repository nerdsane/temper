//! Verification Cascade Tests for e-commerce entities.
//!
//! Runs the full 3-level VerificationCascade on each entity spec:
//! - Level 1: Stateright exhaustive model checking
//! - Level 2: Deterministic simulation with fault injection
//! - Level 3: Property-based testing with random action sequences

use temper_verify::cascade::{CascadeLevel, VerificationCascade};

const ORDER_IOA: &str = include_str!("../specs/order.ioa.toml");
const PAYMENT_IOA: &str = include_str!("../specs/payment.ioa.toml");
const SHIPMENT_IOA: &str = include_str!("../specs/shipment.ioa.toml");

#[test]
fn cascade_order_all_levels_pass() {
    let cascade = VerificationCascade::from_ioa(ORDER_IOA)
        .with_sim_seeds(10)
        .with_prop_test_cases(1000);

    let result = cascade.run();

    for level in &result.levels {
        assert!(
            level.passed,
            "Order cascade level failed: {}",
            level.summary
        );
    }

    // L1: Stateright model check
    assert!(
        result
            .level_result(CascadeLevel::ModelCheck)
            .unwrap()
            .passed,
        "L1 Model Check should pass"
    );
    // L2: Deterministic simulation
    assert!(
        result
            .level_result(CascadeLevel::Simulation)
            .unwrap()
            .passed,
        "L2 Simulation should pass"
    );
    // L3: Property tests
    assert!(
        result
            .level_result(CascadeLevel::PropertyTest)
            .unwrap()
            .passed,
        "L3 Property Tests should pass"
    );
    assert!(result.all_passed, "Order cascade should pass all levels");
}

#[test]
fn cascade_payment_all_levels_pass() {
    let cascade = VerificationCascade::from_ioa(PAYMENT_IOA)
        .with_sim_seeds(10)
        .with_prop_test_cases(1000);

    let result = cascade.run();

    for level in &result.levels {
        assert!(
            level.passed,
            "Payment cascade level failed: {}",
            level.summary
        );
    }

    assert!(
        result
            .level_result(CascadeLevel::ModelCheck)
            .unwrap()
            .passed,
        "L1 Model Check should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::Simulation)
            .unwrap()
            .passed,
        "L2 Simulation should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::PropertyTest)
            .unwrap()
            .passed,
        "L3 Property Tests should pass"
    );
    assert!(result.all_passed, "Payment cascade should pass all levels");
}

#[test]
fn cascade_shipment_all_levels_pass() {
    let cascade = VerificationCascade::from_ioa(SHIPMENT_IOA)
        .with_sim_seeds(10)
        .with_prop_test_cases(1000);

    let result = cascade.run();

    for level in &result.levels {
        assert!(
            level.passed,
            "Shipment cascade level failed: {}",
            level.summary
        );
    }

    assert!(
        result
            .level_result(CascadeLevel::ModelCheck)
            .unwrap()
            .passed,
        "L1 Model Check should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::Simulation)
            .unwrap()
            .passed,
        "L2 Simulation should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::PropertyTest)
            .unwrap()
            .passed,
        "L3 Property Tests should pass"
    );
    assert!(result.all_passed, "Shipment cascade should pass all levels");
}
