use parrot_core::{Message, Role};
use parrot_domain::values::context::TokenEstimator;
use serde::Deserialize;

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "Version")]
    version: i32,
    #[serde(flatten)]
    replay: Replay,
}

#[derive(Deserialize)]
#[serde(tag = "Protocol")]
enum Replay {
    #[serde(rename = "chat-completions")]
    Chat {
        #[serde(rename = "ReasoningContent")]
        reasoning_content: Option<String>,
        #[serde(rename = "Reasoning")]
        reasoning: Option<String>,
    },
    #[serde(rename = "responses")]
    Responses {
        #[serde(rename = "Output")]
        output: Vec<ResponsesItem>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ResponsesItem {
    #[serde(rename = "reasoning")]
    Reasoning {
        #[serde(default)]
        summary: Vec<ReasoningText>,
        #[serde(default)]
        content: Vec<ReasoningText>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ReasoningText {
    #[serde(rename = "summary_text", alias = "reasoning_text")]
    Text { text: String },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum AnthropicBlock {
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    #[serde(other)]
    Other,
}

pub(super) fn estimate_tokens(message: &Message, provider_id: &str) -> i64 {
    if message.role != Role::Assistant || message.provider_id != provider_id {
        return 0;
    }
    let raw = message.provider_content.as_str();
    if raw.trim_start().starts_with('[') {
        return serde_json::from_str::<Vec<AnthropicBlock>>(raw).map_or(0, |blocks| {
            blocks
                .iter()
                .map(|block| match block {
                    AnthropicBlock::Thinking { thinking } => {
                        TokenEstimator::estimate_tokens(thinking)
                    }
                    AnthropicBlock::Other => 0,
                })
                .sum()
        });
    }
    let Ok(envelope) = serde_json::from_str::<Envelope>(raw) else {
        return 0;
    };
    if envelope.version != 1 {
        return 0;
    }
    match envelope.replay {
        Replay::Chat {
            reasoning_content,
            reasoning,
        } => reasoning_content
            .iter()
            .chain(&reasoning)
            .map(|text| TokenEstimator::estimate_tokens(text))
            .sum(),
        Replay::Responses { output } => output
            .iter()
            .map(|item| match item {
                ResponsesItem::Reasoning { summary, content } => summary
                    .iter()
                    .chain(content)
                    .map(|part| match part {
                        ReasoningText::Text { text } => TokenEstimator::estimate_tokens(text),
                        ReasoningText::Other => 0,
                    })
                    .sum(),
                ResponsesItem::Other => 0,
            })
            .sum(),
        Replay::Other => 0,
    }
}
