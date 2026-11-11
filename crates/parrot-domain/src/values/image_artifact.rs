use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageArtifactMetadata {
    pub artifact_id: String,
    pub sha256: String,
    pub media_type: String,
    pub byte_length: i64,
    pub width: u32,
    pub height: u32,
    pub frame_count: u32,
    pub aggregate_pixels: i64,
    pub display_name: String,
    pub origin: String,
}
