use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for price in ["-0.1", "NaN", "Infinity", "not-a-number"] {
        let fixture = ConfigFixture::new();
        assert!(fixture.configured(&format!("providers:\n  custom:\n    models:\n      m:\n        cached_input_price: {price}\n")).is_err());
    }
}
