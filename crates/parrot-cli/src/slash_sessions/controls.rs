use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot::{ListSkillsResponse, ListedSkillScope, Skill},
    values::context::ContextSize,
};

use crate::dispatch::dialog::DialogOption;

use super::{CommandContext, SlashCommand, SlashFailure, load_command};

pub struct CompactCommand;

impl SlashCommand for CompactCommand {
    fn name(&self) -> &str {
        "/compact"
    }

    fn summary(&self) -> &str {
        "Compact the current session"
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let target = match compact_target(arguments) {
                Ok(target) => target,
                Err(()) => {
                    context
                        .dialog
                        .show_error("usage: /compact", cancellation)
                        .await?;
                    return Ok(());
                }
            };
            context.activity.wait_until_idle(cancellation).await?;
            if let Err(failure) = context.session.compact(target, cancellation).await {
                context
                    .dialog
                    .show_error(
                        &format!("compaction failed: {}", exception_message(&failure)),
                        cancellation,
                    )
                    .await?;
            }
            Ok(())
        })
    }
}

fn compact_target(arguments: &str) -> Result<Option<&str>, ()> {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return if arguments.is_empty() {
            Ok(None)
        } else {
            Err(())
        };
    }
    if trimmed.contains([' ', '\t', '\r', '\n']) || ContextSize::parse(trimmed).is_err() {
        return Err(());
    }
    Ok(Some(trimmed))
}

pub struct GoalCommand;

impl SlashCommand for GoalCommand {
    fn name(&self) -> &str {
        "/goal"
    }

    fn summary(&self) -> &str {
        "Set or clear the session goal"
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            if arguments.is_empty() {
                context.session.clear_goal(cancellation).await
            } else {
                context.session.set_goal(arguments, cancellation).await
            }
        })
    }
}

pub struct StandingInstructionCommand;

impl SlashCommand for StandingInstructionCommand {
    fn name(&self) -> &str {
        "/standing-instruction"
    }

    fn summary(&self) -> &str {
        "Set or clear the instruction added to every agent's system prompt"
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        context
            .session
            .set_standing_instruction(arguments, cancellation)
    }
}

pub struct SandboxEnableCommand;

impl SlashCommand for SandboxEnableCommand {
    fn name(&self) -> &str {
        "/sandbox_enable"
    }

    fn summary(&self) -> &str {
        "Globally enable or disable the OS process sandbox"
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let value = arguments
                .trim_matches(|character: char| character.is_whitespace() || character == '\0');
            let enabled = if value.eq_ignore_ascii_case("true") {
                true
            } else if value.eq_ignore_ascii_case("false") {
                false
            } else {
                context
                    .dialog
                    .show_error("usage: /sandbox_enable <true|false>", cancellation)
                    .await?;
                return Ok(());
            };
            let result: Result<(), SlashFailure> = async {
                let response = context
                    .session
                    .sandbox_enable(enabled, cancellation)
                    .await?;
                context
                    .dialog
                    .show(
                        &[if response.enabled {
                            "sandbox enabled"
                        } else {
                            "sandbox disabled"
                        }
                        .to_owned()],
                        cancellation,
                    )
                    .await?;
                Ok(())
            }
            .await;
            if let Err(failure) = result {
                context
                    .dialog
                    .show_error(
                        &format!("failed to set sandbox: {}", exception_message(&failure)),
                        cancellation,
                    )
                    .await?;
            }
            Ok(())
        })
    }
}

// Catch(Exception) uses RpcException.Message, unlike the skills RPC catch,
// which uses Status.Detail. This projects remote status without claiming that
// native transport diagnostics reproduce a .NET DebugException.
fn exception_message(failure: &SlashFailure) -> String {
    match failure {
        SlashFailure::Rpc(status) => format!(
            "Status(StatusCode=\"{:?}\", Detail=\"{}\")",
            status.code(),
            status.message()
        ),
        SlashFailure::Command(message) => message.clone(),
    }
}

pub struct SkillsCommand;

impl SlashCommand for SkillsCommand {
    fn name(&self) -> &str {
        "/skills"
    }

    fn summary(&self) -> &str {
        "List, enable, or disable skills"
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            if !arguments.is_empty() {
                context
                    .dialog
                    .show_error("usage: /skills", cancellation)
                    .await?;
                return Ok(());
            }
            match run_skills(context, cancellation).await {
                Err(SlashFailure::Rpc(failure)) => {
                    context
                        .dialog
                        .show_error(
                            &format!("skills unavailable: {}", failure.message()),
                            cancellation,
                        )
                        .await?;
                    Ok(())
                }
                result => result,
            }
        })
    }
}

