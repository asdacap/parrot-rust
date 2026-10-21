//! OpenAI-compatible image token estimates for inspected image dimensions.
//!
//! Unknown models, non-image content, and invalid image dimensions deliberately
//! retain the source fallback cost; format inspection and provider wire
//! encoding are separate.

/// Source fallback charged when image costs cannot be determined.
pub const FALLBACK_IMAGE_TOKENS: i64 = 4096;

#[derive(Clone, Copy)]
struct Rule {
    maximum_dimension: i32,
    patch_budget: i64,
    multiplier_percent: i64,
    base_tokens: i64,
    tile_tokens: i64,
}

impl Rule {
    fn for_model(model_id: &str) -> Option<Self> {
        Some(match model_id {
            "gpt-6-astra" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna" => {
                Self::patches(65_535, 0, 120)
            }
            "gpt-5.5" => Self::patches(6000, 10_000, 120),
            "gpt-5.4" | "gpt-5.4-mini" | "gpt-5.4-nano" => Self::patches(2048, 2500, 120),
            "gpt-5.2" => Self::patches(2048, 6144, 120),
            "gpt-4.1-mini" | "gpt-4.1-mini-2025-04-14" => Self::patches(2048, 6144, 162),
            "gpt-5.1" => Self::tiles(2048, 70, 140),
            "gpt-4.1" | "gpt-4o" => Self::tiles(2048, 85, 170),
            "gpt-4o-mini" => Self::tiles(2048, 2833, 5667),
            _ => return None,
        })
    }

    const fn patches(maximum_dimension: i32, patch_budget: i64, multiplier_percent: i64) -> Self {
        Self {
            maximum_dimension,
            patch_budget,
            multiplier_percent,
            base_tokens: 0,
            tile_tokens: 0,
        }
    }

    const fn tiles(maximum_dimension: i32, base_tokens: i64, tile_tokens: i64) -> Self {
        Self {
            maximum_dimension,
            patch_budget: 0,
            multiplier_percent: 0,
            base_tokens,
            tile_tokens,
        }
    }

    fn calculate(self, mut width: i32, mut height: i32) -> i64 {
        if width <= 0 || height <= 0 {
            return FALLBACK_IMAGE_TOKENS;
        }
        let mut scale =
            1.0_f64.min(f64::from(self.maximum_dimension) / f64::from(width.max(height)));
        width = ((f64::from(width) * scale).floor() as i32).max(1);
        height = ((f64::from(height) * scale).floor() as i32).max(1);
        if self.tile_tokens > 0 {
            scale = 1.0_f64.min(768.0 / f64::from(width.min(height)));
            width = ((f64::from(width) * scale).floor() as i32).max(1);
            height = ((f64::from(height) * scale).floor() as i32).max(1);
            return self.base_tokens
                + patches(width, 512) * patches(height, 512) * self.tile_tokens;
        }
        if self.patch_budget > 0 && patch_count(width, height) > self.patch_budget {
            scale =
                (1024.0 * self.patch_budget as f64 / (f64::from(width) * f64::from(height))).sqrt();
            let width_scale = f64::from(width) * scale / 32.0;
            let height_scale = f64::from(height) * scale / 32.0;
            scale *= (width_scale.floor() / width_scale).min(height_scale.floor() / height_scale);
            width = ((f64::from(width) * scale).floor() as i32).max(1);
            height = ((f64::from(height) * scale).floor() as i32).max(1);
        }
        (patch_count(width, height) * self.multiplier_percent + 99) / 100
    }
}

fn patches(dimension: i32, patch: i64) -> i64 {
    (i64::from(dimension) + patch - 1) / patch
}

fn patch_count(width: i32, height: i32) -> i64 {
    patches(width, 32) * patches(height, 32)
}

/// Calculates the source image calculator result for image content.
pub fn calculate_image_tokens(model_id: &str, width: i32, height: i32) -> i64 {
    calculate_content_tokens(model_id, true, width, height)
}

/// Calculates a content part, using the standard fallback for non-images.
pub fn calculate_content_tokens(model_id: &str, is_image: bool, width: i32, height: i32) -> i64 {
    if !is_image {
        return FALLBACK_IMAGE_TOKENS;
    }
    Rule::for_model(model_id).map_or(FALLBACK_IMAGE_TOKENS, |rule| rule.calculate(width, height))
}
