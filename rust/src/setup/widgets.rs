//! Grapheme-aware inputs and the high-contrast, scrollable terminal surface.
use super::state::{Action, Field, Screen, SetupState};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextInput {
    pub text: String,
    /// Byte boundary at the beginning of a grapheme (or end of text).
    pub cursor: usize,
    pub selected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    Insert(String),
    Left,
    Right,
    Home,
    End,
    Backspace,
    Delete,
    SelectAll,
}

impl TextInput {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            cursor: text.len(),
            text,
            selected: false,
        }
    }
    pub fn edit(&mut self, edit: Edit) -> bool {
        let before_length = self.text.len();
        match edit {
            Edit::SelectAll => self.selected = true,
            Edit::Home => {
                self.cursor = 0;
                self.selected = false;
            }
            Edit::End => {
                self.cursor = self.text.len();
                self.selected = false;
            }
            Edit::Left => {
                self.cursor = self.previous();
                self.selected = false;
            }
            Edit::Right => {
                self.cursor = self.next();
                self.selected = false;
            }
            Edit::Insert(mut text) => {
                text.retain(|c| !c.is_control());
                let changed = if self.selected {
                    self.text != text
                } else {
                    !text.is_empty()
                };
                self.remove_selection();
                self.text.insert_str(self.cursor, &text);
                self.cursor += text.len();
                self.normalize_cursor();
                return changed;
            }
            Edit::Backspace => {
                if !self.remove_selection() {
                    let start = self.previous();
                    self.text.replace_range(start..self.cursor, "");
                    self.cursor = start;
                }
            }
            Edit::Delete => {
                if !self.remove_selection() {
                    let end = self.next();
                    self.text.replace_range(self.cursor..end, "");
                }
            }
        }
        self.normalize_cursor();
        self.text.len() != before_length
    }
    fn normalize_cursor(&mut self) {
        // Deletion or insertion can join adjacent Unicode grapheme clusters.
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .find(|&index| index >= self.cursor)
            .unwrap_or(self.text.len());
    }
    fn previous(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }
    fn next(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |g| self.cursor + g.len())
    }
    fn remove_selection(&mut self) -> bool {
        if !self.selected {
            return false;
        }
        self.text.clear();
        self.cursor = 0;
        self.selected = false;
        true
    }
    pub fn visible(&self, masked: bool, width: usize) -> (String, usize) {
        let before = self.text[..self.cursor].graphemes(true).count();
        let clusters: Vec<&str> = self.text.graphemes(true).collect();
        let cell_width = |i: usize| {
            if masked {
                1
            } else {
                UnicodeWidthStr::width(clusters[i])
            }
        };
        let mut start = 0;
        let mut cursor_width: usize = (0..before).map(cell_width).sum();
        while cursor_width >= width.max(1) && start < before {
            cursor_width = cursor_width.saturating_sub(cell_width(start));
            start += 1;
        }
        let mut text = String::new();
        let mut used = 0;
        for (i, cluster) in clusters.iter().enumerate().skip(start) {
            let cells = cell_width(i);
            if used + cells > width {
                break;
            }
            if masked {
                text.push('*');
            } else {
                text.extend(cluster.chars().filter(|c| !c.is_control()));
            }
            used += cells;
        }
        (text, cursor_width)
    }
}

