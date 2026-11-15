use super::Execution;
use parrot_core::{CancellationToken, Content, Error, Message, Role};
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
    path: &'a str,
    media_type: &'a str,
}

#[derive(Serialize)]
struct ToolCallArtifact<'a> {
    id: &'a str,
    name: &'a str,
    arguments_json: &'a str,
}

pub(super) async fn spill_group(
    execution: &Execution<'_>,
    messages: &[Message],
) -> Result<Vec<Message>, Error> {
    let cancellation = CancellationToken::new(execution.cancellation);
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
                    path: "",
                    media_type: "",
                },
                Content::Image {
                    path, media_type, ..
                } => ContentArtifact {
                    kind: "Image",
                    text: "",
                    path,
                    media_type,
                },
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
    let path = execution
        .effects
        .write_scratch(&writer.contents, execution.cancellation)
        .await?;
    let notice = execution
        .templates
        .render(
            "compaction.oversized-tool-group-notice",
            &[("path".into(), path.into())],
            &parrot_core::NeverCancelled,
        )
        .map_err(|error| Error::template(error.to_string()))?;
    Ok(vec![Message {
        role: Role::System,
        contents: vec![Content::Text(notice)],
        ..Message::default()
    }])
}
