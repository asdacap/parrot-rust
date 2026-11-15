use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    fixture
        .configured("theme: dark\n")
        .unwrap()
        .set_model("added")
        .unwrap();
    assert_eq!(fixture.load().unwrap().model, "added");
    assert!(fixture.saved().contains("theme: dark"));
}
