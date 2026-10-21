//! Immutable alias and routing snapshots over the actual provider registry.
use super::registry::{ProviderModel, ProviderRegistry};
use crate::sessions::Error;
use parrot_domain::values::context::ContextSize;
use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, RwLock},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelSelector {
    pub value: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelSelectorError;
impl fmt::Display for ModelSelectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("A model selector is required. (Parameter 'value')")
    }
}
impl std::error::Error for ModelSelectorError {}
impl ModelSelector {
    pub fn new(value: impl Into<String>) -> Result<Self, ModelSelectorError> {
        let value = value.into();
        if value.is_empty() {
            Err(ModelSelectorError)
        } else {
            Ok(Self { value })
        }
    }
}
impl fmt::Display for ModelSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelAliasIconColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    Gray,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelAliasIcon {
    pub glyph: String,
    pub color: ModelAliasIconColor,
}
impl ModelAliasIcon {
    pub fn parse(glyph: impl Into<String>, color: &str) -> Result<Self, ModelAliasIconError> {
        let color = match color {
            "black" => ModelAliasIconColor::Black,
            "red" => ModelAliasIconColor::Red,
            "green" => ModelAliasIconColor::Green,
            "yellow" => ModelAliasIconColor::Yellow,
            "blue" => ModelAliasIconColor::Blue,
            "magenta" => ModelAliasIconColor::Magenta,
            "cyan" => ModelAliasIconColor::Cyan,
            "white" => ModelAliasIconColor::White,
            "gray" => ModelAliasIconColor::Gray,
            _ => return Err(ModelAliasIconError(color.to_owned())),
        };
        Ok(Self {
            glyph: glyph.into(),
            color,
        })
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelAliasIconError(pub String);
impl fmt::Display for ModelAliasIconError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unsupported model alias icon color {}", self.0)
    }
}
impl std::error::Error for ModelAliasIconError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelAliasDefinition {
    pub name: String,
    pub model_string: String,
    pub usage: String,
    pub augment_system_prompt: Option<String>,
    pub icon: Option<ModelAliasIcon>,
    pub context_limit: Option<ContextSize>,
}
impl ModelAliasDefinition {
    pub fn new(
        name: impl Into<String>,
        model_string: impl Into<String>,
        usage: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            model_string: model_string.into(),
            usage: usage.into(),
            augment_system_prompt: None,
            icon: None,
            context_limit: None,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelAliasSnapshot {
    definitions: Vec<ModelAliasDefinition>,
}
impl ModelAliasSnapshot {
    pub fn definitions(&self) -> &[ModelAliasDefinition] {
        &self.definitions
    }
    pub fn find(&self, name: &str) -> Option<&ModelAliasDefinition> {
        self.definitions
            .iter()
            .find(|definition| definition.name == name)
    }
}

pub struct ModelAliasCatalog {
    registry: Arc<ProviderRegistry>,
    initial_snapshot: Arc<ModelAliasSnapshot>,
}
impl ModelAliasCatalog {
    pub fn new(
        registry: Arc<ProviderRegistry>,
        definitions: impl IntoIterator<Item = ModelAliasDefinition>,
    ) -> Result<Self, Error> {
        let initial_snapshot = Self::validate(&registry, definitions)?;
        Ok(Self {
            registry,
            initial_snapshot,
        })
    }
    pub fn initial_snapshot(&self) -> Arc<ModelAliasSnapshot> {
        self.initial_snapshot.clone()
    }
    pub fn prepare_replacement(
        &self,
        definitions: impl IntoIterator<Item = ModelAliasDefinition>,
    ) -> Result<Arc<ModelAliasSnapshot>, Error> {
        Self::validate(&self.registry, definitions)
    }
    fn validate(
        registry: &ProviderRegistry,
        definitions: impl IntoIterator<Item = ModelAliasDefinition>,
    ) -> Result<Arc<ModelAliasSnapshot>, Error> {
        let definitions: Vec<_> = definitions.into_iter().collect();
        let mut names = BTreeMap::new();
        for definition in &definitions {
            let name = &definition.name;
            if name.is_empty() {
                return Err(Error::Provider(
                    "model alias: alias name is required".into(),
                ));
            }
            if name.trim() != name {
                return Err(Error::Provider(format!(
                    "model alias: alias name \"{name}\" has surrounding whitespace"
                )));
            }
            if name.contains('/') {
                return Err(Error::Provider(format!(
                    "model alias: alias name \"{name}\" contains '/'"
                )));
            }
            if names.insert(name.as_str(), ()).is_some() {
                return Err(Error::Provider(format!(
                    "model alias: duplicate alias \"{name}\""
                )));
            }
            if definition.usage.is_empty() {
                return Err(Error::Provider(format!(
                    "model alias: usage is required for \"{name}\""
                )));
            }
        }
        for definition in definitions
            .iter()
            .filter(|definition| !definition.model_string.is_empty())
        {
            if names.contains_key(definition.model_string.as_str()) {
                return Err(Error::Provider(format!(
                    "model alias: alias \"{}\" targets alias \"{}\"",
                    definition.name, definition.model_string
                )));
            }
            if let Err(error) = registry.resolve_canonical(&definition.model_string) {
                return Err(Error::Provider(format!(
                    "model alias: invalid target for \"{}\": {error}",
                    definition.name
                )));
            }
        }
        let mut definitions = definitions;
        definitions.sort_by(|left, right| left.name.encode_utf16().cmp(right.name.encode_utf16()));
        Ok(Arc::new(ModelAliasSnapshot { definitions }))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRoutingSnapshot {
    pub configured_default_selector: String,
    pub aliases: Arc<ModelAliasSnapshot>,
    pub revision: i64,
    pub context_limit: Option<ContextSize>,
}
pub struct ModelRouting {
    catalog: Arc<ModelAliasCatalog>,
    snapshot: RwLock<Arc<ModelRoutingSnapshot>>,
}
impl ModelRouting {
    pub fn new(
        catalog: Arc<ModelAliasCatalog>,
        configured_default_selector: impl Into<String>,
    ) -> Self {
        let snapshot = Arc::new(ModelRoutingSnapshot {
            configured_default_selector: configured_default_selector.into(),
            aliases: catalog.initial_snapshot(),
            revision: 0,
            context_limit: None,
        });
        Self {
            catalog,
            snapshot: RwLock::new(snapshot),
        }
    }
    pub fn capture(&self) -> Arc<ModelRoutingSnapshot> {
        self.snapshot.read().expect("routing snapshot lock").clone()
    }
    pub fn prepare(
        &self,
        configured_default: impl Into<String>,
        aliases: impl IntoIterator<Item = ModelAliasDefinition>,
    ) -> Result<Arc<ModelRoutingSnapshot>, Error> {
        self.prepare_with_revision(
            configured_default,
            aliases,
            self.capture().revision.wrapping_add(1),
        )
    }
    pub fn prepare_with_revision(
        &self,
        configured_default: impl Into<String>,
        aliases: impl IntoIterator<Item = ModelAliasDefinition>,
        revision: i64,
    ) -> Result<Arc<ModelRoutingSnapshot>, Error> {
        Ok(Arc::new(ModelRoutingSnapshot {
            configured_default_selector: configured_default.into(),
            aliases: self.catalog.prepare_replacement(aliases)?,
            revision,
            context_limit: None,
        }))
    }
    pub fn publish(&self, snapshot: Arc<ModelRoutingSnapshot>) {
        *self.snapshot.write().expect("routing snapshot lock") = snapshot;
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedModelSelection {
    pub requested_selector: ModelSelector,
    pub alias: Option<ModelAliasDefinition>,
    pub canonical_model: ProviderModel,
    pub routing_snapshot: Arc<ModelRoutingSnapshot>,
}
impl ResolvedModelSelection {
    pub fn context_limit(&self) -> Option<&ContextSize> {
        self.alias
            .as_ref()
            .and_then(|alias| alias.context_limit.as_ref())
            .or(self.routing_snapshot.context_limit.as_ref())
    }
    pub fn canonical_base(&self) -> String {
        format!(
            "{}/{}",
            self.canonical_model.provider.id(),
            self.canonical_model.model.id
        )
    }
    pub fn alias_snapshot(&self) -> &Arc<ModelAliasSnapshot> {
        &self.routing_snapshot.aliases
    }
}
pub struct ModelRouter {
    registry: Arc<ProviderRegistry>,
    routing: Arc<ModelRouting>,
}
impl ModelRouter {
    pub fn new(registry: Arc<ProviderRegistry>, routing: Arc<ModelRouting>) -> Self {
        Self { registry, routing }
    }
    pub fn routing_revision(&self) -> i64 {
        self.routing.capture().revision
    }
    pub fn resolve(&self, selector: &str) -> Result<ResolvedModelSelection, Error> {
        self.resolve_from(self.routing.capture(), selector)
    }
    pub fn resolve_from(
        &self,
        snapshot: Arc<ModelRoutingSnapshot>,
        selector: &str,
    ) -> Result<ResolvedModelSelection, Error> {
        let requested = if selector.is_empty() {
            &snapshot.configured_default_selector
        } else {
            selector
        };
        if requested.is_empty() {
            let canonical_model = self.registry.resolve_default_canonical()?;
            return Ok(ResolvedModelSelection {
                requested_selector: ModelSelector {
                    value: canonical_model.selector(),
                },
                alias: None,
                canonical_model,
                routing_snapshot: snapshot,
            });
        }
        let requested_selector = ModelSelector {
            value: requested.to_owned(),
        };
        let alias = snapshot.aliases.find(requested).cloned();
        let target = match &alias {
            Some(alias) if alias.model_string.is_empty() => {
                return Err(Error::Provider(format!(
                    "model alias: alias \"{requested}\" is not configured"
                )));
            }
            Some(alias) => alias.model_string.as_str(),
            None => requested,
        };
        let canonical_model = self.registry.resolve_canonical(target)?;
        Ok(ResolvedModelSelection {
            requested_selector,
            alias,
            canonical_model,
            routing_snapshot: snapshot,
        })
    }
}
