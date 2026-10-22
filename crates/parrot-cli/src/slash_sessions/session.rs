use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
};
use parrot_foundation::config::Configuration;
use parrot_host::web::router::WebClient;

use crate::dispatch::arguments::ChatOptions;

use super::{
    ExistingSessionBinding, SlashActivity, SlashFailure, SlashSession, SlashSessionBinding,
    native_call,
};

pub type ReplaceSession = Arc<
    dyn for<'a> Fn(
            wire::UserSession,
            &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<(), SlashFailure>>
        + Send
        + Sync,
>;

pub struct CallbackSessionBinding(pub ReplaceSession);
impl SlashSessionBinding for CallbackSessionBinding {
    fn replace<'a>(
        &'a mut self,
        session: wire::UserSession,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        (self.0)(session, cancellation)
    }
}

/// The actual mutable source SlashSession. Every operation uses the selected
/// native client; it never substitutes a new session or local storage algorithm.
pub struct RpcSlashSession {
    client: WebClient,
    current: wire::UserSession,
    configuration: Arc<Mutex<Configuration>>,
    interactive_permissions: bool,
    binding: Box<dyn SlashSessionBinding>,
}

impl RpcSlashSession {
    pub fn new(
        client: WebClient,
        initial: wire::UserSession,
        configuration: Arc<Mutex<Configuration>>,
        interactive_permissions: bool,
        binding: Box<dyn SlashSessionBinding>,
    ) -> Self {
        Self {
            client,
            current: initial,
            configuration,
            interactive_permissions,
            binding,
        }
    }
}

// Rust observer cancellation can drop a pending binding future. Preserve the
// source rollback guarantee on that path as well as on returned binding errors.
struct Rollback<'a> {
    current: &'a mut wire::UserSession,
    previous: Option<wire::UserSession>,
}
impl<'a> Rollback<'a> {
    fn new(current: &'a mut wire::UserSession) -> Self {
        let previous = Some(current.clone());
        Self { current, previous }
    }
    fn keep(mut self) {
        self.previous = None;
    }
}
impl Drop for Rollback<'_> {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            *self.current = previous;
        }
    }
}

fn required<T>(value: protobuf::MessageField<T>, field: &str) -> Result<T, SlashFailure> {
    value
        .into_option()
        .ok_or_else(|| SlashFailure::Command(format!("missing {field} in server response")))
}

impl SlashSession for RpcSlashSession {
    fn current_session(&self) -> &wire::UserSession {
        &self.current
    }

