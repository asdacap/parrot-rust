use super::*;

impl MemoryState {
    fn add_skill(&mut self, name: &str, content: SkillContent) {
        self.skill_rows.push((
            Skill {
                id: format!("/physical/{name}/SKILL.md"),
                name: name.into(),
                path: format!("/skills/{name}/SKILL.md"),
            },
            content,
        ));
    }
}

#[test]
fn mention_syntax_preserves_boundaries_code_environment_exclusions_and_utf16_offsets() {
    for (input, expected) in [
        ("$9skill", "9skill"),
        ("($skill-name).", "skill-name"),
        ("$plugin:skill", "plugin:skill"),
        ("email$x", ""),
        ("$PATH and $skill", "skill"),
        (
            "$HOME $PATH $USER $SHELL $PWD $TMP $TMPDIR $TEMP $LANG $TERM $XDG_CONFIG_HOME",
            "",
        ),
        ("$home $PATHS", "home,PATHS"),
        ("é$skill _embedded$skip :$ok", "skill,ok"),
        (
            "`$inline`\n```\n$fenced\n```\n\\$escaped",
            "inline,fenced,escaped",
        ),
    ] {
        assert_eq!(
            parse_skill_mentions(input)
                .iter()
                .map(|mention| mention.name.as_str())
                .collect::<Vec<_>>()
                .join(","),
            expected,
            "{input}"
        );
    }
    let mentions = parse_skill_mentions("😀 [$ same ]( /second/SKILL.md ) then $other");
    assert_eq!(
        mentions[0],
        SkillMention {
            name: "same".into(),
            path: Some("/second/SKILL.md".into()),
            position: 3,
        }
    );
    assert_eq!(mentions[1].position, 38);
    assert_eq!(
        parse_skill_mentions("[$HOME](/skill/SKILL.md)")[0].name,
        "HOME"
    );
    assert_eq!(parse_skill_mentions("[$same](/path)x")[0].path, None);
    assert_eq!(parse_skill_mentions("[$ ]( )")[0].path.as_deref(), Some(""));
}

#[test]
fn core_queries_text_mentions_and_reads_each_selected_record_once_in_text_order() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.pending_messages.clear();
    state.enqueue_message(Delivery::Steer, "unused");
    state.pending_messages[0].1.contents = vec![
        Content::Image {
            path: "$ignored".into(),
            url: "$ignored".into(),
            media_type: "image/png".into(),
            width: 1,
            height: 1,
            bytes: None,
        },
        Content::Text("😀 $second".into()),
        Content::Text("[$first](/skills/first/SKILL.md) $second $missing $PATH".into()),
    ];
    state.add_skill(
        "first",
        SkillContent::Text("---\nname: first\n---\nFIRST BODY {name}".into()),
    );
    state.add_skill("second", SkillContent::Text("SECOND BODY".into()));
    state.add_skill("ignored", SkillContent::Text("not input text".into()));

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|step| *step == "resolve_skill_references")
            .count(),
        1
    );
    assert_eq!(
        *state.skill_lookups.lock().unwrap(),
        [
            SkillReference {
                name: "second".into(),
                path: None
            },
            SkillReference {
                name: "first".into(),
                path: Some("/skills/first/SKILL.md".into())
            },
            SkillReference {
                name: "second".into(),
                path: None
            },
            SkillReference {
                name: "missing".into(),
                path: None
            },
        ]
    );
    assert_eq!(
        *state.skill_reads.lock().unwrap(),
        ["/physical/second/SKILL.md", "/physical/first/SKILL.md",]
    );
    let context = &state.recorded_input_contexts[0];
    assert_eq!(
        state.loaded_skill_paths(),
        ["/skills/second/SKILL.md", "/skills/first/SKILL.md"]
    );
    assert!(
        context.content.find("SECOND BODY").unwrap() < context.content.find("FIRST BODY").unwrap()
    );
    assert!(
        context
            .content
            .contains("---\nname: first\n---\nFIRST BODY {name}")
    );
    assert_eq!(
        state.provider.requests.lock().unwrap()[0].messages[0].contents,
        [Content::Text(context.content.clone())]
    );
}

