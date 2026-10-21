use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::surface::TerminalSurface;
use super::{
    CancellationToken, ItemIdentity, LiveBufferCaret, LiveBufferItem, LiveBufferRenderContext,
    LiveBufferRetention, ScrollbackItem, ScrollbackLayout, ScrollbackRenderContext, TerminalFrame,
    TerminalLine, TerminalStyle,
};
use crate::terminal_layout::TerminalText;

const HIDE: &str = "\x1b[?25l\x1b[?7l";
const SHOW: &str = "\x1b[?7h\x1b[?25h";

/// Owns an inline terminal frame and serializes complete render/write operations.
pub struct TerminalFrameRenderer<W: Write + Send> {
    state: Mutex<State<W>>,
    gate: AdmissionGate,
    color: bool,
    max_live_rows: usize,
    max_input_rows: usize,
    inline_diff: bool,
}

#[derive(Default)]
struct Admission {
    next: u64,
    active: bool,
    waiting: VecDeque<u64>,
}

#[derive(Default)]
struct AdmissionGate {
    state: Mutex<Admission>,
    changed: Condvar,
}

struct AdmissionGuard<'a>(&'a AdmissionGate);

impl AdmissionGate {
    fn enter(&self, cancel: &CancellationToken) -> io::Result<AdmissionGuard<'_>> {
        cancel.check()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Drawing gate poisoned."))?;
        let ticket = state.next;
        state.next = state.next.wrapping_add(1);
        state.waiting.push_back(ticket);
        loop {
            if let Err(error) = cancel.check() {
                state.waiting.retain(|value| *value != ticket);
                self.changed.notify_all();
                return Err(error);
            }
            if !state.active && state.waiting.front() == Some(&ticket) {
                state.waiting.pop_front();
                state.active = true;
                return Ok(AdmissionGuard(self));
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(10))
                .map_err(|_| io::Error::other("Drawing gate poisoned."))?
                .0;
        }
    }
}

impl Drop for AdmissionGuard<'_> {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.active = false;
        self.0.changed.notify_all();
    }
}

struct State<W> {
    output: W,
    pending_output: String,
    surface: TerminalSurface,
    active_scrollback: Option<Arc<dyn ScrollbackItem>>,
    pending_scrollback: Vec<Arc<dyn ScrollbackItem>>,
    caret_row: usize,
    committed: bool,
    committed_gap: bool,
    frame: Option<TerminalFrame>,
    last_layout: ScrollbackLayout,
    last_packing: Option<ItemIdentity>,
    rendered_height: usize,
    rendered_width: usize,
}

impl<W: Write + Send> TerminalFrameRenderer<W> {
    pub fn new(
        output: W,
        color: bool,
        max_live_rows: isize,
        max_input_rows: isize,
        inline_diff: bool,
    ) -> Self {
        Self {
            state: Mutex::new(State {
                output,
                pending_output: String::new(),
                surface: TerminalSurface::new(1, 1),
                active_scrollback: None,
                pending_scrollback: Vec::new(),
                caret_row: 0,
                committed: false,
                committed_gap: false,
                frame: None,
                last_layout: ScrollbackLayout::Compact,
                last_packing: None,
                rendered_height: 0,
                rendered_width: 0,
            }),
            gate: AdmissionGate::default(),
            color,
            max_live_rows: max_live_rows.max(1) as usize,
            max_input_rows: max_input_rows.max(1) as usize,
            inline_diff,
        }
    }

    pub fn draw(
        &self,
        items: &[Arc<dyn LiveBufferItem>],
        columns: isize,
        rows: isize,
        cancel: &CancellationToken,
    ) -> io::Result<()> {
        let _admission = self.gate.enter(cancel)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Renderer state poisoned."))?;
        let width = columns.max(1) as usize;
        let frame = self.render(items, width, rows.max(1) as usize)?;
        if state.frame.is_none() {
            state.draw_frame(&frame, width, 1, self.color);
        } else {
            state.replace_frame(&frame, width, self.color);
        }
        if !state.pending_output.is_empty() {
            state.write_pending_output()?;
        }
        state.frame = Some(frame);
        state.rendered_width = width;
        Ok(())
    }

