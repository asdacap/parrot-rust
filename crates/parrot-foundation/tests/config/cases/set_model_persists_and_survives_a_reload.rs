use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    fixture.load().unwrap().set_model("glm-5.2").unwrap();
    assert_eq!(fixture.load().unwrap().model, "glm-5.2");
}