    fn select_model<'a>(
        &'a mut self,
        model: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            self.current = native_call(
                self.client.update_session(wire::UpdateSessionRequest {
                    user_session_id: self.current.id.clone(),
                    model: model.into(),
                    ..Default::default()
                }),
                c,
            )
            .await?;
            self.configuration
                .lock()
                .map_err(|_| SlashFailure::Command("configuration lock poisoned".into()))?
                .set_model(&self.current.model)
                .map_err(|failure| SlashFailure::Command(failure.to_string()))
        })
    }
    fn set_model_preset<'a>(
        &'a mut self,
        name: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::ModelPreset, SlashFailure>> {
        Box::pin(async move {
            required(
                native_call(
                    self.client.set_model_preset(wire::SetModelPresetRequest {
                        user_session_id: self.current.id.clone(),
                        name: name.into(),
                        ..Default::default()
                    }),
                    c,
                )
                .await?
                .preset,
                "preset",
            )
        })
    }
    fn select_model_preset<'a>(
        &'a mut self,
        name: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::ModelPreset, SlashFailure>> {
        Box::pin(async move {
            let response = native_call(
                self.client
                    .select_model_preset(wire::SelectModelPresetRequest {
                        user_session_id: self.current.id.clone(),
                        name: name.into(),
                        ..Default::default()
                    }),
                c,
            )
            .await?;
            self.current = required(response.session, "session")?;
            required(response.preset, "preset")
        })
    }
    fn list_model_presets<'a>(
        &'a mut self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<wire::ModelPreset>, SlashFailure>> {
        Box::pin(async move {
            Ok(native_call(
                self.client
                    .list_model_presets(wire::ListModelPresetsRequest::new()),
                c,
            )
            .await?
            .presets)
        })
    }
    fn select_mode<'a>(
        &'a mut self,
        mode: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            self.current = native_call(
                self.client.update_session(wire::UpdateSessionRequest {
                    user_session_id: self.current.id.clone(),
                    mode: mode.into(),
                    ..Default::default()
                }),
                c,
            )
            .await?;
            Ok(())
        })
    }
    fn start_new<'a>(
        &'a mut self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let mut options = ChatOptions::default();
            {
                let configuration = self.configuration.lock().expect("configuration poisoned");
                options.apply_configuration(
                    &configuration.model,
                    &configuration.runtime.default_profile,
                );
            }
            let replacement = native_call(
                self.client.create_session(wire::CreateSessionRequest {
                    model: options.model,
                    mode: options.mode,
                    interactive_permissions: self.interactive_permissions,
                    ..Default::default()
                }),
                c,
            )
            .await?;
            let rollback = Rollback::new(&mut self.current);
            *rollback.current = replacement.clone();
            self.binding.replace(replacement, c).await?;
            rollback.keep();
            Ok(())
        })
    }
    fn load_existing<'a>(
        &'a mut self,
        replacement: wire::UserSession,
        binding: &'a mut dyn ExistingSessionBinding,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let rollback = Rollback::new(&mut self.current);
            {
                let mut commit = || *rollback.current = replacement.clone();
                binding.bind(&mut commit, c).await?;
            }
            rollback.keep();
            Ok(())
        })
    }
    fn set_goal<'a>(
        &'a mut self,
        goal: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            native_call(
                self.client.set_goal(wire::SetGoalRequest {
                    user_session_id: self.current.id.clone(),
                    state: Some(wire::set_goal_request::State::Goal(goal.into())),
                    ..Default::default()
                }),
                c,
            )
            .await?;
            Ok(())
        })
    }
    fn clear_goal<'a>(
        &'a mut self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            native_call(
                self.client.set_goal(wire::SetGoalRequest {
                    user_session_id: self.current.id.clone(),
                    state: Some(wire::set_goal_request::State::Clear(wire::ClearGoal::new())),
                    ..Default::default()
                }),
                c,
            )
            .await?;
            Ok(())
        })
    }
    fn set_standing_instruction<'a>(
        &'a mut self,
        instruction: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            native_call(
                self.client
                    .set_standing_instruction(wire::SetStandingInstructionRequest {
                        user_session_id: self.current.id.clone(),
                        instruction: instruction.into(),
                        ..Default::default()
                    }),
                c,
            )
            .await?;
            Ok(())
        })
    }
    fn compact<'a>(
        &'a mut self,
        target: Option<&'a str>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            native_call(
                self.client.compact(wire::CompactRequest {
                    user_session_id: self.current.id.clone(),
                    target_context_size: target.unwrap_or_default().into(),
                    ..Default::default()
                }),
                c,
            )
            .await?;
            Ok(())
        })
    }
    fn set_context_limit<'a>(
        &'a mut self,
        limit: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::SetContextLimitResponse, SlashFailure>> {
        Box::pin(async move {
            native_call(
                self.client.set_context_limit(wire::SetContextLimitRequest {
                    user_session_id: self.current.id.clone(),
                    context_limit: limit.into(),
                    ..Default::default()
                }),
                c,
            )
            .await
        })
    }
    fn sandbox_enable<'a>(
        &'a mut self,
        enabled: bool,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::SandboxEnableResponse, SlashFailure>> {
        Box::pin(async move {
            native_call(
                self.client.sandbox_enable(wire::SandboxEnableRequest {
                    user_session_id: self.current.id.clone(),
                    enabled,
                    ..Default::default()
                }),
                c,
            )
            .await
        })
    }
    fn list_skills<'a>(
        &'a mut self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::ListSkillsResponse, SlashFailure>> {
        Box::pin(async move {
            native_call(
                self.client.list_skills(wire::ListSkillsRequest {
                    user_session_id: self.current.id.clone(),
                    ..Default::default()
                }),
                c,
            )
            .await
        })
    }
    fn configure_skill<'a>(
        &'a mut self,
        path: &'a str,
        enabled: bool,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::Skill, SlashFailure>> {
        Box::pin(async move {
            required(
                native_call(
                    self.client.configure_skill(wire::ConfigureSkillRequest {
                        user_session_id: self.current.id.clone(),
                        path: path.into(),
                        enabled,
                        ..Default::default()
                    }),
                    c,
                )
                .await?
                .skill,
                "skill",
            )
        })
    }
}

/// Source activity polling uses the actual caller's busy observation, never
/// clearing busy when an ordinary TurnEnded or command completion is observed.
pub struct PollingSlashActivity(pub Arc<dyn Fn() -> bool + Send + Sync>);
impl SlashActivity for PollingSlashActivity {
    fn wait_until_idle<'a>(
        &'a self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            while (self.0)() {
                tokio::select! {
                    biased;
                    () = c.cancelled() => return Err(tonic::Status::cancelled("operation cancelled").into()),
                    () = tokio::time::sleep(Duration::from_millis(20)) => {},
                }
            }
            Ok(())
        })
    }
}
