use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use parrot_cli::terminal_frames::{
    CancellationToken, ItemIdentity, LiveBufferCaret, LiveBufferItem, LiveBufferRender,
    LiveBufferRenderContext, LiveBufferRetention, ScrollbackItem, ScrollbackLayout,
    ScrollbackRenderContext, TerminalCellStyleSpan, TerminalFrameRenderer, TerminalLine,
    TerminalStyle,
};
use serde_json::Value;

#[path = "emulator.rs"]
mod emulator;

fn number(value: &Value, key: &str, fallback: isize) -> isize {
    value
        .get(key)
        .map_or(fallback, |x| x.as_i64().unwrap() as isize)
}
fn flag(value: &Value, key: &str, fallback: bool) -> bool {
    value.get(key).map_or(fallback, |x| x.as_bool().unwrap())
}
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap()
}

struct FixtureLive(Value);
impl LiveBufferItem for FixtureLive {
    fn render(&self, _: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        let lines = self.0["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| {
                let spans: Vec<_> = line["spans"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|span| {
                        TerminalCellStyleSpan::new(
                            number(span, "start", 0).try_into().unwrap(),
                            number(span, "length", 0).try_into().unwrap(),
                            TerminalStyle::new(text(span, "style")),
                        )
                    })
                    .collect();
                TerminalLine::with_spans(
                    text(line, "text"),
                    TerminalStyle::new(text(line, "style")),
                    spans,
                )
            })
            .collect();
        let caret = self.0["caret"].as_array().map(|caret| LiveBufferCaret {
            row: caret[0].as_i64().unwrap() as isize,
            cells: caret[1].as_i64().unwrap() as isize,
        });
        let retention = match text(&self.0, "retention") {
            "Tail" => LiveBufferRetention::Tail,
            "Fixed" => LiveBufferRetention::Fixed,
            "Caret" => LiveBufferRetention::Caret,
            other => panic!("unexpected retention {other}"),
        };
        Ok(LiveBufferRender {
            lines,
            caret,
            retention,
        })
    }
}

struct FixtureScroll {
    value: Value,
    sequence: Option<ItemIdentity>,
}
impl FixtureScroll {
    fn new(value: &Value, identities: &mut BTreeMap<String, ItemIdentity>) -> Self {
        let sequence = value.get("sequence").map(|key| {
            identities
                .entry(key.as_str().unwrap().to_owned())
                .or_insert_with(|| Arc::new(()))
                .clone()
        });
        Self {
            value: value.clone(),
            sequence,
        }
    }
}
impl ScrollbackItem for FixtureScroll {
    fn is_completed(&self) -> bool {
        flag(&self.value, "completed", true)
    }
    fn layout(&self) -> ScrollbackLayout {
        match self
            .value
            .get("layout")
            .and_then(Value::as_str)
            .unwrap_or("Compact")
        {
            "Compact" => ScrollbackLayout::Compact,
            "Block" => ScrollbackLayout::Block,
            "User" => ScrollbackLayout::User,
            "Assistant" => ScrollbackLayout::Assistant,
            other => panic!("unexpected layout {other}"),
        }
    }
    fn starts_layout(&self) -> bool {
        flag(&self.value, "starts", true)
    }
    fn ends_layout(&self) -> bool {
        flag(&self.value, "ends", true)
    }
    fn sequence_identity(&self) -> Option<ItemIdentity> {
        self.sequence.clone()
    }
    fn packing_identity(&self) -> Option<ItemIdentity> {
        self.value
            .get("packing")
            .map(|value| Arc::new(value.as_str().unwrap().to_owned()) as ItemIdentity)
    }
    fn packs(&self, previous: Option<&ItemIdentity>) -> bool {
        previous
            .and_then(|value| value.downcast_ref::<String>())
            .is_some_and(|previous| {
                self.value.get("pack_if").and_then(Value::as_str) == previous.split('|').next()
            })
    }
    fn continues(&self, previous: &dyn ScrollbackItem) -> bool {
        self.sequence
            .as_ref()
            .zip(previous.sequence_identity().as_ref())
            .is_some_and(|(own, previous)| Arc::ptr_eq(own, previous))
    }
    fn render(&self, context: ScrollbackRenderContext) -> io::Result<Vec<String>> {
        let skip_heading = flag(&self.value, "suppress_heading_if_same_packing", false)
            && context
                .previous_packing
                .as_ref()
                .and_then(|value| value.downcast_ref::<String>())
                .is_some_and(|previous| {
                    self.value.get("packing").and_then(Value::as_str) == Some(previous.as_str())
                });
        Ok(self.value["lines"]
            .as_array()
            .unwrap()
            .iter()
            .skip(usize::from(skip_heading))
            .map(|line| line.as_str().unwrap().to_owned())
            .collect())
    }
}

