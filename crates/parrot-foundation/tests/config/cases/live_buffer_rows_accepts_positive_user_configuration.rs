use crate::support::Context;
#[test]
fn source_assertions() {
    for rows in [1, 25] {
        let context = Context::new();
        let configuration = context
            .configured(&format!("live_buffer_rows: {rows}\n"))
            .unwrap();
        assert_eq!(configuration.runtime.live_buffer_rows, rows);
    }
}
