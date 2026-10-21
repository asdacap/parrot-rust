//! Ordinal tool selection with redaction applied before specialized or fallback code.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::{
    Presentation, PresenterError, PresenterResult, ToolCallPresentation, ToolPresentationMetadata,
    ToolPresenter, ToolTerminalPresentation, redact_call, redact_terminal,
};

pub struct ToolPresenterRegistry {
    presenters: BTreeMap<String, Arc<dyn ToolPresenter>>,
    fallback: Arc<dyn ToolPresenter>,
}

impl ToolPresenterRegistry {
    pub fn new(
        presenters: Vec<Arc<dyn ToolPresenter>>,
        fallback: Arc<dyn ToolPresenter>,
    ) -> PresenterResult<Self> {
        let mut registered = BTreeMap::new();
        for presenter in presenters {
            let name = presenter.tool_name().to_owned();
            if name.is_empty() {
                return Err(PresenterError(
                    "A tool presenter name cannot be empty.".into(),
                ));
            }
            if registered.insert(name.clone(), presenter).is_some() {
                return Err(PresenterError(format!(
                    "A presenter for '{name}' is already registered."
                )));
            }
        }
        Ok(Self {
            presenters: registered,
            fallback,
        })
    }

    pub fn registered_names(&self) -> impl Iterator<Item = &str> {
        self.presenters.keys().map(String::as_str)
    }

    pub fn describe(&self, tool_name: &str) -> ToolPresentationMetadata {
        self.find(tool_name).metadata()
    }

    pub fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        let presenter = self.find(&call.tool_name);
        let call = redact_call(call, &presenter.metadata());
        match presenter.present_live(&call, frame) {
            Err(_) if !Arc::ptr_eq(presenter, &self.fallback) => {
                self.fallback.present_live(&call, frame)
            }
            result => result,
        }
    }

    pub fn present_started(
        &self,
        call: &ToolCallPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let presenter = self.find(&call.tool_name);
        let call = redact_call(call, &presenter.metadata());
        match presenter.present_started(&call) {
            Err(_) if !Arc::ptr_eq(presenter, &self.fallback) => {
                self.fallback.present_started(&call)
            }
            result => result,
        }
    }

    pub fn present_child_started(
        &self,
        call: &ToolCallPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let presenter = self.find(&call.tool_name);
        let call = redact_call(call, &presenter.metadata());
        match presenter.present_child_started(&call) {
            Err(_) if !Arc::ptr_eq(presenter, &self.fallback) => {
                self.fallback.present_child_started(&call)
            }
            result => result,
        }
    }

    pub fn present_terminal(
        &self,
        call: &ToolCallPresentation,
        terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let presenter = self.find(&call.tool_name);
        let metadata = presenter.metadata();
        let call = redact_call(call, &metadata);
        let terminal = redact_terminal(terminal, &metadata);
        match presenter.present_terminal(&call, &terminal) {
            Err(_) if !Arc::ptr_eq(presenter, &self.fallback) => {
                self.fallback.present_terminal(&call, &terminal)
            }
            result => result,
        }
    }

    fn find(&self, tool_name: &str) -> &Arc<dyn ToolPresenter> {
        self.presenters.get(tool_name).unwrap_or(&self.fallback)
    }
}
