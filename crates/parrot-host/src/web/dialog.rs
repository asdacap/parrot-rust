//! Browser frames implementing the CLI dialog interactions.

use parrot_domain::{
    contracts::async_ops::Cancellation,
    protocol::messages::{parrot as wire, web},
};
use std::{collections::HashMap, fmt::Write, future::Future, sync::Mutex};
use tokio::sync::{mpsc, oneshot};
use tonic::Status;

pub struct WebSlashDialog {
    frames: mpsc::UnboundedSender<web::SlashFrame>,
    prompts: Mutex<HashMap<String, oneshot::Sender<web::AnswerSlashPromptRequest>>>,
}

impl WebSlashDialog {
    pub fn new(frames: mpsc::UnboundedSender<web::SlashFrame>) -> Self {
        Self {
            frames,
            prompts: Mutex::new(HashMap::new()),
        }
    }

    pub fn answer(&self, answer: web::AnswerSlashPromptRequest) -> bool {
        self.prompts
            .lock()
            .expect("slash prompts lock")
            .remove(&answer.prompt_id)
            .is_some_and(|prompt| prompt.send(answer).is_ok())
    }

    pub async fn select(
        &self,
        title: &str,
        options: &[web::SlashOption],
        cancellation: &dyn Cancellation,
    ) -> Result<Option<web::SlashOption>, Status> {
        let answer = self
            .ask(
                |prompt_id| {
                    web::slash_frame::Payload::Select(web::SlashSelect {
                        prompt_id,
                        title: title.into(),
                        options: options.to_vec(),
                        ..Default::default()
                    })
                },
                cancellation,
            )
            .await?;
        Ok(match answer.answer {
            Some(web::answer_slash_prompt_request::Answer::OptionId(id)) => {
                options.iter().find(|option| option.id == id).cloned()
            }
            _ => None,
        })
    }

    pub async fn read_text(
        &self,
        prompt: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<String>, Status> {
        self.read(prompt, false, cancellation).await
    }

    pub async fn read_secret(
        &self,
        prompt: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<String>, Status> {
        self.read(prompt, true, cancellation).await
    }

    async fn read(
        &self,
        prompt: &str,
        secret: bool,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<String>, Status> {
        let answer = self
            .ask(
                |prompt_id| {
                    web::slash_frame::Payload::ReadText(web::SlashReadText {
                        prompt_id,
                        prompt: prompt.into(),
                        secret,
                        ..Default::default()
                    })
                },
                cancellation,
            )
            .await?;
        Ok(match answer.answer {
            Some(web::answer_slash_prompt_request::Answer::Text(text)) => Some(text),
            _ => None,
        })
    }

    pub async fn confirm(
        &self,
        lines: &[String],
        cancellation: &dyn Cancellation,
    ) -> Result<bool, Status> {
        let answer = self
            .ask(
                |prompt_id| {
                    web::slash_frame::Payload::Confirm(web::SlashConfirm {
                        prompt_id,
                        lines: lines.to_vec(),
                        ..Default::default()
                    })
                },
                cancellation,
            )
            .await?;
        Ok(matches!(
            answer.answer,
            Some(web::answer_slash_prompt_request::Answer::Confirmed(true))
        ))
    }

    pub async fn show(
        &self,
        lines: &[String],
        cancellation: &dyn Cancellation,
    ) -> Result<(), Status> {
        self.ask(
            |prompt_id| {
                web::slash_frame::Payload::Show(web::SlashShow {
                    prompt_id,
                    lines: lines.to_vec(),
                    ..Default::default()
                })
            },
            cancellation,
        )
        .await
        .map(|_| ())
    }

    pub async fn print(
        &self,
        lines: &[String],
        cancellation: &dyn Cancellation,
    ) -> Result<(), Status> {
        self.emit(
            web::slash_frame::Payload::Print(web::SlashPrint {
                lines: lines.to_vec(),
                ..Default::default()
            }),
            cancellation,
        )
    }

    pub async fn show_error(
        &self,
        message: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<(), Status> {
        self.emit(
            web::slash_frame::Payload::Error(web::SlashError {
                message: message.into(),
                ..Default::default()
            }),
            cancellation,
        )
    }

    pub async fn session_replaced(
        &self,
        session: wire::UserSession,
        cancellation: &dyn Cancellation,
    ) -> Result<(), Status> {
        self.emit(
            web::slash_frame::Payload::SessionReplaced(session),
            cancellation,
        )
    }

    pub async fn load<T, E, F>(
        &self,
        activity: &str,
        load: F,
        cancellation: &dyn Cancellation,
    ) -> Result<T, E>
    where
        E: From<Status>,
        F: Future<Output = Result<T, E>>,
    {
        self.emit(
            web::slash_frame::Payload::Loading(web::SlashLoading {
                activity: activity.into(),
                ..Default::default()
            }),
            cancellation,
        )?;
        let _loaded = Loaded(&self.frames);
        load.await
    }

    fn emit(
        &self,
        payload: web::slash_frame::Payload,
        cancellation: &dyn Cancellation,
    ) -> Result<(), Status> {
        if cancellation.is_cancelled() {
            return Err(Status::cancelled("operation cancelled"));
        }
        self.frames
            .send(web::SlashFrame {
                payload: Some(payload),
                ..Default::default()
            })
            .map_err(|_| Status::cancelled("slash stream closed"))
    }

    async fn ask(
        &self,
        frame: impl FnOnce(String) -> web::slash_frame::Payload,
        cancellation: &dyn Cancellation,
    ) -> Result<web::AnswerSlashPromptRequest, Status> {
        let prompt_id = random_id()?;
        let (answer, received) = oneshot::channel();
        self.prompts
            .lock()
            .expect("slash prompts lock")
            .insert(prompt_id.clone(), answer);
        let _pending = PendingPrompt {
            dialog: self,
            id: &prompt_id,
        };
        self.emit(frame(prompt_id.clone()), cancellation)?;
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(Status::cancelled("operation cancelled")),
            result = received => result.map_err(|_| Status::cancelled("slash prompt closed")),
        }
    }
}

pub(super) fn random_id() -> Result<String, Status> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| Status::internal(error.to_string()))?;
    // Guid.NewGuid().ToString("N"): UUID version 4 with no separators.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut id = String::with_capacity(32);
    for byte in bytes {
        write!(id, "{byte:02x}").expect("writing into a String cannot fail");
    }
    Ok(id)
}

struct PendingPrompt<'a> {
    dialog: &'a WebSlashDialog,
    id: &'a str,
}
impl Drop for PendingPrompt<'_> {
    fn drop(&mut self) {
        self.dialog
            .prompts
            .lock()
            .expect("slash prompts lock")
            .remove(self.id);
    }
}

struct Loaded<'a>(&'a mpsc::UnboundedSender<web::SlashFrame>);
impl Drop for Loaded<'_> {
    fn drop(&mut self) {
        let _ = self.0.send(web::SlashFrame {
            payload: Some(web::slash_frame::Payload::Loaded(web::SlashLoaded::new())),
            ..Default::default()
        });
    }
}
