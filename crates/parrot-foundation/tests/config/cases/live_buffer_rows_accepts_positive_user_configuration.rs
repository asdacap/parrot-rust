use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for rows in [1, 25] {
        let fixture = ConfigFixture::new();
        let configuration = fixture
            .configured(&format!("live_buffer_rows: {rows}\n"))
            .unwrap();
        assert_eq!(configuration.runtime.live_buffer_rows, rows);
    }
}
