#[path = "common/feature_expectations.rs"]
mod contract;

#[test]
#[should_panic(expected = "expected Write")]
fn an_unexpected_refusal_cannot_pass_as_support() {
    contract::require_expected("base", "create_file", Some("unsupported"));
}

#[test]
#[should_panic(expected = "expected Refuse")]
fn an_unsupported_operation_cannot_be_counted_as_success() {
    contract::require_expected("quota", "set_attributes", None);
}

#[test]
#[should_panic(expected = "actual refusal")]
fn an_unrelated_error_cannot_satisfy_a_required_refusal() {
    contract::require_expected("v4", "create_file", Some("I/O error"));
}

#[test]
#[should_panic(expected = "expected Absent")]
fn missing_sharing_is_not_a_driver_refusal() {
    contract::require_expected("base", "truncate_shared", Some("unsupported feature"));
}