#[test]
fn separate_inputs_use_local_offsets_and_utf16_identity_tiebreak_before_body_reads() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.pending_messages.clear();
    state.enqueue_message(Delivery::Steer, "$later");
    state.enqueue_message(Delivery::Steer, "$earlier");
    state.skill_rows = vec![
        (
            Skill {
                id: "\u{e000}".into(),
                name: "later".into(),
                path: "/later".into(),
            },
            SkillContent::Text("later".into()),
        ),
        (
            Skill {
                id: "\u{10000}".into(),
                name: "earlier".into(),
                path: "/earlier".into(),
            },
            SkillContent::Text("earlier".into()),
        ),
    ];
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|step| *step == "resolve_skill_references")
            .count(),
        2
    );
    assert_eq!(
        *state.skill_reads.lock().unwrap(),
        ["\u{10000}", "\u{e000}"]
    );
    assert_eq!(state.loaded_skill_paths(), ["/earlier", "/later"]);
}

#[test]
fn canonical_identity_deduplicates_a_steer_and_reloads_on_the_next_turn() {
    let mut state = MemoryState::new([
        Reply::tools("call"),
        Reply::final_response("first answer"),
        Reply::final_response("second answer"),
    ]);
    state.pending_messages.clear();
    state.enqueue_message(Delivery::Steer, "$selected");
    state.enqueue_message(Delivery::Queue, "$selected");
    state
        .steers_after_results
        .push_back("[$selected](/physical/selected/SKILL.md)".into());
    state.add_skill("selected", SkillContent::Text("SELECTED BODY".into()));

    assert_eq!(state.run(), Ok(Outcome::Completed("second answer".into())));
    assert_eq!(
        *state.skill_reads.lock().unwrap(),
        ["/physical/selected/SKILL.md", "/physical/selected/SKILL.md",]
    );
    assert_eq!(state.recorded_input_contexts.len(), 2);
    assert_eq!(state.skill_lookups.lock().unwrap().len(), 3);
    assert!(state.pending_messages.is_empty());
    assert_eq!(state.provider.requests.lock().unwrap().len(), 3);
}

#[test]
fn core_uses_configured_templates_without_interpreting_skill_body_text() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.pending_messages.clear();
    state.enqueue_message(Delivery::Steer, "$selected");
    state.add_skill(
        "selected",
        SkillContent::Text("literal {name} & <body>".into()),
    );
    state.skill_templates.lock().unwrap().insert(
        ConfigKey::SkillSelectedTemplate,
        "configured [{name}] => {content}".into(),
    );
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(
        state.recorded_input_contexts[0].content,
        "configured [selected] => literal {name} & <body>"
    );
}

#[test]
fn unavailable_and_oversized_records_produce_notices_without_loaded_paths() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.pending_messages.clear();
    state.enqueue_message(
        Delivery::Steer,
        "[$<bad&>](/logical/<bad&>/SKILL.md) $large $boundary",
    );
    state.skill_rows.push((
        Skill {
            id: "bad-row".into(),
            name: "<bad&>".into(),
            path: "/logical/<bad&>/SKILL.md".into(),
        },
        SkillContent::Unavailable("  denied\n <target> & details  ".into()),
    ));
    state.add_skill("large", SkillContent::Text("界".repeat(3000)));
    state.add_skill("boundary", SkillContent::Text("x".repeat(8192)));
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    let context = &state.recorded_input_contexts[0];
    assert_eq!(state.loaded_skill_paths(), ["/skills/boundary/SKILL.md"]);
    assert!(context.content.contains("<name>&lt;bad&amp;&gt;</name>"));
    assert!(
        context
            .content
            .contains("denied &lt;target&gt; &amp; details")
    );
    assert!(context.content.contains("<skill-too-large>"));
    assert!(context.content.contains("8192-byte injection limit"));
    assert!(context.content.contains(&"x".repeat(8192)));
    assert!(!context.content.contains(&"界".repeat(3000)));
}

#[test]
fn diagnostics_are_normalized_and_limited_in_utf16_without_splitting_unicode() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.pending_messages.clear();
    state.enqueue_message(Delivery::Steer, "$denied");
    state.add_skill(
        "denied",
        SkillContent::Unavailable(format!(" \n {} tail", "😀".repeat(600))),
    );
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    let context = &state.recorded_input_contexts[0];
    assert!(context.content.contains(&"😀".repeat(512)));
    assert!(!context.content.contains(&"😀".repeat(513)));
    assert!(!context.content.contains("tail"));
    assert!(state.loaded_skill_paths().is_empty());
}

