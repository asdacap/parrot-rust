use parrot_domain::protocol::messages::parrot::{Model, ModelVariant};

/// A concrete model and optional metadata-defined effort, not a parsed suffix.
#[derive(Clone, Debug)]
pub struct ModelSelection {
    pub model: Model,
    pub variant: Option<ModelVariant>,
}

impl ModelSelection {
    pub fn new(model: Model, variant: Option<ModelVariant>) -> Self {
        Self { model, variant }
    }

    pub fn base_selector(&self) -> String {
        format!("{}/{}", self.model.provider_id, self.model.id)
    }

    pub fn selector(&self) -> String {
        match &self.variant {
            Some(variant) => format!("{}/{}", self.base_selector(), variant.name),
            None => self.base_selector(),
        }
    }

    pub fn resolve(models: &[Model], selector: &str) -> Option<Self> {
        let (provider, remainder) = selector.split_once('/')?;
        if provider.is_empty() || selector.split('/').any(str::is_empty) {
            return None;
        }
        let provider_models: Vec<_> = models
            .iter()
            .filter(|model| model.provider_id == provider)
            .collect();
        let exact: Vec<_> = provider_models
            .iter()
            .filter(|model| model.id == remainder)
            .collect();
        match exact.as_slice() {
            [model] => return Some(Self::new((***model).clone(), None)),
            [] => {}
            _ => return None,
        }
        let candidates: Vec<_> = provider_models
            .into_iter()
            .flat_map(|model| {
                model
                    .variants
                    .iter()
                    .filter(move |variant| remainder == format!("{}/{}", model.id, variant.name))
                    .map(move |variant| Self::new(model.clone(), Some(variant.clone())))
            })
            .collect();
        match candidates.as_slice() {
            [candidate] => Some(candidate.clone()),
            _ => None,
        }
    }

    pub fn with_variant(&self, name: &str) -> Option<Self> {
        let matches: Vec<_> = self
            .model
            .variants
            .iter()
            .filter(|variant| variant.name == name)
            .collect();
        match matches.as_slice() {
            [variant] => Some(Self::new(self.model.clone(), Some((*variant).clone()))),
            _ => None,
        }
    }

    pub fn with_first_variant(&self) -> Self {
        match self.model.variants.first() {
            Some(variant) => Self::new(self.model.clone(), Some(variant.clone())),
            None => self.clone(),
        }
    }
}
