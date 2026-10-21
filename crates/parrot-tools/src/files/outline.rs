#[derive(Default)]
pub struct MarkdownOutline {
    headings: Vec<String>,
    in_fence: bool,
}

impl MarkdownOutline {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, line_number: usize, line: &str) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            self.in_fence = !self.in_fence;
            return;
        }

        let level = line.len() - line.trim_start_matches('#').len();
        if self.in_fence
            || !(1..=6).contains(&level)
            || (line.len() > level && line.as_bytes()[level] != b' ')
        {
            return;
        }
        self.headings.push(format!("{line_number}: {line}"));
    }

    pub fn is_empty(&self) -> bool {
        self.headings.is_empty()
    }

    pub fn render(&self) -> String {
        self.headings.join("\n")
    }
}
