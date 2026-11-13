# Enhanced CLI Markdown output

The enhanced CLI displays Markdown using [Termimad's syntax](https://docs.rs/termimad/0.35.5/termimad/) and highlights fenced code with the `base16-ocean.dark` theme. Its bundled grammars and theme work without runtime downloads.

This applies to assistant responses, reasoning summaries, child finals, plan reports and Markdown-backed YAML previews. `parrot chat --basic` still selects the separate minimal renderer.

## Syntax and presentation

Termimad's Markdown dialect is not a full CommonMark or GitHub Flavored Markdown implementation. It replaces the CLI's former hand-written dialect. Termimad does not retain the exact old colors and layouts.

Notable differences:

- The default skin centers first-level headings. Quote markers, table borders, padding and wrapping differ from the former renderer.
- Emphasis uses Termimad's supported delimiters. Underscore emphasis and empty-delimiter escaping do not follow the old custom rules.
- Links do not receive the former `[label](url)` to `label (url)` conversion. Task checkboxes do not receive the former checkbox-glyph conversion.
- Tables start with `|`. Pipe-less tables are not recognized. Narrow tables no longer use the former `Header: value` fallback.
- Fenced code starts at column zero with triple backticks. Tilde fences and the former generalized indentation and marker-length rules are not supported. Termimad also recognizes four-space-indented code.
- Ordered-list numbering follows the library. Non-ASCII decimal markers do not receive the former custom ordered-list treatment.
- Language-aware highlighting replaces the old language-independent keyword list. Tokens and colors therefore differ, including comments and multiline strings.

Known language names and common aliases select bundled grammars, including Rust, TypeScript, Kotlin, Swift, C# aliases, Go, Python and shell spellings. Missing or unknown languages remain plain code. Highlighting errors and blocks beyond 10,000 lines or 512 Ki UTF-16 units fall back to plain code. The unit count includes one newline unit per logical code line. These limits affect highlighting, not whether code text is displayed.

No-color output keeps the same visible text and geometry as colored output. The CLI sanitizes untrusted terminal controls and expands tabs before Markdown parsing. Prefixes are outside Markdown parsing and reserve their cell width once. Shared CLI grapheme metrics handle narrow widths and complex Unicode. An indivisible grapheme or an already overwide prefix may exceed the available columns rather than being split or discarded.

## Streaming and flushing

The CLI still owns the pending preview and committed scrollback. Completed ordinary regions can be promoted while a response streams. Open code fences and tables stay pending until a safe boundary. Ordered-list blocks, blank separators and indented continuations now stay pending until a complete following block boundary or response completion. A partial following line does not close the block. This avoids numbering changes caused by incoming token boundaries.

Pending text remains a raw preview, not partially rendered Markdown. Response completion, EOF, cancellation and stream errors commit the remaining text once through the existing response lifecycle. Final JSON-to-YAML conversion still occurs at that boundary where eligible.

JSON object keys are always double-quoted YAML strings, including ordinary names, numbers, dates and tag-looking text. Key formatting does not interpret keys as dates or consult the local timezone. Long serialized keys and multiline keys use explicit YAML key syntax. Member order, numeric spelling and value formatting are retained. Malformed JSON and duplicate keys fall back to the original text.

Code highlighting continues across logical lines when a code block wraps. Untrusted
input cannot supply terminal-control sequences through Markdown or code highlighting.
