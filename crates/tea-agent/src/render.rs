//! Presentation projection from [`crate::app::AppState`] to terminal rows.
//!
//! The renderer owns presentation semantics above the core event boundary.
//! Core events remain lossless; this layer decides how a user, assistant,
//! tool, notice, or Markdown table occupies terminal rows.

use crate::app::{AppState, NoticeSeverity, ToolProjection, ToolState, TranscriptEntry, UiSurface};
#[cfg(test)]
use crate::composer::Composer;
use crate::ui::frame_layout;
use crate::ui::theme::{Role, Theme};
use crate::ui::visual_layout::VisualLayout;
use hi_lite::{Highlighter, Kind, Language};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tea_providers::ProviderRegistry;
#[cfg(test)]
use tea_tui::Color;
use tea_tui::Style;
use tea_tui::{Cursor, Size, StyledLine};

/// Public measured-frame contract for consumers that need layout without painting.
pub use crate::ui::frame_layout::FrameLayout;

/// Plan terminal footer regions independently of transcript rendering.
pub fn measured_frame_layout(
    width: u16,
    height: u16,
    composer_rows: usize,
    menu_rows: usize,
) -> FrameLayout {
    frame_layout::plan_flow(width, height, 0, 0, composer_rows, menu_rows, 1)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RenderLine {
    text: String,
    style: Style,
    character_styles: Option<Vec<Style>>,
}

impl RenderLine {
    fn plain(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            character_styles: None,
        }
    }

    fn styled(text: impl Into<String>, style: Style, character_styles: Vec<Style>) -> Self {
        Self {
            text: text.into(),
            style,
            character_styles: Some(character_styles),
        }
    }
}

/// Main-screen presentation split at the durable terminal projection frontier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MainPresentation {
    pub(crate) commit: Vec<StyledLine>,
    pub(crate) live: Vec<StyledLine>,
    pub(crate) cursor: Option<Cursor>,
}

/// A temporary alternate-screen surface projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SurfacePresentation {
    pub(crate) lines: Vec<StyledLine>,
    pub(crate) cursor: Option<Cursor>,
}

/// Return the largest front-contiguous transcript prefix that is safe to
/// commit permanently to native terminal scrollback.
pub(crate) fn stable_prefix(entries: &[TranscriptEntry]) -> usize {
    entries
        .iter()
        .take_while(|entry| match entry {
            TranscriptEntry::Welcome { .. }
            | TranscriptEntry::User { .. }
            | TranscriptEntry::Notice { .. }
            | TranscriptEntry::Error { .. } => true,
            TranscriptEntry::Assistant { streaming, .. } => !streaming,
            TranscriptEntry::Tool(tool) => {
                matches!(tool.state, ToolState::Completed | ToolState::Failed)
            }
        })
        .count()
}

/// Render newly stable semantic entries as permanently committed physical rows.
pub(crate) fn committed_lines(
    state: &AppState,
    start: usize,
    end: usize,
    width: u16,
) -> Vec<StyledLine> {
    let mut output = Vec::new();
    for (index, entry) in state
        .transcript()
        .iter()
        .enumerate()
        .skip(start)
        .take(end.saturating_sub(start))
    {
        if index != 0 {
            output.push(styled(&RenderLine::plain(String::new(), Style::default())));
        }
        output.extend(entry_lines_for_entry(entry, width).iter().map(styled));
    }
    output
}

/// Project only the semantic suffix that is not yet committed, followed by
/// activity, composer, slash completion, and footer rows.
pub(crate) fn main_presentation(
    state: &AppState,
    registry: &ProviderRegistry,
    size: Size,
    committed_entries: usize,
) -> MainPresentation {
    let width = size.width;
    let entries = state.transcript();
    let suffix_start = committed_entries.min(entries.len());
    let suffix = &entries[suffix_start..];
    let mut lines = Vec::new();

    // A commit ends on a fresh terminal row. That row is the existing tea
    // breathing/separator row, so it becomes part of the mutable tail rather
    // than an application-owned historical viewport.
    if suffix.is_empty() {
        if suffix_start != 0 {
            lines.push(RenderLine::plain(String::new(), Style::default()));
        }
    } else {
        if suffix_start != 0 {
            lines.push(RenderLine::plain(String::new(), Style::default()));
        }
        for (index, entry) in suffix.iter().enumerate() {
            if index != 0 {
                lines.push(RenderLine::plain(String::new(), Style::default()));
            }
            lines.extend(entry_lines_for_entry(entry, width));
        }
        // Preserve the prior transcript-to-activity breathing row.
        lines.push(RenderLine::plain(String::new(), Style::default()));
    }

    lines.extend(activity_lines(state, width));
    lines.extend(history_search_lines(state, width, 3));
    let desired_composer_rows = composer_layout(state, width).rows.len().max(1);
    let composer_capacity = if entries.is_empty() {
        usize::from(size.height).max(1)
    } else {
        usize::from(size.height.saturating_sub(3).max(1))
    };
    let visual = composer_layout(state, width);
    let composer_start =
        composer_view_start(&visual, desired_composer_rows.min(composer_capacity) as u16);
    let composer_row = lines.len();
    let theme = Theme::default();
    for (row, line) in visual.rows.iter().skip(composer_start).enumerate() {
        if row >= composer_capacity {
            break;
        }
        let text = line.text.strip_prefix("❯ ").unwrap_or(&line.text);
        let prefix = if composer_start != 0 && row == 0 {
            "┃↑"
        } else {
            "┃ "
        };
        lines.push(RenderLine::plain(
            format!("{prefix}{text}"),
            theme.style(Role::Text),
        ));
    }

    if !state.slash_completion_rows(1).is_empty() {
        lines.extend(slash_menu_lines(state, width));
    } else {
        let session_id_fits = state.session_id().is_none_or(|session_id| {
            display_width("session ") + display_width(session_id) <= usize::from(width)
        });
        let footer = footer_render_lines(state, registry, width, session_id_fits);
        if !footer.is_empty() {
            lines.push(RenderLine::plain(String::new(), Style::default()));
            lines.extend(footer);
        }
    }

    let cursor_row = composer_row.saturating_add(visual.cursor_row.saturating_sub(composer_start));
    let cursor = Some(Cursor {
        column: visual
            .cursor_column
            .min(usize::from(width.saturating_sub(1))) as u16,
        row: cursor_row.min(usize::from(u16::MAX)) as u16,
        visible: true,
    });
    fit_live(lines, cursor, size, composer_row)
}

/// Render a bounded window of the active reverse-history search directly in
/// the mutable tail. These rows are intentionally kept above the composer so
/// `InlineTerminal` can redraw them without adding transient search content to
/// native terminal scrollback.
fn history_search_lines(state: &AppState, width: u16, max_rows: usize) -> Vec<RenderLine> {
    let Some(results) = state.history_search_results() else {
        return Vec::new();
    };
    let theme = Theme::default();
    if results.matches.is_empty() {
        return vec![RenderLine::plain(
            "History search · no matching session messages · Esc cancel",
            theme.style(Role::Muted),
        )];
    }

    let selected = results.selected.min(results.matches.len() - 1);
    let visible = max_rows.min(results.matches.len());
    let start = selected
        .saturating_sub(visible / 2)
        .min(results.matches.len().saturating_sub(visible));
    let mut lines = vec![RenderLine::plain(
        format!(
            "History search · {}/{} · ↑↓ select · Enter use · Esc cancel",
            selected + 1,
            results.matches.len(),
        ),
        theme.style(Role::Muted),
    )];
    lines.extend(
        results
            .matches
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(index, message)| {
                history_search_excerpt(message, &results.query, index == selected, width, theme)
            }),
    );
    lines
}

