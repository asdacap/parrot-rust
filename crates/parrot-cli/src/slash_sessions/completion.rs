use std::collections::HashSet;

use parrot_domain::{
    contracts::async_ops::Cancellation,
    protocol::messages::parrot::{ListSkillsRequest, Skill},
};
use parrot_host::web::router::WebClient;

use crate::terminal_input::{EditorRangeError, IncrementalEditor, PromptState};

use super::{CommandDescription, SlashCommandRegistry, SlashFailure, native_call};

/// Completes the registry's first slash token, retaining the selected command's
/// actual registry index rather than identifying commands by their spelling.
#[derive(Default)]
pub struct SlashCommandCompletion {
    indices: Vec<usize>,
    commands: Vec<CommandDescription>,
    selected: usize,
}

impl SlashCommandCompletion {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn commands(&self) -> &[CommandDescription] {
        &self.commands
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn refresh(&mut self, entered: &str, registry: &SlashCommandRegistry) {
        let selected = self.indices.get(self.selected).copied();
        self.indices = if entered.contains([' ', '\t', '\r', '\n']) {
            Vec::new()
        } else {
            registry.complete_indices(entered)
        };
        self.commands = self
            .indices
            .iter()
            .map(|index| {
                let command = &registry.commands()[*index];
                CommandDescription {
                    name: command.name().to_owned(),
                    summary: command.summary().to_owned(),
                }
            })
            .collect();
        self.selected = selected
            .and_then(|index| {
                self.indices
                    .iter()
                    .position(|candidate| *candidate == index)
            })
            .unwrap_or(0);
    }

    pub fn select_previous(&mut self) {
        if !self.commands.is_empty() {
            self.selected = (self.selected + self.commands.len() - 1) % self.commands.len();
        }
    }

    pub fn select_next(&mut self) {
        if !self.commands.is_empty() {
            self.selected = (self.selected + 1) % self.commands.len();
        }
    }

    pub fn accept(&self, entered: &str) -> Option<String> {
        let command = self.commands.get(self.selected)?;
        let end = entered
            .find([' ', '\t', '\r', '\n'])
            .unwrap_or(entered.len());
        Some(format!("{}{}", command.name, &entered[end..]))
    }
}

/// Session-scoped skills are loaded by the actual generated native client. The
/// state below is also the state used by the real rune-indexed editor adapter.
pub struct SkillCompletion {
    client: WebClient,
    state: SkillCompletionState,
}

impl SkillCompletion {
    pub fn new(client: WebClient) -> Self {
        Self {
            client,
            state: SkillCompletionState::default(),
        }
    }

    pub fn skills(&self) -> &[Skill] {
        &self.state.matches
    }

    pub fn selected(&self) -> usize {
        self.state.selected
    }

    pub fn description(skill: &Skill) -> &str {
        skill
            .short_description
            .as_deref()
            .filter(|text| !text.is_empty())
            .unwrap_or(&skill.description)
    }

    pub async fn refresh_catalog(
        &mut self,
        user_session_id: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<(), SlashFailure> {
        let mut client = self.client.clone();
        let response = native_call(
            client.list_skills(ListSkillsRequest {
                user_session_id: user_session_id.to_owned(),
                ..Default::default()
            }),
            cancellation,
        )
        .await;
        match response {
            Ok(listed) => self.state.replace_catalog(&listed.skills),
            Err(SlashFailure::Rpc(_)) => self.state.replace_catalog(&[]),
            Err(failure) => return Err(failure),
        }
        Ok(())
    }

    pub fn refresh(&mut self, prompt: &PromptState) {
        self.state.refresh(prompt);
    }

    pub fn select_previous(&mut self) {
        self.state.select_previous();
    }

    pub fn select_next(&mut self) {
        self.state.select_next();
    }

    pub fn accept(&self, editor: &mut IncrementalEditor) -> Result<bool, EditorRangeError> {
        self.state.accept(editor)
    }
}

#[derive(Default)]
struct SkillCompletionState {
    available: Vec<Skill>,
    matches: Vec<Skill>,
    token: Option<SkillToken>,
    selected: usize,
}

impl SkillCompletionState {
    fn replace_catalog(&mut self, skills: &[Skill]) {
        let mut names = HashSet::new();
        self.available = skills
            .iter()
            .filter(|skill| skill.enabled && names.insert(skill.name.as_str()))
            .cloned()
            .collect();
        self.available.sort_by(|left, right| {
            left.name
                .encode_utf16()
                .cmp(right.name.encode_utf16())
                .then_with(|| left.path.encode_utf16().cmp(right.path.encode_utf16()))
        });
        self.matches.clear();
        self.token = None;
        self.selected = 0;
    }