#[test]
fn combined_budget_keeps_complete_skill_bodies_and_reports_omitted_instructions() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.pending_messages.clear();
    let names = (0..150)
        .map(|index| format!("large-{index}"))
        .collect::<Vec<_>>();
    state.enqueue_message(
        Delivery::Steer,
        &names
            .iter()
            .map(|name| format!("${name}"))
            .collect::<Vec<_>>()
            .join(" "),
    );
    for name in names {
        state.add_skill(&name, SkillContent::Text("x".repeat(7900)));
    }
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    let context = &state.recorded_input_contexts[0];
    assert!(context.content.len() <= 1024 * 1024);
    assert!(!state.loaded_skill_paths().is_empty());
    assert!(state.loaded_skill_paths().len() < 150);
    assert!(context.content.contains("active-turn prompt budget"));
    assert_eq!(
        context.content.matches(&"x".repeat(7900)).count(),
        state.loaded_skill_paths().len()
    );
    assert_eq!(state.skill_reads.lock().unwrap().len(), 150);
}

#[test]
fn combined_diagnostic_budget_reserves_space_for_an_omitted_summary() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.pending_messages.clear();
    let names = (0..1200)
        .map(|index| format!("denied-{index}"))
        .collect::<Vec<_>>();
    state.enqueue_message(
        Delivery::Steer,
        &names
            .iter()
            .map(|name| format!("${name}"))
            .collect::<Vec<_>>()
            .join(" "),
    );
    for name in names {
        state.add_skill(&name, SkillContent::Unavailable("x".repeat(2000)));
    }
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    let context = &state.recorded_input_contexts[0];
    assert!(context.content.len() <= 1024 * 1024);
    assert!(
        context
            .content
            .contains("diagnostics omitted by the active-turn prompt budget")
    );
    assert!(state.loaded_skill_paths().is_empty());
    assert!(context.content.matches("<skill-unavailable>").count() < 1200);
}

#[test]
fn cancellation_after_metadata_lookup_stops_before_body_read_or_context_recording() {
    let mut state = MemoryState::new([Reply::final_response("not invoked")]);
    state.pending_messages.clear();
    state.enqueue_message(Delivery::Steer, "$selected");
    state.add_skill("selected", SkillContent::Text("instructions".into()));
    state.fail_at = Some("cancel_get_skills");
    assert_eq!(state.run(), Err(Error::cancelled()));
    assert_eq!(state.skill_lookups.lock().unwrap().len(), 1);
    assert!(state.skill_reads.lock().unwrap().is_empty());
    assert!(state.recorded_input_contexts.is_empty());
    assert!(state.provider.requests.lock().unwrap().is_empty());
}

#[test]
fn unknown_skills_do_not_read_content_or_expand_templates() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.pending_messages.clear();
    state.enqueue_message(Delivery::Steer, "$missing");
    state.fail_at = Some("render_template");
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(
        *state.skill_lookups.lock().unwrap(),
        [SkillReference {
            name: "missing".into(),
            path: None
        }]
    );
    assert!(state.skill_reads.lock().unwrap().is_empty());
    assert!(state.recorded_input_contexts.is_empty());
}

#[test]
fn an_unusable_diagnostic_template_fails_before_any_content_read() {
    let mut state = MemoryState::new([Reply::final_response("not invoked")]);
    state.pending_messages.clear();
    state.enqueue_message(Delivery::Steer, "$selected");
    state.add_skill("selected", SkillContent::Text("instructions".into()));
    state
        .skill_templates
        .lock()
        .unwrap()
        .insert(ConfigKey::SkillUnavailableTemplate, "x".repeat(1024 * 1024));
    assert_eq!(
        state.run(),
        Err(Error::failed(
            "The unavailable-skill prompt template exceeds the active-turn prompt budget."
        ))
    );
    assert!(state.skill_reads.lock().unwrap().is_empty());
    assert!(state.recorded_input_contexts.is_empty());
    assert!(state.provider.requests.lock().unwrap().is_empty());
}