/// Collapse a submitted message into one highlighted, context-preserving row.
/// The source message itself remains untouched in durable storage; this is a
/// display-only excerpt for the current terminal frame.
fn history_search_excerpt(
    message: &str,
    query: &str,
    selected: bool,
    width: u16,
    theme: Theme,
) -> RenderLine {
    let message = message.replace(['\r', '\n'], " ");
    let query = query.replace(['\r', '\n'], " ");
    let match_byte = (!query.is_empty()).then(|| message.find(&query)).flatten();
    let match_start = match_byte
        .map(|index| message[..index].chars().count())
        .unwrap_or(0);
    let match_end = match_start + query.chars().count();
    let characters = message.chars().collect::<Vec<_>>();
    let content_width = usize::from(width.saturating_sub(3)).max(1);
    let (start, end) = excerpt_range(characters.len(), match_start, content_width);
    let leading_ellipsis = start != 0;
    let trailing_ellipsis = end != characters.len();

    let mut text = if selected {
        String::from("› ")
    } else {
        String::from("  ")
    };
    let marker_style = if selected {
        theme.style(Role::Accent)
    } else {
        theme.style(Role::Muted)
    };
    let body_style = theme.style(Role::Text);
    let match_style = theme.style(Role::Accent);
    let mut styles = vec![marker_style; 2];
    if leading_ellipsis {
        text.push('…');
        styles.push(theme.style(Role::Muted));
    }
    for (index, character) in characters[start..end].iter().enumerate() {
        let source_index = start + index;
        text.push(*character);
        styles.push(
            if match_byte.is_some() && (match_start..match_end).contains(&source_index) {
                match_style
            } else {
                body_style
            },
        );
    }
    if trailing_ellipsis {
        text.push('…');
        styles.push(theme.style(Role::Muted));
    }
    RenderLine::styled(text, body_style, styles)
}

/// Keep a query match in view while retaining enough leading context to make a
/// history row recognizable. Character boundaries make the result UTF-8 safe;
/// the normal terminal wrapper still handles wide display cells afterward.
fn excerpt_range(length: usize, match_start: usize, limit: usize) -> (usize, usize) {
    if length <= limit {
        return (0, length);
    }
    let start = match_start.saturating_sub(limit / 3).min(length - limit);
    (start, start + limit)
}

/// Project an explicit full-screen surface for the alternate screen.
pub(crate) fn surface_presentation(
    state: &AppState,
    registry: &ProviderRegistry,
    size: Size,
) -> SurfacePresentation {
    let width = size.width;
    let height = size.height;
    let theme = Theme::default();
    let mut rows = vec![RenderLine::plain(String::new(), Style::default()); usize::from(height)];
    let payload = state.surface_lines().map(<[String]>::to_vec);
    let lines: Vec<String> = match state.surface() {
        UiSurface::Help => payload.clone().unwrap_or_else(|| {
            vec![
                "General".into(),
                "  /help  show keybindings and commands".into(),
            ]
        }),
        UiSurface::ToolDetail => payload.unwrap_or_else(|| vec!["No transcript yet.".into()]),
        UiSurface::ModelPicker
        | UiSurface::CustomModel
        | UiSurface::ThinkingPicker
        | UiSurface::SessionPicker => state
            .picker_lines_visible(registry, usize::MAX)
            .unwrap_or_default(),
        UiSurface::None => Vec::new(),
    };
    if height != 0 {
        rows[0] = RenderLine::plain("┃ ", theme.style(Role::Text));
    }
    if height > 1 {
        rows[1] = RenderLine::plain("─".repeat(usize::from(width)), theme.style(Role::Muted));
    }
    let content_limit = height.saturating_sub(2);
    let mut y = 2_u16;
    let surface_start = state.surface_offset().min(lines.len());
    'payload: for line in lines.into_iter().skip(surface_start) {
        for wrapped in wrap_lines(&line, width, theme.style(Role::Text)) {
            if y >= content_limit {
                break 'payload;
            }
            rows[usize::from(y)] = wrapped;
            y = y.saturating_add(1);
        }
    }
    if height > 2 {
        let divider = height - 2;
        rows[usize::from(divider)] =
            RenderLine::plain("─".repeat(usize::from(width)), theme.style(Role::Muted));
        let hint = match state.surface() {
            UiSurface::Help => "↑↓ Navigate · Enter Open · Esc Close",
            UiSurface::ToolDetail => "↑↓ Scroll · Ctrl+O Close · Esc Close",
            UiSurface::ModelPicker
            | UiSurface::CustomModel
            | UiSurface::ThinkingPicker
            | UiSurface::SessionPicker => "↑↓ Navigate · Enter Select · Esc Close",
            UiSurface::None => "Esc Close",
        };
        rows[usize::from(height - 1)] = RenderLine::plain(hint, theme.style(Role::Muted));
    }
    SurfacePresentation {
        lines: rows.into_iter().map(|line| styled(&line)).collect(),
        cursor: (width > 2 && height != 0).then_some(Cursor {
            column: 2,
            row: 0,
            visible: true,
        }),
    }
}

fn fit_live(
    lines: Vec<RenderLine>,
    cursor: Option<Cursor>,
    size: Size,
    composer_row: usize,
) -> MainPresentation {
    let maximum = usize::from(size.height);
    if maximum == 0 {
        return MainPresentation {
            commit: Vec::new(),
            live: Vec::new(),
            cursor: None,
        };
    }
    // Drop old mutable transcript rows first. The composer must remain present
    // even on a tiny terminal; status rows can be clipped after it when there
    // is no possible layout that retains every footer line.
    let start = lines.len().saturating_sub(maximum).min(composer_row);
    let end = start.saturating_add(maximum).min(lines.len());
    let cursor = cursor.map(|cursor| Cursor {
        row: cursor
            .row
            .saturating_sub(start.min(usize::from(u16::MAX)) as u16)
            .min(size.height.saturating_sub(1)),
        ..cursor
    });
    MainPresentation {
        commit: Vec::new(),
        live: lines
            .into_iter()
            .skip(start)
            .take(end.saturating_sub(start))
            .map(|line| styled(&line))
            .collect(),
        cursor,
    }
}

fn styled(line: &RenderLine) -> StyledLine {
    let styles = line
        .character_styles
        .as_ref()
        .cloned()
        .unwrap_or_else(|| vec![line.style; line.text.chars().count()]);
    let mut output = StyledLine::default();
    for (character, style) in line.text.chars().zip(styles) {
        output.push(character.to_string(), style);
    }
    output
}

fn footer_lines(state: &AppState, registry: &ProviderRegistry) -> [String; 2] {
    let mut lines = state.footer_lines(registry);
    // Active work is represented by the transient activity row. Keeping the fixed footer
    // stable avoids leaving a stale `Asking` label behind while tools stream.
    lines[0] = lines[0].replace("⏺ Asking · ", "");
    lines
}