    fn refresh(&mut self, prompt: &PromptState) {
        let selected = self
            .matches
            .get(self.selected)
            .map(|skill| skill.path.clone());
        self.token = find_token(prompt);
        self.matches = match &self.token {
            Some(token) => self
                .available
                .iter()
                .filter(|skill| {
                    // The token grammar is ASCII. Valid catalog names are not
                    // restricted to ASCII: the .NET OrdinalIgnoreCase treatment of
                    // non-ASCII names versus ASCII prefixes remains explicitly OPEN
                    // in T67's source map; do not substitute Unicode lowercasing.
                    skill
                        .name
                        .get(..token.prefix.len())
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&token.prefix))
                })
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        self.selected = selected
            .and_then(|path| self.matches.iter().position(|skill| skill.path == path))
            .unwrap_or(0);
    }

    fn select_previous(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + self.matches.len() - 1) % self.matches.len();
        }
    }

    fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    fn accept(&self, editor: &mut IncrementalEditor) -> Result<bool, EditorRangeError> {
        let (Some(skill), Some(token)) = (self.matches.get(self.selected), self.token.as_ref())
        else {
            return Ok(false);
        };
        editor.replace_range(token.start, token.length, &format!("${}", skill.name))?;
        Ok(true)
    }
}

struct SkillToken {
    start: usize,
    length: usize,
    prefix: String,
}

fn find_token(prompt: &PromptState) -> Option<SkillToken> {
    let runes: Vec<_> = prompt.text.chars().collect();
    let cursor = prompt.cursor.clamp(0, runes.len() as isize) as usize;
    let mut start = cursor;
    while start > 0 && is_name(runes[start - 1]) {
        start -= 1;
    }
    if start == 0 || runes[start - 1] != '$' {
        return None;
    }
    let dollar = start - 1;
    if dollar > 0 && (runes[dollar - 1].is_ascii_alphanumeric() || runes[dollar - 1] == '_') {
        return None;
    }
    let mut end = cursor;
    while end < runes.len() && is_name(runes[end]) {
        end += 1;
    }
    let token: String = runes[start..end].iter().collect();
    if ENVIRONMENT_NAMES.contains(&token.as_str()) {
        return None;
    }
    Some(SkillToken {
        start: dollar,
        length: end - dollar,
        prefix: runes[start..cursor].iter().collect(),
    })
}

fn is_name(rune: char) -> bool {
    rune.is_ascii_alphanumeric() || matches!(rune, '_' | '-')
}

