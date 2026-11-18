use super::*;
use crate::agent_turn::services::SelectionSource;
use parrot_provider::routing::aliases::ModelAliasIconColor;

pub(super) struct RuntimeEventPublisher<'a, S: AccessSnapshot + 'static> {
    pub(super) events: ContextEmitter<'a>,
    pub(super) selection: Arc<dyn SelectionSource<S>>,
}

impl<S: AccessSnapshot + 'static> RuntimeEventPublisher<'_, S> {
    /// Publishes the turn start event carrying the selection captured at publication.
    pub(super) fn report_turn_started<'a>(
        &'a self,
        cancellation: parrot_core::CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<(), CoreError>> {
        adapt_operation(async move {
            let selection = self.selection.capture()?;
            let mut payload = protocol::TurnStarted {
                model: selection.model.canonical_model.selector(),
                ..Default::default()
            };
            if let Some(icon) = selection
                .model
                .alias
                .as_ref()
                .and_then(|alias| alias.icon.as_ref())
            {
                payload.model_alias_icon =
                    protobuf::MessageField::some(protocol::TurnModelAliasIcon {
                        glyph: icon.glyph.clone(),
                        color: protobuf::EnumOrUnknown::new(presented_icon_color(icon.color)),
                        ..Default::default()
                    });
            }
            let mut event = self.events.event();
            event.set_turn_started(payload);
            self.events.emit(event, &cancellation).await
        })
    }
}

fn presented_icon_color(color: ModelAliasIconColor) -> protocol::TurnModelAliasIconColor {
    use protocol::TurnModelAliasIconColor::*;
    match color {
        ModelAliasIconColor::Black => TURN_MODEL_ALIAS_ICON_COLOR_BLACK,
        ModelAliasIconColor::Red => TURN_MODEL_ALIAS_ICON_COLOR_RED,
        ModelAliasIconColor::Green => TURN_MODEL_ALIAS_ICON_COLOR_GREEN,
        ModelAliasIconColor::Yellow => TURN_MODEL_ALIAS_ICON_COLOR_YELLOW,
        ModelAliasIconColor::Blue => TURN_MODEL_ALIAS_ICON_COLOR_BLUE,
        ModelAliasIconColor::Magenta => TURN_MODEL_ALIAS_ICON_COLOR_MAGENTA,
        ModelAliasIconColor::Cyan => TURN_MODEL_ALIAS_ICON_COLOR_CYAN,
        ModelAliasIconColor::White => TURN_MODEL_ALIAS_ICON_COLOR_WHITE,
        ModelAliasIconColor::Gray => TURN_MODEL_ALIAS_ICON_COLOR_GRAY,
    }
}

impl<S: AccessSnapshot + 'static> parrot_core::EventPublisher for RuntimeEventPublisher<'_, S> {
    fn publish<'a>(
        &'a self,
        event: parrot_core::ConversationEvent,
        cancellation: parrot_core::CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<(), CoreError>> {
        adapt_operation(async move {
            self.events
                .emit(
                    conversation::render_event(self.events.event(), event),
                    &cancellation,
                )
                .await
        })
    }
}