fn footer_render_lines(
    state: &AppState,
    registry: &ProviderRegistry,
    width: u16,
    show_session_id: bool,
) -> Vec<RenderLine> {
    let [primary, secondary] = footer_lines(state, registry);
    let mut lines = wrap_footer_primary(&primary, width);
    if let Some((notice, is_error)) = state.footer_notice() {
        let style = if is_error {
            Theme::default().style(Role::Error)
        } else {
            Theme::default().style(Role::Muted)
        };
        lines.extend(wrap_lines(notice, width, style));
    }
    let secondary = if show_session_id {
        secondary
    } else {
        secondary.split('\n').next().unwrap_or_default().to_owned()
    };
    lines.extend(wrap_lines(
        &secondary,
        width,
        Theme::default().style(Role::Muted),
    ));
    lines
}

fn wrap_footer_primary(text: &str, width: u16) -> Vec<RenderLine> {
    let muted = Theme::default().style(Role::Muted);
    let model = Theme::default().style(Role::Model);
    let model_range = text
        .find("⏺ Asking · ")
        .and_then(|prefix| {
            let start = prefix + "⏺ Asking · ".len();
            text[start..]
                .find(" · effort")
                .map(|length| (start, start + length))
        })
        .or_else(|| text.find(" · effort").map(|end| (0, end)));
    let mut source_offset = 0;
    wrap_raw_text(text, width)
        .into_iter()
        .map(|line| {
            let mut styles = Vec::with_capacity(line.chars().count());
            for character in line.chars() {
                let found = text[source_offset..]
                    .find(character)
                    .map(|offset| source_offset + offset)
                    .unwrap_or(source_offset);
                let style = if model_range.is_some_and(|(start, end)| (start..end).contains(&found))
                {
                    model
                } else {
                    muted
                };
                styles.push(style);
                source_offset = found.saturating_add(character.len_utf8());
            }
            RenderLine::styled(line, muted, styles)
        })
        .collect()
}

/// Wrap one activity row after reserving its visible prefix. The first prefix
/// is always at least as wide as the continuation prefix, so wrapping to its
/// remaining budget keeps every physical continuation within `width` too.
fn prefixed_activity_lines(
    text: &str,
    first_prefix: &str,
    first_style: Style,
    continuation_prefix: &str,
    continuation_style: Style,
    width: u16,
) -> Vec<RenderLine> {
    if width == 0 {
        return Vec::new();
    }
    let first_prefix = truncate_display(first_prefix, usize::from(width));
    let continuation_prefix = truncate_display(continuation_prefix, usize::from(width));
    let payload_width = width.saturating_sub(
        u16::try_from(display_width(&first_prefix)).unwrap_or(u16::MAX),
    );
    let source = RenderLine::plain(text, first_style);
    let chunks = wrap_styled_line(&source, payload_width, true);
    if chunks.is_empty() {
        return vec![RenderLine::plain(first_prefix, first_style)];
    }
    chunks
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            if index == 0 {
                RenderLine::plain(format!("{first_prefix}{}", chunk.text), first_style)
            } else {
                RenderLine::plain(
                    format!("{continuation_prefix}{}", chunk.text),
                    continuation_style,
                )
            }
        })
        .collect()
}

fn activity_lines(state: &AppState, width: u16) -> Vec<RenderLine> {
    let mut lines = Vec::new();
    if matches!(state.status(), crate::app::UiStatus::Active) {
        let theme = Theme::default();
        match state.activity_text() {
            // Unchanged default presentation when no tool has published one.
            None => lines.extend(prefixed_activity_lines(
                "",
                &format!("• Thinking {}", thinking_spinner()),
                theme.style(Role::Activity),
                "  ",
                theme.style(Role::Muted),
                width,
            )),
            Some(activity) => {
                let mut rows = activity.lines();
                lines.extend(prefixed_activity_lines(
                    rows.next().unwrap_or_default(),
                    &format!("• {} ", thinking_spinner()),
                    theme.style(Role::Activity),
                    "  ",
                    theme.style(Role::Muted),
                    width,
                ));
                // Logical continuation rows are subordinate but otherwise
                // plain: no border, panel, or alternate layout. Each is
                // reflowed independently so its indentation survives.
                for row in rows {
                    lines.extend(prefixed_activity_lines(
                        row,
                        "  ",
                        theme.style(Role::Muted),
                        "  ",
                        theme.style(Role::Muted),
                        width,
                    ));
                }
            }
        }
    }
    if let Some(message) = state.queued_message() {
        lines.extend(prefixed_activity_lines(
            &message.replace('\n', " "),
            "• Queued next: ",
            Theme::default().style(Role::Activity),
            "  ",
            Theme::default().style(Role::Muted),
            width,
        ));
    }
    lines
}

fn thinking_spinner() -> &'static str {
    const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
    // The mutable terminal tail is intentionally scrollback-native. Keep an
    // active indicator animated without making slow terminal peers spend all
    // of their time replaying redraws instead of observing user-visible state.
    const FRAME_INTERVAL: Duration = Duration::from_millis(500);
    static STARTED: OnceLock<Instant> = OnceLock::new();
    let frame =
        STARTED.get_or_init(Instant::now).elapsed().as_millis() / FRAME_INTERVAL.as_millis();
    FRAMES[frame as usize % FRAMES.len()]
}

fn slash_menu_lines(state: &AppState, width: u16) -> Vec<RenderLine> {
    let rows = state.slash_completion_rows(6);
    if rows.is_empty() {
        return Vec::new();
    }
    let theme = Theme::default();
    let mut lines = vec![RenderLine::plain(
        "─".repeat(usize::from(width)),
        theme.style(Role::Muted),
    )];
    lines.extend(rows.into_iter().map(|(command, help, selected)| {
        RenderLine::plain(
            format!("  {command:<14} {help}"),
            if selected {
                theme.style(Role::Accent)
            } else {
                theme.style(Role::Text)
            },
        )
    }));
    lines.push(RenderLine::plain(
        "─".repeat(usize::from(width)),
        theme.style(Role::Muted),
    ));
    lines.push(RenderLine::plain(
        "↑↓ Navigate · Enter Use · Esc Close",
        theme.style(Role::Muted),
    ));
    lines
}

/// Return the number of visual rows occupied by the composer.
pub fn composer_height(state: &AppState, width: u16) -> u16 {
    composer_layout(state, width).rows.len().max(1) as u16
}

fn composer_layout(state: &AppState, width: u16) -> VisualLayout {
    VisualLayout::measure(state.composer().text(), state.composer().cursor(), width)
}

fn composer_view_start(layout: &VisualLayout, visible_rows: u16) -> usize {
    if visible_rows == 0 || layout.rows.len() <= usize::from(visible_rows) {
        return 0;
    }
    layout
        .cursor_row
        .saturating_sub(usize::from(visible_rows).saturating_sub(1))
        .min(layout.rows.len().saturating_sub(usize::from(visible_rows)))
}

fn entry_lines_for_entry(entry: &TranscriptEntry, width: u16) -> Vec<RenderLine> {
    match entry {
        TranscriptEntry::Welcome { text } => {
            wrap_lines(text, width, Theme::default().style(Role::Muted))
        }
        TranscriptEntry::User { text } => rail_lines(text, width),
        TranscriptEntry::Assistant { text, streaming } => markdown_lines(text, width, !streaming),
        TranscriptEntry::Tool(tool) => tool_projection_lines(tool, width),
        TranscriptEntry::Error { text } => wrap_lines(
            strip_prefix(text, "assistant error: "),
            width,
            Theme::default().style(Role::Error),
        ),
        TranscriptEntry::Notice { text, severity } => wrap_lines(
            text,
            width,
            match severity {
                NoticeSeverity::Info => Theme::default().style(Role::Muted),
                NoticeSeverity::Warning => Theme::default().style(Role::Activity),
            },
        ),
    }
}