const ENVIRONMENT_NAMES: &[&str] = &[
    "HOME",
    "PATH",
    "USER",
    "SHELL",
    "PWD",
    "TMP",
    "TMPDIR",
    "TEMP",
    "LANG",
    "TERM",
    "XDG_CONFIG_HOME",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_input::{TerminalKey, TerminalKeyKind};

    fn skill(name: &str, path: &str, enabled: bool) -> Skill {
        Skill {
            name: name.to_owned(),
            path: path.to_owned(),
            enabled,
            description: format!("{name} description"),
            ..Default::default()
        }
    }

    fn prompt(text: &str, cursor: isize) -> PromptState {
        PromptState {
            prefix: "> ".into(),
            text: text.into(),
            cursor,
        }
    }

    fn names(state: &SkillCompletionState) -> Vec<&str> {
        state
            .matches
            .iter()
            .map(|skill| skill.name.as_str())
            .collect()
    }

    #[test]
    fn skill_completion_filters_enabled_and_replaces_only_active_rune_token() {
        let mut state = SkillCompletionState::default();
        state.replace_catalog(&[
            skill("zebra", "/zebra", true),
            skill("alpha", "/alpha", true),
            skill("alpine", "/alpine", false),
        ]);
        let mut editor = IncrementalEditor::new("> ", 100);
        editor.apply(TerminalKey::with_text(
            TerminalKeyKind::Paste,
            "🙂 use $al suffix",
        ));
        for _ in 0..7 {
            editor.apply(TerminalKey::new(TerminalKeyKind::Left));
        }
        state.refresh(&editor.prompt());
        assert!(state.accept(&mut editor).unwrap());
        assert_eq!(editor.prompt(), prompt("🙂 use $alpha suffix", 12));
        assert_eq!(names(&state), ["alpha"]);
        state.refresh(&prompt("word$al", 7));
        assert!(state.matches.is_empty());
        for cursor in [1, 3, 5] {
            state.refresh(&prompt("$HOME", cursor));
            assert!(state.matches.is_empty());
        }
    }

    #[test]
    fn skill_completion_sorts_wraps_retains_path_and_resets_session_catalog() {
        let mut state = SkillCompletionState::default();
        state.replace_catalog(&[
            skill("beta", "/beta", true),
            skill("alpha", "/alpha", true),
            skill("alpha", "/duplicate", true),
        ]);
        state.refresh(&prompt("$", 1));
        state.select_next();
        state.refresh(&prompt("$", 1));
        assert_eq!(
            state
                .matches
                .iter()
                .map(|skill| (skill.name.as_str(), skill.path.as_str()))
                .collect::<Vec<_>>(),
            [("alpha", "/alpha"), ("beta", "/beta")]
        );
        assert_eq!(state.selected, 1);
        state.select_next();
        assert_eq!(state.selected, 0);
        state.select_previous();
        assert_eq!(state.selected, 1);
        state.replace_catalog(&[skill("other", "/other", true)]);
        assert!(state.matches.is_empty());
        assert!(state.token.is_none());
        assert_eq!(state.selected, 0);
        state.refresh(&prompt("$", 1));
        assert_eq!(names(&state), ["other"]);
    }

    #[test]
    fn skill_catalog_keeps_first_enabled_exact_name_and_clones_all_unicode_data() {
        let mut original = vec![
            skill("alpha", "/disabled", false),
            skill("alpha", "/first", true),
            skill("alpha", "/second", true),
            skill("Alpha", "/uppercase", true),
            skill("\u{e000}", "/bmp", true),
            skill("\u{10000}", "/supplementary", true),
        ];
        let mut state = SkillCompletionState::default();
        state.replace_catalog(&original);
        original[1].name = "mutated".into();
        state.refresh(&prompt("$", 1));
        assert_eq!(names(&state), ["Alpha", "alpha", "\u{10000}", "\u{e000}"]);
        assert_eq!(state.matches[1].path, "/first");
        state.refresh(&prompt("$AL", 3));
        assert_eq!(names(&state), ["Alpha", "alpha"]);
    }

    #[test]
    fn skill_token_clamps_cursor_and_preserves_source_ascii_boundary_rules() {
        assert!(find_token(&prompt("$alpha", -1)).is_none());
        assert_eq!(find_token(&prompt("$alpha", 500)).unwrap().prefix, "alpha");
        for text in ["x$alpha", "_$alpha", "9$alpha", "word$", "$alpha/"] {
            assert!(
                find_token(&prompt(text, text.chars().count() as isize)).is_none(),
                "{text}"
            );
        }
        for text in ["-$alpha", "🙂$alpha", "é$alpha", "($alpha", "$a_b-c9"] {
            assert!(
                find_token(&prompt(text, text.chars().count() as isize)).is_some(),
                "{text}"
            );
        }
        for name in ENVIRONMENT_NAMES {
            let text = format!("${name}");
            for cursor in 1..=text.len() {
                assert!(
                    find_token(&prompt(&text, cursor as isize)).is_none(),
                    "{text}@{cursor}"
                );
            }
        }
        assert!(find_token(&prompt("$home", 5)).is_some());
        assert!(find_token(&prompt("$HOME-tail", 4)).is_some());
    }

    #[test]
    fn skill_accept_replaces_the_full_token_at_middle_and_keeps_editor_history() {
        let mut state = SkillCompletionState::default();
        state.replace_catalog(&[skill("alpha", "/alpha", true)]);
        let mut editor = IncrementalEditor::new("> ", 100);
        editor.remember("earlier input");
        editor.replace("🙂 $alpine suffix");
        for _ in 0..11 {
            editor.apply(TerminalKey::new(TerminalKeyKind::Left));
        }
        assert_eq!(editor.prompt().cursor, 5);
        state.refresh(&editor.prompt());
        assert!(state.accept(&mut editor).unwrap());
        assert_eq!(editor.prompt(), prompt("🙂 $alpha suffix", 8));
        editor.recall();
        assert_eq!(editor.prompt().text, "earlier input");
        state.replace_catalog(&[]);
        assert!(!state.accept(&mut editor).unwrap());
        state.select_next();
        state.select_previous();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn skill_description_uses_nonempty_optional_short_description_only() {
        let mut item = skill("alpha", "/alpha", true);
        assert_eq!(SkillCompletion::description(&item), "alpha description");
        item.short_description = Some(String::new());
        assert_eq!(SkillCompletion::description(&item), "alpha description");
        item.short_description = Some("short alpha".into());
        assert_eq!(SkillCompletion::description(&item), "short alpha");
    }
}
