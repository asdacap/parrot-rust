use parrot_domain::contracts::async_ops::Cancellation;

use super::{CommandContext, CommandDescription, SlashCommand, SlashFailure};

/// The source registry preserves construction order for command discovery and
/// uses ordinal matching for dispatch and completion.
pub struct SlashCommandRegistry {
    commands: Vec<Box<dyn SlashCommand>>,
}

impl SlashCommandRegistry {
    pub fn new(commands: Vec<Box<dyn SlashCommand>>) -> Self {
        Self { commands }
    }

    pub fn commands(&self) -> &[Box<dyn SlashCommand>] {
        &self.commands
    }

    pub fn descriptions(&self) -> Vec<CommandDescription> {
        self.commands
            .iter()
            .map(|command| CommandDescription {
                name: command.name().to_owned(),
                summary: command.summary().to_owned(),
            })
            .collect()
    }

    pub fn find(&self, name: &str) -> Option<&dyn SlashCommand> {
        self.commands
            .iter()
            .find(|command| command.name() == name)
            .map(|command| command.as_ref())
    }

    pub(crate) fn complete_indices(&self, entered: &str) -> Vec<usize> {
        let name = name(entered);
        if !name.starts_with('/') {
            return Vec::new();
        }
        let mut indices: Vec<_> = self
            .commands
            .iter()
            .enumerate()
            .filter_map(|(index, command)| command.name().starts_with(name).then_some(index))
            .collect();
        indices.sort_by(|left, right| {
            self.commands[*left]
                .name()
                .cmp(self.commands[*right].name())
        });
        indices
    }

    pub fn complete(&self, entered: &str) -> Vec<CommandDescription> {
        self.complete_indices(entered)
            .into_iter()
            .map(|index| {
                let command = &self.commands[index];
                CommandDescription {
                    name: command.name().to_owned(),
                    summary: command.summary().to_owned(),
                }
            })
            .collect()
    }

    pub async fn dispatch(
        &self,
        entered: &str,
        context: &mut CommandContext<'_>,
        cancellation: &dyn Cancellation,
    ) -> Result<(), SlashFailure> {
        let command_name = name(entered);
        let Some(command) = self.find(command_name) else {
            context
                .dialog
                .show_error(
                    &format!("unknown command {command_name}, try /help"),
                    cancellation,
                )
                .await?;
            return Ok(());
        };
        let arguments = entered[command_name.len()..].trim_start_matches([' ', '\t', '\r', '\n']);
        let descriptions = self.descriptions();
        let mut actual = CommandContext {
            session: context.session,
            activity: context.activity,
            dialog: context.dialog,
            refresh_skills: context.refresh_skills,
            commands: &descriptions,
        };
        command.run(arguments, &mut actual, cancellation).await
    }
}

fn name(entered: &str) -> &str {
    entered.split([' ', '\t', '\r', '\n']).next().unwrap_or("")
}
