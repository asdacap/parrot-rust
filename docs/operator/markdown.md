# Enhanced CLI Markdown output

The enhanced CLI uses [Termimad](https://docs.rs/termimad/0.35.5/termimad/) for Markdown parsing and terminal layout. Fenced code uses [Syntect](https://docs.rs/syntect/5.3.0/syntect/) with [two-face](https://docs.rs/two-face/0.5.1/two_face/) grammars and the bundled `base16-ocean.dark` theme. Grammar and theme assets are local dependencies; rendering does not download them at runtime. The regex backend is pure Rust, without Oniguruma.

This applies to assistant responses, reasoning summaries, child finals, plan reports and Markdown-backed YAML previews. `parrot chat --basic` still selects the separate minimal renderer.

## Syntax and presentation

Termimad's Markdown dialect is not a full CommonMark or GitHub Flavored Markdown implementation. It replaces the CLI's former hand-written dialect; exact old colors and layouts are not retained.

Notable differences:

- First-level headings are centered by the default skin. Quote markers, table borders, padding and wrapping differ from the former renderer.
- Emphasis uses Termimad's supported delimiters. Underscore emphasis and empty-delimiter escaping do not follow the old custom rules.
- Links do not receive the former `[label](url)` to `label (url)` conversion. Task checkboxes do not receive the former checkbox-glyph conversion.
- Tables start with `|`. Pipe-less tables are not recognized. Narrow tables no longer use the former `Header: value` fallback.
- Fenced code starts at column zero with triple backticks. Tilde fences and the former generalized indentation/marker-length rules are not supported. Termimad also recognizes four-space-indented code.
- Ordered-list numbering follows the library. Non-ASCII decimal markers do not receive the former custom ordered-list treatment.
- Language-aware highlighting replaces the old language-independent keyword list. Tokens and colors therefore differ, including comments and multiline strings.

Known language names and common aliases select bundled grammars, including Rust, TypeScript, Kotlin, Swift, C# aliases, Go, Python and shell spellings. Missing or unknown languages remain plain code. Highlighting errors and blocks beyond 10,000 lines or 512 Ki UTF-16 units fall back to plain code. The unit count includes one newline unit per logical code line. These limits affect highlighting, not whether code text is displayed.

No-color output keeps the same visible text and geometry as colored output. The CLI sanitizes untrusted terminal controls and expands tabs before Markdown parsing. Prefixes are outside Markdown parsing and reserve their cell width once. Shared CLI grapheme metrics handle narrow widths and complex Unicode; an indivisible grapheme or an already overwide prefix may exceed the available columns rather than being split or discarded.

## Streaming and flushing

The CLI still owns the pending preview and committed scrollback. Completed ordinary regions can be promoted while a response streams. Open code fences and tables stay pending until a safe boundary. Ordered-list blocks, blank separators and indented continuations now stay pending until a complete following block boundary or response completion. A partial following line does not close the block. This avoids numbering changes caused by incoming token boundaries.

Pending text remains a raw preview, not partially rendered Markdown. Response completion, EOF, cancellation and stream errors commit the remaining text once through the existing response lifecycle. Final JSON-to-YAML conversion still occurs at that boundary where eligible.

JSON object keys are always double-quoted YAML strings, including ordinary names, numbers, dates and tag-looking text. Key formatting does not consult .NET date parsing or the local timezone. Long serialized keys and multiline keys use explicit YAML key syntax. Member order, numeric spelling and value formatting are retained; malformed JSON and duplicate keys fall back to the original text.

Markdown formatting does not write to stdout, discover terminal size, move the cursor or flush a writer. It returns lines to the existing terminal frame renderer, which owns terminal writes and flushing.

## Implementation and retained tests

The adapter is in [`terminal_layout/markdown.rs`](../../crates/parrot-cli/src/terminal_layout/markdown.rs). Termimad handles ordinary Markdown. A code-only span wrapper preserves highlighting across logical code lines before wrapping. The same grapheme wrapper handles widths that the library cannot safely fit, including wide list markers and narrow tables. Trusted library SGR is decoded with bounded style state; untrusted input never enters that decoder.

The custom Markdown parser, table renderer, generalized fence helpers and token recognizer were removed. Shared `TerminalText` layout and terminal frame ownership are unchanged. Final JSON/YAML formatting uses quoted string keys instead of the former CLR date and decimal classifiers.

Legacy .NET/source-parity snapshots remain intact. Incompatible cases are individually ignored with reasons instead of being rewritten to match the new renderer. Mixed fixture suites expose each case separately so unchanged `TerminalText`, YAML, blank-output and fence behavior remains tested. Active adapter and live-stream tests cover the new library behavior and retained safety/lifecycle requirements.