fn tool_projection_lines(tool: &ToolProjection, width: u16) -> Vec<RenderLine> {
    let payload = match tool.state {
        ToolState::Started => Some(tool.arguments.as_str()),
        ToolState::Progress => tool
            .latest_progress
            .as_deref()
            .or(Some(tool.arguments.as_str())),
        ToolState::Completed | ToolState::Failed => tool
            .settled_result
            .as_deref()
            .or(tool.latest_progress.as_deref()),
    }
    .unwrap_or_default();
    tool_lines(&tool.tool_name, tool.state, payload, width)
}

fn rail_lines(text: &str, width: u16) -> Vec<RenderLine> {
    let budget = width.saturating_sub(2);
    wrap_raw_text(text, budget)
        .into_iter()
        .map(|line| RenderLine::plain(format!("┃ {line}"), Theme::default().style(Role::Text)))
        .collect()
}

fn tool_lines(name: &str, state: ToolState, payload: &str, width: u16) -> Vec<RenderLine> {
    let marker = match state {
        ToolState::Started => '⏺',
        ToolState::Progress => '…',
        ToolState::Completed => '✓',
        ToolState::Failed => '✗',
    };
    let detail = payload.lines().next().unwrap_or_default().trim();
    let label = if detail.is_empty() {
        format!("{marker} {name}")
    } else {
        format!("{marker} {name}: {}", compact_tool_detail(detail))
    };
    let mut style = match state {
        ToolState::Failed => Theme::default().style(Role::Error),
        ToolState::Completed => Theme::default().style(Role::Success),
        ToolState::Started | ToolState::Progress => Theme::default().style(Role::Muted),
    };
    style.bold = state == ToolState::Failed;
    let mut output = wrap_lines(&label, width, style);
    if !matches!(state, ToolState::Started) && payload.lines().count() > 1 {
        output.push(RenderLine::plain(
            "  └ … (Ctrl+O to view)",
            Theme::default().style(Role::Muted),
        ));
    }
    output
}

fn compact_tool_detail(detail: &str) -> String {
    truncate_display(detail.trim(), 72)
}

fn markdown_lines(text: &str, width: u16, style_diffs: bool) -> Vec<RenderLine> {
    let mut output = Vec::new();
    let raw_lines: Vec<&str> = text.split('\n').collect();
    let mut index = 0;
    let mut markdown = Highlighter::new(Language::Markdown);
    let mut markdown_scratch = Vec::new();
    let mut code_highlighter = None;
    let mut code_scratch = Vec::new();
    let mut code_is_diff = false;
    let mut code_is_complete = false;
    let mut code_line_number = 0;
    let mut code_line_number_width = 1;
    let mut in_code = false;
    while index < raw_lines.len() {
        let raw = raw_lines[index];
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            if in_code {
                in_code = false;
                code_highlighter = None;
                code_is_diff = false;
                code_is_complete = false;
                code_line_number = 0;
                code_line_number_width = 1;
                code_scratch.clear();
                markdown.reset();
            } else {
                let _ = markdown.highlight_into(raw.as_bytes(), &mut markdown_scratch);
                markdown.reset();
                let info = trimmed.trim_start_matches('`').trim();
                let language_name = info
                    .split(|character: char| character.is_ascii_whitespace() || character == ',')
                    .find(|name| !name.is_empty())
                    .unwrap_or_default();
                code_is_diff = matches!(
                    language_name.to_ascii_lowercase().as_str(),
                    "diff" | "patch" | "udiff"
                );
                code_is_complete = !code_is_diff
                    || style_diffs
                    || raw_lines[index + 1..]
                        .iter()
                        .any(|line| line.trim_start().starts_with("```"));
                code_highlighter = Language::from_name(language_name).map(Highlighter::new);
                code_line_number = 0;
                code_line_number_width = raw_lines[index + 1..]
                    .iter()
                    .take_while(|line| !line.trim_start().starts_with("```"))
                    .count()
                    .max(1)
                    .to_string()
                    .len();
                in_code = true;
                // The info string selects syntax highlighting but is not rendered: the
                // numbered gutter supplies code structure without extra fence rows.
            }
            index += 1;
            continue;
        }
        if in_code {
            code_line_number += 1;
            if code_is_diff {
                if code_is_complete {
                    output.extend(diff_code_lines(
                        raw,
                        width,
                        code_line_number,
                        code_line_number_width,
                    ));
                } else {
                    output.extend(code_lines(
                        &RenderLine::plain(raw, Theme::default().style(Role::Muted)),
                        width,
                        code_line_number,
                        code_line_number_width,
                    ));
                }
            } else if let Some(highlighter) = code_highlighter.as_mut() {
                let highlighted = highlighted_line(
                    raw,
                    highlighter,
                    &mut code_scratch,
                    Theme::default().style(Role::Muted),
                );
                output.extend(code_lines(
                    &highlighted,
                    width,
                    code_line_number,
                    code_line_number_width,
                ));
            } else {
                output.extend(code_lines(
                    &RenderLine::plain(raw, Theme::default().style(Role::Muted)),
                    width,
                    code_line_number,
                    code_line_number_width,
                ));
            }
            index += 1;
            continue;
        }

        let highlighted = highlighted_line(
            raw,
            &mut markdown,
            &mut markdown_scratch,
            Theme::default().style(Role::Plain),
        );
        if is_table_header(
            raw_lines.get(index).copied(),
            raw_lines.get(index + 1).copied(),
        ) {
            let start = index;
            index += 2;
            while index < raw_lines.len() && raw_lines[index].contains('|') {
                index += 1;
            }
            output.extend(render_table(&raw_lines[start..index], width));
            continue;
        }
        if let Some(heading) = trimmed.strip_prefix('#') {
            output.extend(wrap_lines(
                heading.trim_start_matches('#').trim_start(),
                width,
                {
                    let mut style = Theme::default().style(Role::Text);
                    style.bold = true;
                    style
                },
            ));
        } else if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            output.extend(wrap_lines(
                &format!("• {item}"),
                width,
                Theme::default().style(Role::Plain),
            ));
        } else if let Some((marker, item)) = ordered_list_item(trimmed) {
            output.extend(wrap_lines(
                &format!("{marker} {item}"),
                width,
                Theme::default().style(Role::Plain),
            ));
        } else if let Some(quote) = trimmed.strip_prefix('>') {
            output.extend(wrap_lines(
                &format!("│ {}", quote.trim_start()),
                width,
                Theme::default().style(Role::Muted),
            ));
        } else {
            output.extend(wrap_styled_line(&highlighted, width, false));
        }
        index += 1;
    }
    output
}

fn ordered_list_item(line: &str) -> Option<(&str, &str)> {
    let boundary = line.find(". ")?;
    let (number, item) = line.split_at(boundary);
    if !number.is_empty() && number.chars().all(|character| character.is_ascii_digit()) {
        Some((number, item.trim_start_matches(". ")))
    } else {
        None
    }
}