    pub fn commit(
        &self,
        item: Arc<dyn ScrollbackItem>,
        items: &[Arc<dyn LiveBufferItem>],
        columns: isize,
        rows: isize,
        cancel: &CancellationToken,
    ) -> io::Result<()> {
        let _admission = self.gate.enter(cancel)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Renderer state poisoned."))?;
        let width = columns.max(1) as usize;
        let frame = self.render(items, width, rows.max(1) as usize)?;
        let mut active = state.active_scrollback.clone();
        let mut pending = state.pending_scrollback.clone();
        let mut emitted = Vec::new();
        if active
            .as_ref()
            .is_none_or(|previous| item.continues(previous.as_ref()))
        {
            emit(item, &mut emitted, &mut pending, &mut active);
        } else {
            pending.push(item);
        }
        let context = ScrollbackRenderContext {
            columns: width,
            color: self.color,
            inline_diff: self.inline_diff,
            previous_packing: None,
        };
        let lines = state.render_scrollback(&emitted, context)?;
        let available = if lines.is_empty() {
            state.rendered_height.max(1)
        } else {
            1
        };
        state.active_scrollback = active;
        state.pending_scrollback = pending;
        state.clear_frame();
        for line in lines {
            state.pending_output.push_str(&line);
            state.pending_output.push_str("\r\n");
        }
        state.draw_frame(&frame, width, available, self.color);
        state.write_pending_output()?;
        state.frame = Some(frame);
        state.rendered_width = width;
        Ok(())
    }

    pub fn clear(&self, cancel: &CancellationToken) -> io::Result<()> {
        let _admission = self.gate.enter(cancel)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Renderer state poisoned."))?;
        state.clear_frame();
        state.frame = None;
        state.rendered_width = 0;
        state.write_pending_output()
    }