pub struct Row {
    pub label: String,
    pub action: Action,
    pub field: Option<Field>,
    pub masked: bool,
}
impl Row {
    pub fn button(label: impl Into<String>, action: Action) -> Self {
        Self {
            label: label.into(),
            action,
            field: None,
            masked: false,
        }
    }
    pub fn input(label: &str, field: Field, masked: bool) -> Self {
        Self {
            label: label.into(),
            action: Action::Input(field),
            field: Some(field),
            masked,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Hit {
    pub area: Rect,
    pub row: usize,
    pub action: Action,
}
#[derive(Default)]
pub struct Surface {
    pub hits: Vec<Hit>,
    pub modal: Option<Rect>,
    pub small: bool,
}

fn literal(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// Rows are also the keyboard focus order. Recomputed geometry makes mouse
/// targeting and scrolling obey exactly the same layout after every resize.
pub fn render(frame: &mut Frame<'_>, state: &mut SetupState) -> Surface {
    let area = frame.area();
    let base = Style::default().fg(Color::White).bg(Color::Black);
    frame.render_widget(Block::default().style(base), area);
    if area.width < 40 || area.height < 12 {
        frame.render_widget(
            Paragraph::new("Resize required: at least 40 x 12\nDraft preserved. Ctrl+C: exit")
                .style(base),
            area,
        );
        return Surface {
            small: true,
            ..Surface::default()
        };
    }
    let title = format!(
        " iSponsorBlockTV / {}{} ",
        state.title(),
        if state.dirty() { " / unsaved" } else { "" }
    );
    frame.render_widget(
        Paragraph::new(title).style(base.add_modifier(Modifier::BOLD)),
        Rect::new(0, 0, area.width, 1),
    );
    let modal = state.screen != Screen::Main;
    let content = if modal {
        let width = area.width.saturating_sub(4).min(88);
        Rect::new((area.width - width) / 2, 1, width, area.height - 6)
    } else {
        Rect::new(0, 1, area.width, area.height - 6)
    };
    if modal {
        frame.render_widget(Clear, content);
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(state.title())
        .style(base);
    let inner = block.inner(content);
    frame.render_widget(block, content);
    let rows = state.rows();
    state.focus = state.focus.min(rows.len().saturating_sub(1));
    let hint = state.hint();
    // The warning remains visible even when the pairing tab is selected.
    let hint_height = (UnicodeWidthStr::width(hint.as_str())
        .div_ceil(usize::from(inner.width.max(1))) as u16)
        .clamp(1, inner.height.saturating_sub(1).max(1));
    frame.render_widget(
        Paragraph::new(literal(&hint))
            .wrap(ratatui::widgets::Wrap { trim: false })
            .style(Style::default().fg(Color::Yellow)),
        Rect::new(inner.x, inner.y, inner.width, hint_height),
    );
    let visible = usize::from(inner.height.saturating_sub(hint_height));
    if state.focus < state.scroll {
        state.scroll = state.focus;
    }
    if state.focus >= state.scroll + visible {
        state.scroll = state.focus.saturating_sub(visible.saturating_sub(1));
    }
    state.scroll = state.scroll.min(rows.len().saturating_sub(visible));
    let mut surface = Surface {
        modal: modal.then_some(content),
        ..Surface::default()
    };
    for (index, row) in rows.iter().enumerate().skip(state.scroll).take(visible) {
        let rect = Rect::new(
            inner.x,
            inner.y + hint_height + (index - state.scroll) as u16,
            inner.width,
            1,
        );
        let selected = index == state.focus;
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            base
        };
        let prefix = if selected { "> " } else { "  " };
        let label = literal(&row.label);
        if let Some(field) = row.field {
            let label = format!("{prefix}{label}: ");
            // Keep input usable in narrow terminals, truncating only its label.
            let max_label = usize::from(rect.width).saturating_sub(8);
            let label: String = label
                .graphemes(true)
                .scan(0, |used, grapheme| {
                    *used += UnicodeWidthStr::width(grapheme);
                    (*used <= max_label).then_some(grapheme)
                })
                .collect();
            let label_width = UnicodeWidthStr::width(label.as_str());
            let input = state.input(field);
            let width = usize::from(rect.width).saturating_sub(label_width);
            let (value, cursor) = input.visible(row.masked, width);
            let style = if selected && input.selected {
                style.add_modifier(Modifier::UNDERLINED)
            } else {
                style
            };
            frame.render_widget(
                Paragraph::new(Line::raw(format!("{label}{value}"))).style(style),
                rect,
            );
            if selected && width > 0 {
                frame.set_cursor_position((
                    rect.x + (label_width + cursor.min(width - 1)) as u16,
                    rect.y,
                ));
            }
        } else {
            frame.render_widget(
                Paragraph::new(Line::raw(format!("{prefix}{label}"))).style(style),
                rect,
            );
        }
        surface.hits.push(Hit {
            area: rect,
            row: index,
            action: row.action.clone(),
        });
    }
    let status = if state.message.is_empty() {
        "Tab/Shift+Tab focus • arrows navigate • Enter activate"
    } else {
        &state.message
    };
    frame.render_widget(
        Paragraph::new(literal(status))
            .wrap(ratatui::widgets::Wrap { trim: false })
            .style(Style::default().fg(Color::Yellow)),
        Rect::new(0, area.height - 5, area.width, 3),
    );
    frame.render_widget(
        Paragraph::new("Tab focus | Space toggle | Esc back | s save | q exit").style(base),
        Rect::new(0, area.height - 2, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new("Ctrl+A select | paste | mouse/wheel | Ctrl+C exit").style(base),
        Rect::new(0, area.height - 1, area.width, 1),
    );
    surface
}
