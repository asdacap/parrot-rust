//! BeginRow/EmitBroken observations from MarkdownRenderer.cs at
//! 5f27a8a92625ee31d8bec094e5c3e09550193781.
use parrot_cli::terminal_layout::MarkdownRenderer;

#[test]
fn broken_words_reset_continuation_start_across_styled_zero_cell_runs() {
    let cases: serde_json::Value = serde_json::from_str(SOURCE_CASES).unwrap();
    let cases = cases.as_array().unwrap();
    assert_eq!(cases.len(), 30);
    for case in cases {
        let input = &case["input"];
        let expected: Vec<&str> = case["source"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row.as_str().unwrap())
            .collect();
        assert_eq!(
            MarkdownRenderer::render(
                input["prefix"].as_str().unwrap(),
                input["source"].as_str().unwrap(),
                input["width"].as_u64().unwrap() as usize,
                input["color"].as_bool().unwrap(),
            ),
            expected,
            "source BeginRow input: {input}"
        );
    }
}

// The 15 retained style_boundary_layout_probe mismatches from
// t70-frozen-round3-source-recapture.json, then their exact colored source rows,
// recaptured via AcceptanceProbe.cs. The other nine original passing inputs were
// not retained. Oversize first graphemes on continuation rows are intentional.
const SOURCE_CASES: &str = r#"[
{"input":{"op":"md","source":"界**́**̀ab","prefix":"p ","width":1,"color":false},"source":["p","  界","  ́̀a","  b"]},
{"input":{"op":"md","source":"界**́**̀ab","prefix":"p ","width":2,"color":false},"source":["p ","  界","  ́̀a","  b"]},
{"input":{"op":"md","source":"界**́**̀ab","prefix":"👋 ","width":1,"color":false},"source":["👋","   界","   ́̀a","   b"]},
{"input":{"op":"md","source":"界**́**̀ab","prefix":"👋 ","width":2,"color":false},"source":["👋","   界","   ́̀a","   b"]},
{"input":{"op":"md","source":"界**́**̀ab","prefix":"👋 ","width":3,"color":false},"source":["👋 ","   界","   ́̀a","   b"]},
{"input":{"op":"md","source":"界**́**̀̂ab","prefix":"p ","width":1,"color":false},"source":["p","  界","  ́̀̂a","  b"]},
{"input":{"op":"md","source":"界**́**̀̂ab","prefix":"p ","width":2,"color":false},"source":["p ","  界","  ́̀̂a","  b"]},
{"input":{"op":"md","source":"界**́**̀̂ab","prefix":"👋 ","width":1,"color":false},"source":["👋","   界","   ́̀̂a","   b"]},
{"input":{"op":"md","source":"界**́**̀̂ab","prefix":"👋 ","width":2,"color":false},"source":["👋","   界","   ́̀̂a","   b"]},
{"input":{"op":"md","source":"界**́**̀̂ab","prefix":"👋 ","width":3,"color":false},"source":["👋 ","   界","   ́̀̂a","   b"]},
{"input":{"op":"md","source":"a**́**̀界ab","prefix":"p ","width":1,"color":false},"source":["p","  a","  ́̀界","  a","  b"]},
{"input":{"op":"md","source":"a**́**̀界ab","prefix":"p ","width":2,"color":false},"source":["p ","  a","  ́̀界","  a","  b"]},
{"input":{"op":"md","source":"a**́**̀界ab","prefix":"👋 ","width":1,"color":false},"source":["👋","   a","   ́̀界","   a","   b"]},
{"input":{"op":"md","source":"a**́**̀界ab","prefix":"👋 ","width":2,"color":false},"source":["👋","   a","   ́̀界","   a","   b"]},
{"input":{"op":"md","source":"a**́**̀界ab","prefix":"👋 ","width":3,"color":false},"source":["👋 ","   a","   ́̀界","   a","   b"]},
{"input":{"op":"md","source":"界**́**̀ab","prefix":"p ","width":1,"color":true},"source":["\u001b[38;5;195mp\u001b[0m","\u001b[38;5;195m  界\u001b[0m","\u001b[38;5;195m  \u001b[0m\u001b[1;38;5;195ḿ\u001b[0m\u001b[38;5;195m̀a\u001b[0m","\u001b[38;5;195m  b\u001b[0m"]},
{"input":{"op":"md","source":"界**́**̀ab","prefix":"p ","width":2,"color":true},"source":["\u001b[38;5;195mp \u001b[0m","\u001b[38;5;195m  界\u001b[0m","\u001b[38;5;195m  \u001b[0m\u001b[1;38;5;195ḿ\u001b[0m\u001b[38;5;195m̀a\u001b[0m","\u001b[38;5;195m  b\u001b[0m"]},
{"input":{"op":"md","source":"界**́**̀ab","prefix":"👋 ","width":1,"color":true},"source":["\u001b[38;5;195m👋\u001b[0m","\u001b[38;5;195m   界\u001b[0m","\u001b[38;5;195m   \u001b[0m\u001b[1;38;5;195ḿ\u001b[0m\u001b[38;5;195m̀a\u001b[0m","\u001b[38;5;195m   b\u001b[0m"]},
{"input":{"op":"md","source":"界**́**̀ab","prefix":"👋 ","width":2,"color":true},"source":["\u001b[38;5;195m👋\u001b[0m","\u001b[38;5;195m   界\u001b[0m","\u001b[38;5;195m   \u001b[0m\u001b[1;38;5;195ḿ\u001b[0m\u001b[38;5;195m̀a\u001b[0m","\u001b[38;5;195m   b\u001b[0m"]},
{"input":{"op":"md","source":"界**́**̀ab","prefix":"👋 ","width":3,"color":true},"source":["\u001b[38;5;195m👋 \u001b[0m","\u001b[38;5;195m   界\u001b[0m","\u001b[38;5;195m   \u001b[0m\u001b[1;38;5;195ḿ\u001b[0m\u001b[38;5;195m̀a\u001b[0m","\u001b[38;5;195m   b\u001b[0m"]},
{"input":{"op":"md","source":"界**́**̀̂ab","prefix":"p ","width":1,"color":true},"source":["\u001b[38;5;195mp\u001b[0m","\u001b[38;5;195m  界\u001b[0m","\u001b[38;5;195m  \u001b[0m\u001b[1;38;5;195ḿ\u001b[0m\u001b[38;5;195m̀̂a\u001b[0m","\u001b[38;5;195m  b\u001b[0m"]},
{"input":{"op":"md","source":"界**́**̀̂ab","prefix":"p ","width":2,"color":true},"source":["\u001b[38;5;195mp \u001b[0m","\u001b[38;5;195m  界\u001b[0m","\u001b[38;5;195m  \u001b[0m\u001b[1;38;5;195ḿ\u001b[0m\u001b[38;5;195m̀̂a\u001b[0m","\u001b[38;5;195m  b\u001b[0m"]},
{"input":{"op":"md","source":"界**́**̀̂ab","prefix":"👋 ","width":1,"color":true},"source":["\u001b[38;5;195m👋\u001b[0m","\u001b[38;5;195m   界\u001b[0m","\u001b[38;5;195m   \u001b[0m\u001b[1;38;5;195ḿ\u001b[0m\u001b[38;5;195m̀̂a\u001b[0m","\u001b[38;5;195m   b\u001b[0m"]},
{"input":{"op":"md","source":"界**́**̀̂ab","prefix":"👋 ","width":2,"color":true},"source":["\u001b[38;5;195m👋\u001b[0m","\u001b[38;5;195m   界\u001b[0m","\u001b[38;5;195m   \u001b[0m\u001b[1;38;5;195ḿ\u001b[0m\u001b[38;5;195m̀̂a\u001b[0m","\u001b[38;5;195m   b\u001b[0m"]},
{"input":{"op":"md","source":"界**́**̀̂ab","prefix":"👋 ","width":3,"color":true},"source":["\u001b[38;5;195m👋 \u001b[0m","\u001b[38;5;195m   界\u001b[0m","\u001b[38;5;195m   \u001b[0m\u001b[1;38;5;195ḿ\u001b[0m\u001b[38;5;195m̀̂a\u001b[0m","\u001b[38;5;195m   b\u001b[0m"]},
{"input":{"op":"md","source":"a**́**̀界ab","prefix":"p ","width":1,"color":true},"source":["\u001b[38;5;195mp\u001b[0m","\u001b[38;5;195m  a\u001b[0m","\u001b[38;5;195m  \u001b[0m\u001b[1;38;5;195ḿ\u001b[0m\u001b[38;5;195m̀界\u001b[0m","\u001b[38;5;195m  a\u001b[0m","\u001b[38;5;195m  b\u001b[0m"]},
{"input":{"op":"md","source":"a**́**̀界ab","prefix":"p ","width":2,"color":true},"source":["\u001b[38;5;195mp \u001b[0m","\u001b[38;5;195m  a\u001b[0m","\u001b[38;5;195m  \u001b[0m\u001b[1;38;5;195ḿ\u001b[0m\u001b[38;5;195m̀界\u001b[0m","\u001b[38;5;195m  a\u001b[0m","\u001b[38;5;195m  b\u001b[0m"]},
{"input":{"op":"md","source":"a**́**̀界ab","prefix":"👋 ","width":1,"color":true},"source":["\u001b[38;5;195m👋\u001b[0m","\u001b[38;5;195m   a\u001b[0m","\u001b[38;5;195m   \u001b[0m\u001b[1;38;5;195ḿ\u001b[0m\u001b[38;5;195m̀界\u001b[0m","\u001b[38;5;195m   a\u001b[0m","\u001b[38;5;195m   b\u001b[0m"]},
{"input":{"op":"md","source":"a**́**̀界ab","prefix":"👋 ","width":2,"color":true},"source":["\u001b[38;5;195m👋\u001b[0m","\u001b[38;5;195m   a\u001b[0m","\u001b[38;5;195m   \u001b[0m\u001b[1;38;5;195ḿ\u001b[0m\u001b[38;5;195m̀界\u001b[0m","\u001b[38;5;195m   a\u001b[0m","\u001b[38;5;195m   b\u001b[0m"]},
{"input":{"op":"md","source":"a**́**̀界ab","prefix":"👋 ","width":3,"color":true},"source":["\u001b[38;5;195m👋 \u001b[0m","\u001b[38;5;195m   a\u001b[0m","\u001b[38;5;195m   \u001b[0m\u001b[1;38;5;195ḿ\u001b[0m\u001b[38;5;195m̀界\u001b[0m","\u001b[38;5;195m   a\u001b[0m","\u001b[38;5;195m   b\u001b[0m"]}
]"#;