    fn render(
        &self,
        items: &[Arc<dyn LiveBufferItem>],
        width: usize,
        height: usize,
    ) -> io::Result<TerminalFrame> {
        let context = LiveBufferRenderContext {
            columns: width,
            color: self.color,
        };
        let rendered = items
            .iter()
            .map(|item| item.render(context))
            .collect::<io::Result<Vec<_>>>()?;
        if rendered.iter().filter(|item| item.caret.is_some()).count() != 1 {
            return Err(invalid("The live buffer must contain exactly one caret."));
        }
        let tail_rows: usize = rendered
            .iter()
            .filter(|item| item.retention == LiveBufferRetention::Tail)
            .map(|item| item.lines.len())
            .sum();
        let mut tail_to_skip = tail_rows.saturating_sub(self.max_live_rows);
        let mut lines = Vec::new();
        let mut caret = None;
        for item in rendered {
            if item
                .caret
                .is_some_and(|caret| caret.row < 0 || caret.row as usize >= item.lines.len())
            {
                return Err(invalid("The live buffer caret is outside its item."));
            }
            let mut start = 0;
            let mut count = item.lines.len();
            if item.retention == LiveBufferRetention::Tail {
                start = tail_to_skip.min(count);
                count -= start;
                tail_to_skip -= start;
            } else if item.retention == LiveBufferRetention::Caret && count > self.max_input_rows {
                let position = item.caret.ok_or_else(|| {
                    invalid("Caret-retained live buffer content requires a caret.")
                })?;
                start = (position.row as usize + 1)
                    .saturating_sub(self.max_input_rows)
                    .min(count - self.max_input_rows);
                count = self.max_input_rows;
            }
            if let Some(position) = item.caret {
                if (position.row as usize) < start || position.row as usize >= start + count {
                    return Err(invalid("The live buffer caret was removed by retention."));
                }
                caret = Some(LiveBufferCaret {
                    row: (lines.len() + position.row as usize - start) as isize,
                    cells: position.cells.clamp(0, width as isize - 1),
                });
            }
            for line in &item.lines[start..start + count] {
                lines.push(TerminalLine::with_spans(
                    TerminalText::sanitize(line.text()).replace('\n', ""),
                    line.style().clone(),
                    line.style_spans().to_vec(),
                ));
            }
        }
        let mut caret = caret.ok_or_else(|| invalid("The live buffer has no caret."))?;
        let top = lines.len().saturating_sub(height).min(caret.row as usize);
        lines = lines[top..(top + height).min(lines.len())].to_vec();
        caret.row -= top as isize;
        Ok(TerminalFrame { lines, caret })
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn emit(
    item: Arc<dyn ScrollbackItem>,
    emitted: &mut Vec<Arc<dyn ScrollbackItem>>,
    pending: &mut Vec<Arc<dyn ScrollbackItem>>,
    active: &mut Option<Arc<dyn ScrollbackItem>>,
) {
    *active = if item.is_completed() {
        None
    } else {
        Some(item.clone())
    };
    emitted.push(item);
    loop {
        let next = if let Some(current) = active {
            pending
                .iter()
                .position(|item| item.continues(current.as_ref()))
        } else if pending.is_empty() {
            None
        } else {
            Some(0)
        };
        let Some(index) = next else { return };
        let item = pending.remove(index);
        *active = if item.is_completed() {
            None
        } else {
            Some(item.clone())
        };
        emitted.push(item);
    }
}

impl<W: Write> State<W> {
    fn render_scrollback(
        &mut self,
        items: &[Arc<dyn ScrollbackItem>],
        mut context: ScrollbackRenderContext,
    ) -> io::Result<Vec<String>> {
        let mut output = Vec::new();
        for item in items {
            context.previous_packing = self.last_packing.clone();
            let rendered = item.render(context.clone())?;
            let layout = item.layout();
            let needs_gap = match layout {
                ScrollbackLayout::User | ScrollbackLayout::Assistant => !self.committed_gap,
                ScrollbackLayout::Block => self.committed && !self.committed_gap,
                ScrollbackLayout::Compact => {
                    self.committed
                        && !self.committed_gap
                        && self.last_layout == ScrollbackLayout::Block
                }
            };
            if item.starts_layout()
                && !rendered.is_empty()
                && !item.packs(self.last_packing.as_ref())
                && needs_gap
            {
                output.push(String::new());
                self.committed_gap = true;
            }
            if !rendered.is_empty() {
                self.committed = true;
                self.committed_gap = false;
                self.last_layout = layout;
                self.last_packing = item.packing_identity();
            }
            output.extend(rendered);
            if item.ends_layout() && layout != ScrollbackLayout::Compact && !self.committed_gap {
                output.push(String::new());
                self.committed = true;
                self.committed_gap = true;
                self.last_layout = layout;
            }
        }
        Ok(output)
    }

    fn render_surface_row(&mut self, row: usize, line: &TerminalLine, color: bool) -> String {
        self.surface.write(row, line.text(), line.style());
        for span in line.style_spans() {
            self.surface
                .apply_style(row, span.start_cell, span.length, &span.style);
        }
        self.surface.render_row(
            row,
            &TerminalStyle::new(if color { "\x1b[48;5;236m" } else { "" }),
        )
    }

    fn draw_frame(
        &mut self,
        frame: &TerminalFrame,
        width: usize,
        available_rows: usize,
        color: bool,
    ) {
        self.rendered_height = frame.lines.len();
        self.caret_row = frame.caret.row as usize;
        self.surface.resize(width, self.rendered_height);
        self.surface.clear();
        self.pending_output.push_str(HIDE);
        let reserve = self.rendered_height.saturating_sub(available_rows);
        for _ in 0..reserve {
            self.pending_output.push_str("\r\n");
        }
        if reserve > 0 {
            let _ = write!(self.pending_output, "\x1b[{reserve}A\r");
        }
        for (row, line) in frame.lines.iter().enumerate() {
            self.pending_output.push_str("\x1b[2K");
            let rendered = self.render_surface_row(row, line, color);
            self.pending_output.push_str(&rendered);
            if row + 1 < self.rendered_height {
                self.pending_output.push_str("\r\n");
            }
        }
        let last = self.rendered_height - 1;
        if last > self.caret_row {
            let _ = write!(self.pending_output, "\x1b[{}A", last - self.caret_row);
        }
        self.pending_output.push('\r');
        if frame.caret.cells > 0 {
            let _ = write!(self.pending_output, "\x1b[{}C", frame.caret.cells);
        }
        self.pending_output.push_str(SHOW);
    }

    fn replace_frame(&mut self, frame: &TerminalFrame, width: usize, color: bool) {
        let previous = self.frame.as_ref().expect("previous frame");
        let changed: Vec<_> = frame
            .lines
            .iter()
            .enumerate()
            .filter_map(|(row, line)| {
                (width != self.rendered_width || previous.lines.get(row) != Some(line))
                    .then_some(row)
            })
            .collect();
        if changed.is_empty()
            && previous.lines.len() == frame.lines.len()
            && previous.caret == frame.caret
        {
            return;
        }
        let previous_height = self.rendered_height;
        let below = previous_height - self.caret_row - 1;
        self.pending_output.push_str(HIDE);
        if below > 0 {
            let _ = write!(self.pending_output, "\x1b[{below}B");
        }
        self.pending_output.push('\r');
        for _ in 0..frame.lines.len().saturating_sub(previous_height) {
            self.pending_output.push_str("\r\n");
        }
        let above = previous_height.max(frame.lines.len()) - 1;
        if above > 0 {
            let _ = write!(self.pending_output, "\x1b[{above}A\r");
        }
        self.surface.resize(width, frame.lines.len());
        self.surface.clear();
        let mut current = 0;
        for row in changed {
            self.move_to_row(current, row);
            current = row;
            self.pending_output.push_str("\x1b[2K");
            let rendered = self.render_surface_row(row, &frame.lines[row], color);
            self.pending_output.push_str(&rendered);
        }
        let removed = previous_height.saturating_sub(frame.lines.len());
        if removed > 0 {
            self.move_to_row(current, frame.lines.len() - 1);
            let _ = write!(self.pending_output, "\x1b[B\r\x1b[{removed}M");
            current = frame.lines.len();
        }
        self.move_to_row(current, frame.caret.row as usize);
        if frame.caret.cells > 0 {
            let _ = write!(self.pending_output, "\x1b[{}C", frame.caret.cells);
        }
        self.pending_output.push_str(SHOW);
        self.rendered_height = frame.lines.len();
        self.caret_row = frame.caret.row as usize;
    }

    fn move_to_row(&mut self, current: usize, target: usize) {
        match target.cmp(&current) {
            std::cmp::Ordering::Greater => {
                let _ = write!(self.pending_output, "\x1b[{}B", target - current);
            }
            std::cmp::Ordering::Less => {
                let _ = write!(self.pending_output, "\x1b[{}A", current - target);
            }
            std::cmp::Ordering::Equal => {}
        }
        self.pending_output.push('\r');
    }

    fn clear_frame(&mut self) {
        if self.rendered_height == 0 {
            return;
        }
        self.pending_output.push_str(HIDE);
        let below = self.rendered_height - self.caret_row - 1;
        if below > 0 {
            let _ = write!(self.pending_output, "\x1b[{below}B");
        }
        self.pending_output.push('\r');
        if self.rendered_height > 1 {
            let _ = write!(self.pending_output, "\x1b[{}A", self.rendered_height - 1);
        }
        for row in 0..self.rendered_height {
            self.pending_output.push_str("\x1b[2K");
            if row + 1 < self.rendered_height {
                self.pending_output.push_str("\r\n");
            }
        }
        if self.rendered_height > 1 {
            let _ = write!(self.pending_output, "\x1b[{}A", self.rendered_height - 1);
        }
        self.pending_output.push('\r');
        self.pending_output.push_str(SHOW);
        self.rendered_height = 0;
        self.caret_row = 0;
        self.rendered_width = 0;
    }

    fn write_pending_output(&mut self) -> io::Result<()> {
        let pending = std::mem::take(&mut self.pending_output);
        if !pending.is_empty() {
            self.output.write_all(pending.as_bytes())?;
        }
        self.output.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn admission_is_fifo_and_cancelled_waiter_is_removed_before_release() {
        let gate = Arc::new(AdmissionGate::default());
        let initial = gate.enter(&CancellationToken::new()).unwrap();
        let (admitted_tx, admitted_rx) = mpsc::channel();
        let (cancelled_tx, cancelled_rx) = mpsc::channel();
        let mut workers = Vec::new();
        let mut cancellation = None;
        for index in 0..3 {
            let cancel = CancellationToken::new();
            if index == 1 {
                cancellation = Some(cancel.clone());
            }
            let worker_gate = gate.clone();
            let admitted = admitted_tx.clone();
            let cancelled = cancelled_tx.clone();
            workers.push(thread::spawn(move || match worker_gate.enter(&cancel) {
                Ok(_guard) => admitted.send(index).unwrap(),
                Err(error) => cancelled.send((index, error.kind())).unwrap(),
            }));
            let deadline = Instant::now() + Duration::from_secs(5);
            while gate.state.lock().unwrap().waiting.len() != index + 1 {
                assert!(Instant::now() < deadline, "waiter did not enqueue");
                thread::yield_now();
            }
        }
        cancellation.unwrap().cancel();
        assert_eq!(
            cancelled_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            (1, io::ErrorKind::Interrupted)
        );
        assert_eq!(gate.state.lock().unwrap().waiting.len(), 2);
        drop(initial);
        assert_eq!(admitted_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 0);
        assert_eq!(admitted_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 2);
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(!gate.state.lock().unwrap().active);
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(gate.enter(&cancelled).is_err());
        assert!(gate.state.lock().unwrap().waiting.is_empty());
    }
}
