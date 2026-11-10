use crate::{CancellationToken, ConfigKey, Content, Error, Message, Role, States, rendering};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use std::io::{self, Write};

struct ArtifactWriter<'a> {
    contents: Vec<u8>,
    cancellation: CancellationToken<'a>,
}

impl Write for ArtifactWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::other("The operation was canceled."));
        }
        self.contents.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Serialize)]
struct GroupArtifact<'a> {
    messages: Vec<MessageArtifact<'a>>,
}

#[derive(Serialize)]
struct MessageArtifact<'a> {
    role: &'static str,
    contents: Vec<ContentArtifact<'a>>,
    tool_calls: Vec<ToolCallArtifact<'a>>,
    tool_call_id: &'a str,
}

#[derive(Serialize)]
struct ContentArtifact<'a> {
    kind: &'static str,
    text: &'a str,
    image: String,
    media_type: &'a str,
}

#[derive(Serialize)]
struct ToolCallArtifact<'a> {
    id: &'a str,
    name: &'a str,
    arguments_json: &'a str,
}

pub(super) async fn spill_group(
    states: &dyn States,
    messages: &[Message],
    cancellation: CancellationToken<'_>,
) -> Result<Vec<Message>, Error> {
    if cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    let mut artifact = GroupArtifact {
        messages: Vec::with_capacity(messages.len()),
    };
    for message in messages {
        let mut contents = Vec::with_capacity(message.contents.len());
        for content in &message.contents {
            contents.push(match content {
                Content::Text(text) => ContentArtifact {
                    kind: "Text",
                    text,
                    image: String::new(),
                    media_type: "",
                },
                Content::Image {
                    path,
                    bytes,
                    media_type,
                    ..
                } => ContentArtifact {
                    kind: "Image",
                    text: "",
                    image: match bytes {
                        Some(bytes) => STANDARD.encode(bytes),
                        None => STANDARD.encode(states.read_file(path, cancellation).await?),
                    },
                    media_type,
                },
                Content::Artifact(_) => {
                    return Err(Error::failed(
                        "Compaction image content has not been materialized.",
                    ));
                }
            });
        }
        artifact.messages.push(MessageArtifact {
            role: match message.role {
                Role::System => "System",
                Role::User => "User",
                Role::Assistant => "Assistant",
                Role::Tool => "Tool",
            },
            contents,
            tool_calls: message
                .tool_calls
                .iter()
                .map(|call| ToolCallArtifact {
                    id: &call.id,
                    name: &call.name,
                    arguments_json: &call.arguments_json,
                })
                .collect(),
            tool_call_id: &message.tool_call_id,
        });
    }
    let mut writer = ArtifactWriter {
        contents: Vec::new(),
        cancellation,
    };
    serde_json::to_writer_pretty(&mut writer, &artifact).map_err(|error| {
        if cancellation.is_cancelled() {
            Error::cancelled()
        } else {
            Error::failed(format!("Failed to serialize compaction messages: {error}"))
        }
    })?;
    let path = states
        .write_to_scratch(&writer.contents, cancellation)
        .await?;
    let notice = rendering::render_template(
        states,
        ConfigKey::OversizedToolGroupTemplate,
        &[("path".into(), path.into())],
    )
    .await?;
    Ok(vec![Message {
        role: Role::System,
        contents: vec![Content::Text(notice)],
        ..Message::default()
    }])
}
