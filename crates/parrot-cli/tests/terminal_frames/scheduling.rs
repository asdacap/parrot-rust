use parrot_cli::terminal_frames::spinner_text;

fn oracle_rows() -> Vec<serde_json::Value> {
    include_str!("fixtures/observations.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn source_oracle_spinner_text_rows_match() {
    let rows = oracle_rows();
    let mut compared = 0;
    for row in rows.iter().filter(|row| row["name"] == "spinner-text") {
        assert_eq!(
            spinner_text(
                row["activity"].as_str().unwrap(),
                row["frame"].as_u64().unwrap() as usize
            ),
            row["text"]
        );
        compared += 1;
    }
    assert_eq!(compared, 4);
}