#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<u8>>>);
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut output, byte| {
        write!(output, "{byte:02x}").unwrap();
        output
    })
}
fn unhex(value: &str) -> Vec<u8> {
    assert_eq!(value.len() % 2, 0);
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

struct FrameStep {
    operation: Value,
    columns: usize,
    rows: usize,
    source: Vec<u8>,
    rust: Vec<u8>,
    error: bool,
}
struct ScenarioRun {
    name: String,
    columns: usize,
    rows: usize,
    steps: Vec<FrameStep>,
}

fn run_fixtures() -> Vec<ScenarioRun> {
    let scenarios: Value = serde_json::from_str(include_str!("fixtures/inputs.json")).unwrap();
    let mut observations = BTreeMap::new();
    for line in include_str!("fixtures/observations.jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        let record: Value = serde_json::from_str(line).unwrap();
        let name = text(&record, "name");
        if name.starts_with("scheduler-") || name.starts_with("spinner-") {
            continue;
        }
        let key = (name.to_owned(), record["step"].as_u64().unwrap() as usize);
        assert!(
            observations.insert(key, record).is_none(),
            "duplicate source record"
        );
    }
    let mut runs = Vec::new();
    for scenario in scenarios.as_array().unwrap() {
        let name = text(scenario, "name");
        let mut columns = number(scenario, "columns", 12);
        let mut rows = number(scenario, "rows", 100);
        let output = Output::default();
        let renderer = TerminalFrameRenderer::new(
            output.clone(),
            flag(scenario, "color", false),
            number(scenario, "max_live_rows", 10),
            number(scenario, "max_input_rows", 12),
            flag(scenario, "inline_diff", true),
        );
        let cancel = CancellationToken::new();
        let mut identities = BTreeMap::new();
        let mut run = ScenarioRun {
            name: name.to_owned(),
            columns: columns.max(1) as usize,
            rows: rows.max(1) as usize,
            steps: Vec::new(),
        };
        for (step, operation) in scenario["ops"].as_array().unwrap().iter().enumerate() {
            columns = number(operation, "columns", columns);
            rows = number(operation, "rows", rows);
            let boundary = output.0.lock().unwrap().len();
            let result = if text(operation, "op") == "clear" {
                renderer.clear(&cancel)
            } else {
                let items: Vec<Arc<dyn LiveBufferItem>> = operation["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|value| Arc::new(FixtureLive(value.clone())) as Arc<dyn LiveBufferItem>)
                    .collect();
                match text(operation, "op") {
                    "draw" => renderer.draw(&items, columns, rows, &cancel),
                    "commit" => renderer.commit(
                        Arc::new(FixtureScroll::new(&operation["scroll"], &mut identities)),
                        &items,
                        columns,
                        rows,
                        &cancel,
                    ),
                    other => panic!("unexpected operation {other}"),
                }
            };
            let rust = output.0.lock().unwrap()[boundary..].to_vec();
            let expected = observations
                .remove(&(name.to_owned(), step))
                .unwrap_or_else(|| panic!("missing source observation {name}/{step}"));
            let error = expected["error"].as_bool().unwrap();
            assert_eq!(
                result.is_err(),
                error,
                "{name}/{step}: Rust result {result:?}"
            );
            assert_eq!(
                hex(&rust),
                text(&expected, "output"),
                "{name}/{step}: exact source UTF-8 bytes"
            );
            run.steps.push(FrameStep {
                operation: operation.clone(),
                columns: columns.max(1) as usize,
                rows: rows.max(1) as usize,
                source: unhex(text(&expected, "output")),
                rust,
                error,
            });
        }
        runs.push(run);
    }
    assert!(
        observations.is_empty(),
        "unexecuted source frame observations: {observations:?}"
    );
    runs
}

#[test]
fn every_source_frame_operation_matches_bytes_and_errors() {
    let runs = run_fixtures();
    assert_eq!(runs.len(), 13);
    assert_eq!(runs.iter().map(|run| run.steps.len()).sum::<usize>(), 66);
    assert_eq!(
        runs.iter()
            .flat_map(|run| &run.steps)
            .filter(|step| step.error)
            .count(),
        6
    );
}

#[test]
fn source_and_rust_frames_preserve_terminal_state_and_scrollback() {
    for run in run_fixtures() {
        let mut source = emulator::Terminal::new(run.columns, run.rows);
        let mut rust = source.clone();
        for (index, step) in run.steps.iter().enumerate() {
            source.resize(step.columns, step.rows);
            rust.resize(step.columns, step.rows);
            source.feed(&step.source);
            rust.feed(&step.rust);
            assert_eq!(rust, source, "{}/{index}: terminal state", run.name);
            assert!(
                rust.cursor_visible && rust.autowrap,
                "{}/{index}: modes restored",
                run.name
            );
            assert!(
                !rust.alternate_screen,
                "{}/{index}: inline screen only",
                run.name
            );
            if run.name == "draw-clear" && index == 0 {
                assert_eq!(rust.line(0), "live[2J");
                assert_eq!(rust.cursor(), (3, 3));
            }
            if run.name == "draw-clear" && index > 0 {
                assert!(
                    rust.all_lines().iter().all(String::is_empty),
                    "clear erased mutable live rows"
                );
            }
            if run.name.starts_with("style-") {
                let line = rust.cells(0);
                assert_eq!(line[0], "界");
                assert_eq!(line[1], "");
                assert_eq!(line[2], "👨‍👩‍👧‍👦");
                assert_eq!(line[3], "");
                assert_eq!(line[4], "e\u{301}");
                assert_eq!(rust.cursor(), (1, 2));
            }
            if run.name == "resize" {
                assert_eq!(rust.cursor().1, 5);
                assert_eq!(rust.line(rust.cursor().0), "> abc");
                if index == 1 {
                    assert_eq!(rust.line(0), "abcdefg", "narrow resize clips text to cells");
                }
                if index == 2 {
                    assert_eq!(rust.cursor().0, 1, "height resize retains input caret");
                    assert_eq!(
                        rust.line(0),
                        "three",
                        "height resize retains newest tail row"
                    );
                }
            }
            if run.name == "replace-diff" && index == 2 {
                assert_eq!(rust.line(0), "new");
                assert_eq!(rust.line(1), "same");
            }
            if run.name == "replace-diff" && index == 3 {
                assert_eq!(rust.cursor(), (2, 1));
            }
            if run.name == "screen-scrollback" && text(&step.operation, "op") == "clear" {
                let lines = rust
                    .all_lines()
                    .into_iter()
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>();
                assert_eq!(
                    lines,
                    (0..10).map(|i| format!("commit-{i}")).collect::<Vec<_>>()
                );
                assert!(
                    !rust.scrollback().is_empty(),
                    "commits must reach actual scrollback"
                );
            }
        }
    }
}
