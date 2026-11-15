use super::*;

pub(super) struct RuntimeEventPublisher<'a> {
    pub(super) events: ContextEmitter<'a>,
}

impl parrot_core::EventPublisher for RuntimeEventPublisher<'_> {
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
    fn report_turn_started<'a>(
        &'a self,
        model: &'a parrot_core::TurnModel,
        cancellation: parrot_core::CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<(), CoreError>> {
        adapt_operation(async move {
            let mut event = self.events.event();
            let mut payload = protocol::TurnStarted {
                model: model.selector.clone(),
                ..Default::default()
            };
            if let Some(icon) = &model.alias_icon {
                payload.model_alias_icon =
                    protobuf::MessageField::some(protocol::TurnModelAliasIcon {
                        glyph: icon.glyph.clone(),
                        color: protobuf::EnumOrUnknown::new(presented_color(icon.color)),
                        ..Default::default()
                    });
            }
            event.set_turn_started(payload);
            self.events.emit(event, &cancellation).await
        })
    }
}

fn presented_color(color: parrot_core::TurnModelIconColor) -> protocol::TurnModelAliasIconColor {
    use parrot_core::TurnModelIconColor;
    use protocol::TurnModelAliasIconColor;
    match color {
        TurnModelIconColor::Black => TurnModelAliasIconColor::TURN_MODEL_ALIAS_ICON_COLOR_BLACK,
        TurnModelIconColor::Red => TurnModelAliasIconColor::TURN_MODEL_ALIAS_ICON_COLOR_RED,
        TurnModelIconColor::Green => TurnModelAliasIconColor::TURN_MODEL_ALIAS_ICON_COLOR_GREEN,
        TurnModelIconColor::Yellow => TurnModelAliasIconColor::TURN_MODEL_ALIAS_ICON_COLOR_YELLOW,
        TurnModelIconColor::Blue => TurnModelAliasIconColor::TURN_MODEL_ALIAS_ICON_COLOR_BLUE,
        TurnModelIconColor::Magenta => TurnModelAliasIconColor::TURN_MODEL_ALIAS_ICON_COLOR_MAGENTA,
        TurnModelIconColor::Cyan => TurnModelAliasIconColor::TURN_MODEL_ALIAS_ICON_COLOR_CYAN,
        TurnModelIconColor::White => TurnModelAliasIconColor::TURN_MODEL_ALIAS_ICON_COLOR_WHITE,
        TurnModelIconColor::Gray => TurnModelAliasIconColor::TURN_MODEL_ALIAS_ICON_COLOR_GRAY,
    }
}