async fn run_skills(
    context: &mut CommandContext<'_>,
    cancellation: &dyn Cancellation,
) -> Result<(), SlashFailure> {
    let action = context
        .dialog
        .select(
            "Skills",
            &[
                DialogOption::new(
                    "list",
                    "List skills",
                    "Show skills available to this session",
                ),
                DialogOption::new(
                    "manage",
                    "Enable or disable skills",
                    "Change an exact skill path",
                ),
            ],
            cancellation,
        )
        .await?;
    let Some(action) = action else {
        return Ok(());
    };
    if action.id == "list" {
        let listed = load_command(
            context.dialog,
            "Loading skills…",
            context.session.list_skills(cancellation),
            cancellation,
        )
        .await?;
        return show_inventory(context, &listed, cancellation).await;
    }
    loop {
        let listed = load_command(
            context.dialog,
            "Loading skills…",
            context.session.list_skills(cancellation),
            cancellation,
        )
        .await?;
        if listed.skills.is_empty() {
            return show_inventory(context, &listed, cancellation).await;
        }
        if !listed.errors.is_empty() {
            context
                .dialog
                .show(&error_lines(&listed), cancellation)
                .await?;
        }
        let options: Vec<_> = listed
            .skills
            .iter()
            .map(|skill| {
                DialogOption::new(
                    &skill.path,
                    format!(
                        "[{}] {}",
                        if skill.enabled { 'x' } else { ' ' },
                        display_name(skill)
                    ),
                    format!("{} — {} — {}", description(skill), scope(skill), skill.path),
                )
            })
            .collect();
        let selected = context
            .dialog
            .select("Enable or disable skills", &options, cancellation)
            .await?;
        let Some(selected) = selected else {
            return Ok(());
        };
        let mut matching = listed
            .skills
            .iter()
            .filter(|skill| skill.path == selected.id);
        let skill = matching.next().ok_or_else(|| {
            SlashFailure::Command("Sequence contains no matching element".to_owned())
        })?;
        if matching.next().is_some() {
            return Err(SlashFailure::Command(
                "Sequence contains more than one matching element".to_owned(),
            ));
        }
        context.activity.wait_until_idle(cancellation).await?;
        let configured = context
            .session
            .configure_skill(&skill.path, !skill.enabled, cancellation)
            .await?;
        (context.refresh_skills)(cancellation).await?;
        context
            .dialog
            .show(
                &[format!(
                    "Skill {}: {} — {}",
                    if configured.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    display_name(&configured),
                    configured.path
                )],
                cancellation,
            )
            .await?;
    }
}

async fn show_inventory(
    context: &mut CommandContext<'_>,
    listed: &ListSkillsResponse,
    cancellation: &dyn Cancellation,
) -> Result<(), SlashFailure> {
    let mut lines: Vec<_> = listed
        .skills
        .iter()
        .map(|skill| {
            format!(
                "[{}] {} — {} — {} — {}",
                if skill.enabled { 'x' } else { ' ' },
                display_name(skill),
                description(skill),
                scope(skill),
                skill.path
            )
        })
        .collect();
    if lines.is_empty() {
        lines.push("No skills available.".to_owned());
    }
    lines.extend(error_lines(listed));
    context.dialog.show(&lines, cancellation).await?;
    Ok(())
}

fn error_lines(listed: &ListSkillsResponse) -> Vec<String> {
    listed
        .errors
        .iter()
        .map(|error| {
            format!(
                "Error: {} — {}",
                if error.path.is_empty() {
                    "<unknown>"
                } else {
                    &error.path
                },
                error.message
            )
        })
        .collect()
}

fn display_name(skill: &Skill) -> String {
    match skill
        .display_name
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        Some(name) if name != skill.name => format!("{name} ({})", skill.name),
        _ => skill.name.clone(),
    }
}

fn description(skill: &Skill) -> &str {
    skill
        .short_description
        .as_deref()
        .filter(|description| !description.is_empty())
        .unwrap_or(&skill.description)
}

fn scope(skill: &Skill) -> &str {
    match skill.scope.enum_value() {
        Ok(ListedSkillScope::SKILL_SCOPE_REPO) => "repo",
        Ok(ListedSkillScope::SKILL_SCOPE_USER) => "user",
        Ok(ListedSkillScope::SKILL_SCOPE_SYSTEM) => "system",
        _ => "unknown",
    }
}
