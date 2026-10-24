use crate::presenters::*;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct WaitToolPresenter {
    clock: Arc<dyn Fn() -> Duration + Send + Sync>,
}
impl Default for WaitToolPresenter {
    fn default() -> Self {
        Self::new()
    }
}
impl WaitToolPresenter {
    pub fn new() -> Self {
        let origin = Instant::now();
        Self::with_clock(Arc::new(move || origin.elapsed()))
    }
    pub fn with_clock(clock: Arc<dyn Fn() -> Duration + Send + Sync>) -> Self {
        Self { clock }
    }
    fn duration(call: &ToolCallPresentation) -> PresenterResult<i64> {
        let root: Value = serde_json::from_str(&call.arguments_json)?;
        if !root.is_object() {
            return Ok(300_000);
        }
        match root.get("duration_ms") {
            None => Ok(300_000),
            Some(Value::Number(value)) => Ok(value.as_i64().unwrap_or(300_000)),
            Some(_) => Err(PresenterError("wait duration_ms must be a number.".into())),
        }
    }
    fn format(seconds: u64) -> String {
        if seconds < 60 {
            format!("{seconds}s")
        } else if seconds < 3600 {
            format!("{}m {:02}s", seconds / 60, seconds % 60)
        } else {
            format!(
                "{}h {:02}m {:02}s",
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            )
        }
    }
}
impl ToolPresenter for WaitToolPresenter {
    fn tool_name(&self) -> &str {
        "wait"
    }
    fn metadata(&self) -> ToolPresentationMetadata {
        ToolPresentationMetadata {
            live_only: true,
            modeline: true,
            ..Default::default()
        }
    }
    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        let milliseconds = Self::duration(call)?;
        let started = (self.clock)();
        let clock = self.clock.clone();
        let mut report = ToolReport::live(
            "Wait for incoming activity",
            ToolBlock::empty(),
            self.metadata(),
            frame,
        );
        report.timer = Some(Arc::new(move || {
            let elapsed = clock().saturating_sub(started).as_secs_f64();
            let seconds = ((milliseconds as f64 / 1000.0 - elapsed).ceil().max(0.0)) as u64;
            format!("{} left", Self::format(seconds))
        }));
        Ok(Presentation::Tool(report))
    }
    fn present_terminal(
        &self,
        _call: &ToolCallPresentation,
        _terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        Ok(None)
    }
}