fn highlighted_line(
    text: &str,
    highlighter: &mut Highlighter,
    scratch: &mut Vec<Kind>,
    base: Style,
) -> RenderLine {
    let kinds = highlighter.highlight_into(text.as_bytes(), scratch);
    let styles = text
        .char_indices()
        .map(|(index, _)| style_for_kind(kinds.get(index).copied().unwrap_or_default(), base))
        .collect();
    RenderLine::styled(text, base, styles)
}

fn style_for_kind(kind: Kind, base: Style) -> Style {
    let role = match kind {
        Kind::Normal => return base,
        Kind::Keyword => Role::CodeKeyword,
        Kind::Type => Role::CodeType,
        Kind::String => Role::CodeString,
        Kind::Comment => Role::CodeComment,
        Kind::Number => Role::CodeNumber,
        Kind::Bracket => Role::CodeBracket,
        Kind::Operator => Role::CodeOperator,
        Kind::Function => Role::CodeFunction,
        Kind::Constant => Role::CodeConstant,
        Kind::Macro => Role::CodeMacro,
    };
    let mut style = Theme::default().style(role);
    style.bold = base.bold;
    style
}

fn wrap_styled_line(line: &RenderLine, width: u16, preserve_indentation: bool) -> Vec<RenderLine> {
    if width == 0 {
        return Vec::new();
    }
    let styles = line
        .character_styles
        .as_ref()
        .cloned()
        .unwrap_or_else(|| vec![line.style; line.text.chars().count()]);
    let mut characters = line.text.chars().zip(styles).collect::<Vec<_>>();
    if !preserve_indentation {
        let trim = characters
            .iter()
            .take_while(|(character, _)| character.is_whitespace())
            .count();
        characters.drain(..trim);
    }
    if characters.is_empty() {
        return vec![RenderLine::plain(String::new(), line.style)];
    }

    let mut output = Vec::new();
    let mut start = 0;
    while start < characters.len() {
        let mut used = 0;
        let mut end = start;
        let mut last_space = None;
        while end < characters.len() {
            let symbol = characters[end].0;
            let symbol_width = char_width(symbol);
            if used + symbol_width > usize::from(width) {
                break;
            }
            used += symbol_width;
            if symbol.is_whitespace() {
                last_space = Some(end);
            }
            end += 1;
        }
        if end == start {
            end += 1;
        }
        let cut = if end < characters.len() {
            last_space.filter(|space| *space >= start).unwrap_or(end)
        } else {
            end
        };
        let (chunk, next_start) =
            if cut > start && cut < characters.len() && characters[cut].0.is_whitespace() {
                (&characters[start..cut], cut + 1)
            } else {
                (&characters[start..cut], cut)
            };
        let text = chunk
            .iter()
            .map(|(character, _)| *character)
            .collect::<String>();
        let styles = chunk.iter().map(|(_, style)| *style).collect();
        output.push(RenderLine::styled(text, line.style, styles));
        start = next_start.max(start + 1);
    }
    output
}

fn code_lines(
    line: &RenderLine,
    width: u16,
    line_number: usize,
    line_number_width: usize,
) -> Vec<RenderLine> {
    let prefix_width = line_number_width.saturating_add(3);
    let available = width.saturating_sub(u16::try_from(prefix_width).unwrap_or(u16::MAX));
    let chunks = wrap_styled_line(line, available, true);
    if chunks.is_empty() {
        return vec![prepend_code_gutter(
            RenderLine::plain(String::new(), line.style),
            line_number,
            line_number_width,
            true,
        )];
    }
    chunks
        .into_iter()
        .enumerate()
        .map(|(index, line)| prepend_code_gutter(line, line_number, line_number_width, index == 0))
        .collect()
}

fn prepend_code_gutter(
    line: RenderLine,
    line_number: usize,
    line_number_width: usize,
    first_row: bool,
) -> RenderLine {
    let rail_style = Theme::default().style(Role::Muted);
    let gutter = if first_row {
        format!("{line_number:>line_number_width$} │ ")
    } else {
        format!("{:>line_number_width$} │ ", "")
    };
    let mut text = gutter;
    text.push_str(&line.text);
    let mut styles = vec![rail_style; line_number_width + 3];
    styles.extend(
        line.character_styles
            .unwrap_or_else(|| vec![line.style; line.text.chars().count()]),
    );
    RenderLine::styled(text, line.style, styles)
}

fn diff_code_lines(
    raw: &str,
    width: u16,
    line_number: usize,
    line_number_width: usize,
) -> Vec<RenderLine> {
    let style = if raw.starts_with('+') && !raw.starts_with("+++") {
        Theme::default().style(Role::Success)
    } else if raw.starts_with('-') && !raw.starts_with("---") {
        Theme::default().style(Role::Error)
    } else if raw.starts_with("@@") || raw.starts_with("diff ") {
        let mut style = Theme::default().style(Role::Accent);
        style.bold = true;
        style
    } else {
        Theme::default().style(Role::Muted)
    };
    code_lines(
        &RenderLine::plain(raw, style),
        width,
        line_number,
        line_number_width,
    )
}

fn is_table_header(header: Option<&str>, separator: Option<&str>) -> bool {
    let Some(header) = header else { return false };
    let Some(separator) = separator else {
        return false;
    };
    header.contains('|')
        && separator.contains('|')
        && split_table_row(separator).iter().all(|cell| {
            !cell.is_empty()
                && cell
                    .chars()
                    .filter(|character| *character != ':')
                    .all(|character| character == '-')
        })
}

fn split_table_row(row: &str) -> Vec<String> {
    row.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().trim_matches(':').to_owned())
        .collect()
}

fn render_table(rows: &[&str], width: u16) -> Vec<RenderLine> {
    let cells = rows
        .iter()
        .filter(|row| !is_separator_table_row(row))
        .map(|row| split_table_row(row))
        .collect::<Vec<_>>();
    let columns = cells.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return Vec::new();
    }
    let mut widths = (0..columns)
        .map(|column| {
            cells
                .iter()
                .map(|row| row.get(column).map_or(0, |cell| display_width(cell)))
                .max()
                .unwrap_or(1)
                .max(1)
        })
        .collect::<Vec<_>>();
    let max_width = usize::from(width).max(columns + 3);
    let budget = max_width.saturating_sub(columns + 1);
    while widths.iter().sum::<usize>() > budget {
        if let Some((index, _)) = widths.iter().enumerate().max_by_key(|(_, value)| **value) {
            if widths[index] <= 3 {
                break;
            }
            widths[index] -= 1;
        } else {
            break;
        }
    }
    let border = |left: char, middle: char, right: char| {
        format!(
            "{left}{}{right}",
            widths
                .iter()
                .map(|width| "─".repeat(width + 2))
                .collect::<Vec<_>>()
                .join(&middle.to_string())
        )
    };
    let mut output = vec![RenderLine::plain(
        border('┌', '┬', '┐'),
        Theme::default().style(Role::Accent),
    )];
    for (row_index, row) in cells.iter().enumerate() {
        let content = widths
            .iter()
            .enumerate()
            .map(|(column, width)| {
                let value = row.get(column).map_or("", String::as_str);
                format!(" {} ", pad_display(value, *width))
            })
            .collect::<Vec<_>>()
            .join("│");
        output.push(RenderLine::plain(
            format!("│{content}│"),
            Theme::default().style(Role::Plain),
        ));
        if row_index == 0 {
            output.push(RenderLine::plain(
                border('├', '┼', '┤'),
                Theme::default().style(Role::Accent),
            ));
        }
    }
    output.push(RenderLine::plain(
        border('└', '┴', '┘'),
        Theme::default().style(Role::Accent),
    ));
    output
}

