#[derive(Clone, Debug, Eq, PartialEq)]
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

impl ImageArtifactMetadata {
    /// Equality of the content-addressed fields, excluding the first-reference labels.
    pub fn matches_content(&self, other: &Self) -> bool {
        self.artifact_id == other.artifact_id
            && self.sha256 == other.sha256
            && self.media_type == other.media_type
            && self.byte_length == other.byte_length
            && self.width == other.width
            && self.height == other.height
            && self.frame_count == other.frame_count
            && self.aggregate_pixels == other.aggregate_pixels
    }
}
