use super::*;
use parrot_core::{TurnHooks, TurnModel, TurnModelIcon, TurnModelIconColor};
use parrot_provider::routing::aliases::{ModelAliasIconColor, ResolvedModelSelection};

pub(super) struct RuntimeTurnHooks<'a, S: AccessSnapshot + 'static> {
    pub(super) backend: &'a RuntimeBackend<S>,
    pub(super) binding: &'a TurnBinding<S>,
    pub(super) cancellation: &'a dyn Cancellation,
}

impl<S: AccessSnapshot + 'static> TurnHooks for RuntimeTurnHooks<'_, S> {
    fn begin_conversation_turn(&self) -> BoxFuture<'_, Result<TurnModel, CoreError>> {
        adapt_operation(async move {
            let selection = self.backend.selection.capture()?;
            self.backend.sessions.begin_turn().await?;
            let model = presented_model(&selection.model);
            self.binding.bind(selection);
            Ok(model)
        })
    }

    fn activate_conversation_turn(&self, turn_id: &str) -> BoxFuture<'_, Result<(), CoreError>> {
        let turn_id = turn_id.to_owned();
        adapt_operation(async move {
            self.backend.activity.turn_started(&turn_id);
            self.backend
                .state
                .lock()
                .await
                .context
                .ensure_epoch(&self.backend.prompt, self.cancellation)
                .await?;
            self.backend.skills.begin_turn(self.cancellation).await?;
            Ok(())
        })
    }

    fn begin_step(&self) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            self.binding.bind(self.backend.selection.capture()?);
            Ok(())
        })
    }
}

fn presented_model(selection: &ResolvedModelSelection) -> TurnModel {
    TurnModel {
        selector: selection.canonical_model.selector(),
        alias_icon: selection
            .alias
            .as_ref()
            .and_then(|alias| alias.icon.as_ref())
            .map(|icon| TurnModelIcon {
                glyph: icon.glyph.clone(),
                color: presented_icon_color(icon.color),
            }),
    }
}

fn presented_icon_color(color: ModelAliasIconColor) -> TurnModelIconColor {
    match color {
        ModelAliasIconColor::Black => TurnModelIconColor::Black,
        ModelAliasIconColor::Red => TurnModelIconColor::Red,
        ModelAliasIconColor::Green => TurnModelIconColor::Green,
        ModelAliasIconColor::Yellow => TurnModelIconColor::Yellow,
        ModelAliasIconColor::Blue => TurnModelIconColor::Blue,
        ModelAliasIconColor::Magenta => TurnModelIconColor::Magenta,
        ModelAliasIconColor::Cyan => TurnModelIconColor::Cyan,
        ModelAliasIconColor::White => TurnModelIconColor::White,
        ModelAliasIconColor::Gray => TurnModelIconColor::Gray,
    }
}