fn is_separator_table_row(row: &str) -> bool {
    split_table_row(row)
        .iter()
        .all(|cell| !cell.is_empty() && cell.chars().all(|character| character == '-'))
}

fn wrap_lines(text: &str, width: u16, style: Style) -> Vec<RenderLine> {
    wrap_raw_text(text, width)
        .into_iter()
        .map(|text| RenderLine::plain(text, style))
        .collect()
}

fn wrap_raw_text(text: &str, width: u16) -> Vec<String> {
    wrap_raw_text_inner(text, width)
}

fn wrap_raw_text_inner(text: &str, width: u16) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut output = Vec::new();
    for logical in text.split('\n') {
        if logical.is_empty() {
            output.push(String::new());
            continue;
        }
        let mut remaining = logical.trim_start().to_owned();
        while !remaining.is_empty() {
            let mut used = 0;
            let mut end = 0;
            let mut last_space = None;
            for (index, symbol) in remaining.char_indices() {
                let symbol_width = char_width(symbol);
                if used + symbol_width > usize::from(width) {
                    break;
                }
                used += symbol_width;
                end = index + symbol.len_utf8();
                if symbol.is_whitespace() {
                    last_space = Some(index);
                }
            }
            if end == 0 {
                let symbol = remaining.chars().next().expect("remaining is non-empty");
                end = symbol.len_utf8();
            }
            let cut = if end < remaining.len() {
                last_space.filter(|space| *space > 0).unwrap_or(end)
            } else {
                end
            };
            output.push(remaining[..cut].trim_end().to_owned());
            remaining = remaining[cut..].trim_start().to_owned();
        }
    }
    output
}

fn strip_prefix<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.strip_prefix(prefix).unwrap_or(text)
}

fn truncate_display(text: &str, width: usize) -> String {
    let mut output = String::new();
    let mut used = 0;
    for symbol in text.chars() {
        let symbol_width = char_width(symbol);
        if used + symbol_width > width {
            break;
        }
        output.push(symbol);
        used += symbol_width;
    }
    output
}

fn pad_display(text: &str, width: usize) -> String {
    let value = truncate_display(text, width);
    let padding = width.saturating_sub(display_width(&value));
    format!("{value}{}", " ".repeat(padding))
}

fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

