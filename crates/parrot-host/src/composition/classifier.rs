//! Runtime-updatable command safety classifier selection.
//!
//! The slash commands mutate one shared setting per user session; the gate
//! re-resolves the decision model per classification and caches by model id.

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::contracts::safety::{
    CommandClassifierResolver, CommandSafetyClassifier, SafetyClassifierError, SafetyVerdict,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// The user session's safety classifier toggle and selected decision model.
/// Persisted state overrides the predefined configuration defaults.
pub struct SafetyClassifierSetting {
    enabled: AtomicBool,
    model: Mutex<String>,
}

impl SafetyClassifierSetting {
    pub fn new(enabled: bool, model: impl Into<String>) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            model: Mutex::new(model.into()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    pub fn model(&self) -> String {
        self.model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set_model(&self, model: impl Into<String>) {
        *self
            .model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = model.into();
    }
}

type ResolvedClassifierCache = Option<(String, Option<Arc<dyn CommandSafetyClassifier>>)>;

/// Classifies commands through the resolver-selected model, following the
/// live setting. A disabled classifier passes commands through unchecked,
/// which is exactly the absent-gate behavior the toggle promises. A selected
/// model without a configured decisions provider also passes through; a
/// resolved classifier's own failures stay errors so the caller fails safe.
pub struct DynamicCommandSafetyClassifier {
    setting: Arc<SafetyClassifierSetting>,
    resolve: Arc<dyn CommandClassifierResolver>,
    cached: Mutex<ResolvedClassifierCache>,
}

impl DynamicCommandSafetyClassifier {
    pub fn new(
        setting: Arc<SafetyClassifierSetting>,
        resolve: Arc<dyn CommandClassifierResolver>,
    ) -> Self {
        Self {
            setting,
            resolve,
            cached: Mutex::new(None),
        }
    }

    fn classifier_for(
        &self,
        model: &str,
    ) -> Result<Option<Arc<dyn CommandSafetyClassifier>>, SafetyClassifierError> {
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cached
            .as_ref()
            .is_some_and(|(cached_model, _)| cached_model == model)
        {
            return Ok(cached
                .as_ref()
                .and_then(|(_, classifier)| classifier.clone()));
        }
        let resolved = self
            .resolve
            .resolve(model)
            .map_err(SafetyClassifierError::Transport)?;
        *cached = Some((model.to_owned(), resolved.clone()));
        Ok(resolved)
    }
}

impl CommandSafetyClassifier for DynamicCommandSafetyClassifier {
    fn classify<'a>(
        &'a self,
        command: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SafetyVerdict, SafetyClassifierError>> {
        Box::pin(async move {
            if !self.setting.is_enabled() {
                return Ok(SafetyVerdict::Safe);
            }
            let model = self.setting.model();
            let Some(classifier) = self.classifier_for(&model)? else {
                return Ok(SafetyVerdict::Safe);
            };
            classifier.classify(command, cancellation).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct CountingResolver {
        calls: AtomicUsize,
        resolve_to: Option<Arc<dyn CommandSafetyClassifier>>,
    }
    impl CommandClassifierResolver for CountingResolver {
        fn resolve(&self, _: &str) -> Result<Option<Arc<dyn CommandSafetyClassifier>>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.resolve_to.clone())
        }
    }

    struct FixedClassifier(SafetyVerdict);
    impl CommandSafetyClassifier for FixedClassifier {
        fn classify<'a>(
            &'a self,
            _: &'a str,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<SafetyVerdict, SafetyClassifierError>> {
            Box::pin(async { Ok(self.0.clone()) })
        }
    }

    #[tokio::test]
    async fn disabled_classifier_passes_through_without_resolving() {
        let setting = Arc::new(SafetyClassifierSetting::new(false, "openrouter/m"));
        let resolver = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
            resolve_to: None,
        });
        let classifier = DynamicCommandSafetyClassifier::new(setting, resolver.clone());
        assert_eq!(
            classifier
                .classify("ls", &parrot_core::NeverCancelled)
                .await
                .unwrap(),
            SafetyVerdict::Safe
        );
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unknown_provider_passes_through_and_the_verdict_is_cached() {
        let setting = Arc::new(SafetyClassifierSetting::new(true, "openrouter/m"));
        let resolver = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
            resolve_to: None,
        });
        let classifier = DynamicCommandSafetyClassifier::new(setting, resolver.clone());
        for _ in 0..2 {
            assert_eq!(
                classifier
                    .classify("ls", &parrot_core::NeverCancelled)
                    .await
                    .unwrap(),
                SafetyVerdict::Safe
            );
        }
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_resolved_classifier_supplies_the_verdict() {
        let setting = Arc::new(SafetyClassifierSetting::new(true, "openrouter/m"));
        let resolver = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
            resolve_to: Some(Arc::new(FixedClassifier(SafetyVerdict::Unsafe {
                reason: "no".into(),
            }))),
        });
        let classifier = DynamicCommandSafetyClassifier::new(setting, resolver);
        assert_eq!(
            classifier
                .classify("ls", &parrot_core::NeverCancelled)
                .await
                .unwrap(),
            SafetyVerdict::Unsafe {
                reason: "no".into()
            }
        );
    }

    #[tokio::test]
    async fn model_changes_invalidate_the_cache() {
        let setting = Arc::new(SafetyClassifierSetting::new(true, "openrouter/m"));
        let resolver = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
            resolve_to: None,
        });
        let classifier = DynamicCommandSafetyClassifier::new(setting.clone(), resolver.clone());
        classifier
            .classify("ls", &parrot_core::NeverCancelled)
            .await
            .unwrap();
        setting.set_model("other/m");
        classifier
            .classify("ls", &parrot_core::NeverCancelled)
            .await
            .unwrap();
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    }
}