fn char_width(symbol: char) -> usize {
    if symbol == '\u{200d}'
        || matches!(symbol, '\u{fe0e}' | '\u{fe0f}')
        || ('\u{300}'..='\u{36f}').contains(&symbol)
        || ('\u{1ab0}'..='\u{1aff}').contains(&symbol)
        || ('\u{20d0}'..='\u{20ff}').contains(&symbol)
        || ('\u{fe20}'..='\u{fe2f}').contains(&symbol)
    {
        0
    } else if matches!(
        symbol,
        '\u{1100}'..='\u{115f}'
            | '\u{2329}'..='\u{232a}'
            | '\u{2e80}'..='\u{a4cf}'
            | '\u{ac00}'..='\u{d7a3}'
            | '\u{f900}'..='\u{faff}'
            | '\u{fe10}'..='\u{fe19}'
            | '\u{fe30}'..='\u{fe6f}'
            | '\u{ff00}'..='\u{ff60}'
            | '\u{ffe0}'..='\u{ffe6}'
            | '\u{1f300}'..='\u{1faff}'
    ) {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn markdown_table_has_unicode_borders_and_header_rule() {
        let lines = markdown_lines("| Name | Value |\n| --- | --- |\n| foo | bar |", 40, true);
        assert_eq!(lines[0].text, "┌──────┬───────┐");
        assert_eq!(lines[1].text, "│ Name │ Value │");
        assert_eq!(lines[2].text, "├──────┼───────┤");
        assert_eq!(lines[3].text, "│ foo  │ bar   │");
        assert_eq!(lines[4].text, "└──────┴───────┘");
    }

    #[test]
    fn user_entries_render_as_connected_rails() {
        let lines = entry_lines_for_entry(
            &TranscriptEntry::User {
                text: "hello world".into(),
            },
            20,
        );
        assert_eq!(lines[0].text, "┃ hello world");
    }

    #[test]
    fn stable_prefix_stops_at_the_first_mutable_entry() {
        let mut entries = vec![
            TranscriptEntry::User {
                text: "prompt".into(),
            },
            TranscriptEntry::Assistant {
                text: "partial".into(),
                streaming: true,
            },
        ];
        assert_eq!(stable_prefix(&entries), 1);
        let TranscriptEntry::Assistant { streaming, .. } = &mut entries[1] else {
            unreachable!()
        };
        *streaming = false;
        assert_eq!(stable_prefix(&entries), 2);
    }

    #[test]
    fn wide_characters_consume_two_terminal_cells() {
        assert_eq!(display_width("界"), 2);
        assert_eq!(wrap_raw_text("a界b", 3), ["a界", "b"]);
    }

    #[test]
    fn markdown_table_pads_wide_cells_by_display_width() {
        let lines = markdown_lines("| Name | Value |\n| --- | --- |\n| 界 | ok |", 30, true);
        assert_eq!(lines[1].text, "│ Name │ Value │");
        assert_eq!(lines[3].text, "│ 界   │ ok    │");
    }

    #[test]
    fn markdown_inline_tokens_receive_hi_lite_styles() {
        let lines = markdown_lines("**bold** and `inline`", 40, true);
        let styles = lines[0]
            .character_styles
            .as_ref()
            .expect("highlighted markdown line");
        assert_eq!(styles[0].foreground, Some(Color::Blue));
        let inline_start = lines[0].text.find('`').expect("inline code");
        assert_eq!(styles[inline_start].foreground, Some(Color::Green));
    }

    #[test]
    fn markdown_ordered_lists_and_quotes_get_bounded_structure() {
        let lines = markdown_lines("1. first\n2. second\n> quoted", 40, true);
        let text = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(text, ["1 first", "2 second", "│ quoted"]);
    }

    #[test]
    fn fenced_code_uses_the_declared_language_and_renders_a_line_number_gutter() {
        let lines = markdown_lines("```rust\nfn main() { return 1; }\n```", 40, true);
        assert_eq!(lines[0].text, "1 │ fn main() { return 1; }");
        let styles = lines[0]
            .character_styles
            .as_ref()
            .expect("highlighted code line");
        assert_eq!(styles[0].foreground, Some(Color::DarkGrey));
        assert_eq!(styles[4].foreground, Some(Color::Blue));
    }

    #[test]
    fn fenced_code_gutter_aligns_numbers_and_wrapped_continuations() {
        let lines = markdown_lines("```text\na\nb\nlong line that wraps\n```", 18, true);
        let text = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(text, ["1 │ a", "2 │ b", "3 │ long line", "  │ that wraps"]);
    }

    #[test]
    fn fenced_code_gutter_aligns_multi_digit_line_numbers() {
        let body = (1..=10)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = markdown_lines(&format!("```text\n{body}\n```"), 40, true);
        assert!(lines[0].text.starts_with(" 1 │"));
        assert!(lines[9].text.starts_with("10 │"));
    }

    #[test]
    fn transcript_keeps_user_message_above_assistant_code_block() {
        let mut state = AppState::new();
        let user = tea_core::state::AgentMessage::User {
            id: tea_core::state::MessageId(1),
            content: "user message".into(),
        };
        state.apply_event(&tea_core::event::AgentEvent {
            run_id: tea_core::state::RunId(1),
            sequence: tea_core::event::EventSequence(1),
            kind: tea_core::event::AgentEventKind::MessageStart { message: user },
        });
        let assistant_text = "```rust\nfn main() {}\n```";
        let assistant = tea_core::state::AgentMessage::Assistant {
            id: tea_core::state::MessageId(2),
            content: assistant_text.into(),
            tool_calls: Vec::new(),
            stop_reason: Some(tea_core::state::StopReason::Stop),
            error_message: None,
        };
        state.apply_event(&tea_core::event::AgentEvent {
            run_id: tea_core::state::RunId(1),
            sequence: tea_core::event::EventSequence(2),
            kind: tea_core::event::AgentEventKind::MessageUpdate {
                message: assistant.clone(),
                text_delta: Some(assistant_text.into()),
            },
        });
        state.apply_event(&tea_core::event::AgentEvent {
            run_id: tea_core::state::RunId(1),
            sequence: tea_core::event::EventSequence(3),
            kind: tea_core::event::AgentEventKind::MessageEnd { message: assistant },
        });

        let presentation = main_presentation(
            &state,
            &ProviderRegistry::new(),
            Size {
                width: 40,
                height: 12,
            },
            0,
        );
        assert_eq!(presentation.live[0].text(), "┃ user message");
        assert_eq!(presentation.live[2].text(), "1 │ fn main() {}");
    }

    #[test]
    fn unknown_fenced_languages_remain_visible_without_syntax_rules() {
        let lines = markdown_lines("```made-up\ncontent\n```", 40, true);
        assert_eq!(lines[0].text, "1 │ content");
        assert_eq!(
            lines[0]
                .character_styles
                .as_ref()
                .expect("neutral code styles")[4]
                .foreground,
            Some(Color::DarkGrey)
        );
    }

    #[test]
    fn streaming_diffs_stay_neutral_until_the_diff_block_ends() {
        let streaming = markdown_lines("```diff\n+added\n-removed", 40, false);
        let finished = markdown_lines("```diff\n+added\n-removed\n```", 40, false);
        assert_eq!(
            streaming[1]
                .character_styles
                .as_ref()
                .expect("streaming diff styles")[4]
                .foreground,
            Some(Color::DarkGrey)
        );
        assert_eq!(
            finished[0]
                .character_styles
                .as_ref()
                .expect("finished diff styles")[4]
                .foreground,
            Some(Color::Green)
        );
        assert_eq!(
            finished[1]
                .character_styles
                .as_ref()
                .expect("finished diff styles")[4]
                .foreground,
            Some(Color::Red)
        );
    }

    #[test]
    fn tool_cards_preserve_generic_names_and_raw_payloads() {
        let tool = tool_lines(
            "acme.custom",
            ToolState::Started,
            r#"{"command":"cargo test -p tea-agent","timeout":30}"#,
            80,
        );
        assert_eq!(
            tool[0].text,
            "⏺ acme.custom: {\"command\":\"cargo test -p tea-agent\",\"timeout\":30}"
        );
    }

    #[test]
    fn multiline_tool_results_render_a_body_rail() {
        let lines = tool_lines(
            "arbitrary-tool",
            ToolState::Completed,
            "first line\n  second line\nthird line",
            30,
        );
        assert_eq!(lines[0].text, "✓ arbitrary-tool: first line");
        assert_eq!(lines[1].text, "  └ … (Ctrl+O to view)");
        assert_eq!(lines[0].style.foreground, Some(Color::Green));
    }

    #[test]
    fn composer_preserves_indentation_and_scrolls_to_the_cursor() {
        let mut composer = Composer::new();
        composer.replace_from_editor("  first\n    second\n      third");
        let layout = VisualLayout::measure(composer.text(), composer.cursor(), 20);
        assert_eq!(layout.rows[0].text, "❯   first");
        assert_eq!(layout.rows[2].text, "❯       third");
        assert_eq!(composer_view_start(&layout, 2), 1);
    }

    #[test]
    fn startup_composer_follows_the_welcome_transcript() {
        let regions = frame_layout::plan_flow(80, 24, 1, 0, 1, 0, 1);
        assert_eq!(regions.composer.height, 1);
        assert_eq!(regions.composer.y, 2);
        assert_eq!(regions.activity.height, 0);
    }

    #[test]
    fn activity_shows_the_full_next_message_slot() {
        let mut state = AppState::new();
        state.queue_message("first instruction".into());
        state.queue_message("second instruction".into());

        assert_eq!(
            activity_lines(&state, 80)
                .into_iter()
                .map(|line| line.text)
                .collect::<Vec<_>>(),
            ["• Queued next: first instruction  second instruction"]
        );
    }

    fn agent_event(
        sequence: u64,
        kind: tea_core::event::AgentEventKind,
    ) -> tea_core::event::AgentEvent {
        tea_core::event::AgentEvent {
            run_id: tea_core::state::RunId(1),
            sequence: tea_core::event::EventSequence(sequence),
            kind,
        }
    }

    fn publish_activity(state: &mut AppState, sequence: u64, activity: &str) {
        state.apply_event(&agent_event(
            sequence,
            tea_core::event::AgentEventKind::ToolExecutionUpdate {
                tool_call_id: tea_core::state::ToolCallId::new(format!("call-{sequence}"))
                    .expect("fixture call id"),
                tool_name: "todo".into(),
                update: tea_core::tool::ToolUpdate {
                    content: String::new(),
                    details: None,
                    activity: Some(activity.into()),
                },
            },
        ));
    }

    fn active_state() -> AppState {
        let mut state = AppState::new();
        state.apply_event(&agent_event(1, tea_core::event::AgentEventKind::AgentStart));
        state
    }

    /// Replace the animated glyph so presentation assertions never depend on
    /// an instantaneous spinner frame.
    fn mask_spinner(text: &str) -> String {
        let spinner = thinking_spinner();
        text.replacen(&format!("• {spinner} "), "• <spin> ", 1)
            .replacen(&format!("• Thinking {spinner}"), "• Thinking <spin>", 1)
    }

    const TODO_ACTIVITY: &str =
        "Todo · 1 active · 2 pending\n- [>] State machine\n- [ ] Durable integration\n- [ ] Tests";

    #[test]
    fn the_default_activity_row_is_unchanged_without_an_override() {
        let state = active_state();
        let rows = activity_lines(&state, 80)
            .into_iter()
            .map(|line| mask_spinner(&line.text))
            .collect::<Vec<_>>();
        assert_eq!(rows, ["• Thinking <spin>"]);
    }

    #[test]
    fn a_published_activity_replaces_the_default_row_and_keeps_the_spinner() {
        let mut state = active_state();
        publish_activity(&mut state, 2, TODO_ACTIVITY);
        let lines = activity_lines(&state, 80);
        let rows = lines
            .iter()
            .map(|line| mask_spinner(&line.text))
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                "• <spin> Todo · 1 active · 2 pending",
                "  - [>] State machine",
                "  - [ ] Durable integration",
                "  - [ ] Tests",
            ]
        );
        let theme = Theme::default();
        assert_eq!(lines[0].style, theme.style(Role::Activity));
        assert!(lines[1..]
            .iter()
            .all(|line| line.style == theme.style(Role::Muted)));
    }

    #[test]
    fn a_later_activity_atomically_replaces_the_previous_one() {
        let mut state = active_state();
        publish_activity(&mut state, 2, TODO_ACTIVITY);
        publish_activity(&mut state, 3, "Todo · 2 active\n- [>] Only row");
        let rows = activity_lines(&state, 80)
            .into_iter()
            .map(|line| mask_spinner(&line.text))
            .collect::<Vec<_>>();
        assert_eq!(rows, ["• <spin> Todo · 2 active", "  - [>] Only row"]);
    }

    #[test]
    fn activity_rows_stay_in_the_mutable_tail_and_reflow_on_resize() {
        let mut state = AppState::new();
        state.welcome_line();
        state.apply_event(&agent_event(1, tea_core::event::AgentEventKind::AgentStart));
        publish_activity(&mut state, 2, TODO_ACTIVITY);
        for width in [80_u16, 40] {
            let presentation = main_presentation(
                &state,
                &ProviderRegistry::new(),
                Size { width, height: 24 },
                1,
            );
            assert!(
                presentation.commit.is_empty(),
                "activity never joins the stable scrollback prefix"
            );
            assert!(
                presentation
                    .live
                    .iter()
                    .any(|line| line.text().contains("Todo · 1 active · 2 pending")),
                "activity is projected into the mutable tail at width {width}"
            );
        }
    }

    #[test]
    fn activity_rows_wrap_to_the_available_width_and_reflow_geometry() {
        let mut state = active_state();
        let activity = format!(
            "Todo · 1 blocked\n- [!] {} — {}",
            "t".repeat(200),
            "b".repeat(300),
        );
        publish_activity(&mut state, 2, &activity);

        let wide = activity_lines(&state, 80);
        let narrow = activity_lines(&state, 40);
        assert!(
            narrow
                .iter()
                .all(|line| display_width(&line.text) <= 40),
            "every activity physical row fits the terminal width: {narrow:#?}"
        );
        assert!(
            narrow.len() > wide.len(),
            "narrower activity rows occupy more measured layout rows"
        );
    }

    #[test]
    fn a_tiny_terminal_clips_activity_before_losing_the_composer() {
        let mut state = AppState::new();
        state.welcome_line();
        state.apply_event(&agent_event(1, tea_core::event::AgentEventKind::AgentStart));
        publish_activity(&mut state, 2, TODO_ACTIVITY);
        let presentation = main_presentation(
            &state,
            &ProviderRegistry::new(),
            Size {
                width: 40,
                height: 2,
            },
            1,
        );
        assert!(presentation
            .live
            .iter()
            .any(|line| line.text().starts_with("┃")));
        assert!(!presentation
            .live
            .iter()
            .any(|line| line.text().contains("State machine")));
    }

    #[test]
    fn live_startup_frame_uses_the_minimal_transcript_flow_rail() {
        let mut state = AppState::new();
        state.welcome_line();
        let presentation = main_presentation(
            &state,
            &ProviderRegistry::new(),
            Size {
                width: 80,
                height: 24,
            },
            1,
        );
        assert_eq!(presentation.live[0].text(), "");
        assert_eq!(presentation.live[1].text(), "┃ ");
        assert_eq!(
            presentation.cursor,
            Some(Cursor {
                column: 2,
                row: 1,
                visible: true
            })
        );
    }

    #[test]
    fn footer_model_identity_uses_yellow_for_the_provider_model_segment() {
        let state = AppState::new();
        let registry = ProviderRegistry::new();
        let presentation = main_presentation(
            &state,
            &registry,
            Size {
                width: 80,
                height: 8,
            },
            0,
        );
        let footer = state.footer_lines(&registry);
        let primary = &footer[0];
        let model_start = primary
            .find("⏺ Asking · ")
            .map(|prefix| prefix + "⏺ Asking · ".len())
            .unwrap_or(0);
        assert_eq!(
            style_at(&presentation.live[2], model_start).foreground,
            Some(Color::Yellow)
        );
    }

    #[test]
    fn footer_renders_the_calm_session_stats_line() {
        let state = AppState::new();
        let registry = ProviderRegistry::new();
        let presentation = main_presentation(
            &state,
            &registry,
            Size {
                width: 80,
                height: 8,
            },
            0,
        );

        assert_eq!(
            presentation.live[3].text(),
            state.footer_lines(&registry)[1]
        );
    }

    #[test]
    fn footer_renders_session_identity_on_the_line_after_context_stats() {
        let mut state = AppState::new();
        state.set_session_id(Some("0123456789abcdef".into()));
        let presentation = main_presentation(
            &state,
            &ProviderRegistry::new(),
            Size {
                width: 80,
                height: 8,
            },
            0,
        );

        assert_eq!(presentation.live[3].text(), "ctx ?%/?");
        assert_eq!(presentation.live[4].text(), "session 0123456789abcdef");
    }

    #[test]
    fn inline_slash_menu_uses_the_captured_minimal_geometry() {
        let mut state = AppState::new();
        state.welcome_line();
        state.composer_mut().insert('/').expect("insert slash");
        state.update_slash_completion(vec!["/help".into(), "/models".into(), "/new".into()]);
        let presentation = main_presentation(
            &state,
            &ProviderRegistry::new(),
            Size {
                width: 80,
                height: 24,
            },
            1,
        );
        assert_eq!(presentation.live[1].text().chars().next(), Some('┃'));
        assert_eq!(presentation.live[2].text().chars().next(), Some('─'));
        assert!(presentation.live[3].text().starts_with("  /he"));
        assert!(presentation
            .live
            .last()
            .is_some_and(|line| line.text().starts_with("↑↓ Navigate")));
        assert_eq!(
            presentation.cursor,
            Some(Cursor {
                column: 3,
                row: 1,
                visible: true
            })
        );
    }

    #[test]
    fn tiny_frames_keep_cursor_targets_in_bounds_and_mark_hidden_composer_rows() {
        let mut state = AppState::new();
        state.welcome_line();
        state
            .composer_mut()
            .replace_from_editor("one\ntwo\nthree\nfour\nfive");
        let presentation = main_presentation(
            &state,
            &ProviderRegistry::new(),
            Size {
                width: 20,
                height: 5,
            },
            1,
        );
        assert!(presentation.live[0].text().starts_with("┃↑"));
        for (width, height) in [(0, 0), (1, 1), (2, 2)] {
            let presentation =
                main_presentation(&state, &ProviderRegistry::new(), Size { width, height }, 1);
            assert!(presentation.live.len() <= usize::from(height));
            if let Some(cursor) = presentation.cursor {
                assert!(cursor.column < width && cursor.row < height);
            }
        }
    }

    fn style_at(line: &StyledLine, index: usize) -> Style {
        let mut consumed = 0;
        for span in line.spans() {
            let count = span.text.chars().count();
            if index < consumed + count {
                return span.style;
            }
            consumed += count;
        }
        panic!("style index {index} is outside line {:?}", line.text());
    }
}
