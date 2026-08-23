//! Presentation projection from [`crate::app::AppState`] to the local cell grid.
//!
//! The renderer owns presentation semantics above the core event boundary.
//! Core events remain lossless; this layer decides how a user, assistant,
//! tool, notice, or Markdown table occupies terminal rows.

use crate::app::{AppState, NoticeSeverity, ToolProjection, ToolState, TranscriptEntry, UiSurface};
#[cfg(test)]
use crate::composer::Composer;
#[cfg(test)]
use crate::grid::Color;
use crate::grid::{Cell, Grid, Style};
use crate::ui::frame_layout;
use crate::ui::theme::{Role, Theme};
use crate::ui::visual_layout::VisualLayout;
use hi_lite::{Highlighter, Kind, Language};
use tea_core::provider::ProviderRegistry;

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

fn frame_for(
    state: &AppState,
    registry: &ProviderRegistry,
    width: u16,
    height: u16,
) -> FrameLayout {
    let desired_composer_rows =
        VisualLayout::measure(state.composer().text(), state.composer().cursor(), width)
            .rows
            .len()
            .max(1);
    // The composer grows with its content but keeps a bounded viewport once a
    // conversation exists. That leaves room for the transcript/status and
    // makes the hidden-above rail an observable affordance on short terminals.
    let composer_capacity = if state.transcript().is_empty() {
        usize::from(height)
    } else {
        usize::from(height.saturating_sub(3).max(1))
    };
    let composer_rows = desired_composer_rows.min(composer_capacity.max(1));
    let transcript_rows = wrapped_transcript(state, width).len();
    let menu_rows = slash_menu_lines(state, width).len();
    // Measure exactly the rows that `activity_lines` will paint so the fixed
    // footer never overlaps live status output.
    let activity_rows = activity_lines(state).len();
    let footer_rows = footer_render_line_count(state, registry, width);
    frame_layout::plan_flow(
        width,
        height,
        transcript_rows,
        activity_rows,
        composer_rows,
        menu_rows,
        if menu_rows == 0 { footer_rows } else { 0 },
    )
}

/// Render the current presentation state into a fresh frame.
pub fn render(state: &AppState, registry: &ProviderRegistry, width: u16, height: u16) -> Grid {
    if !matches!(state.surface(), UiSurface::None) {
        return render_surface(state, registry, width, height);
    }
    let mut grid = Grid::new(width, height);
    let theme = Theme::default();
    let regions = frame_for(state, registry, width, height);
    let transcript = wrapped_transcript(state, regions.transcript.width);
    let visible_rows = regions.transcript.height as usize;
    let start = if state.follows_output() {
        transcript.len().saturating_sub(visible_rows)
    } else {
        state.viewport_offset().min(transcript.len())
    };
    for (row, line) in transcript.iter().skip(start).enumerate() {
        if row >= visible_rows {
            break;
        }
        put_line(
            &mut grid,
            regions.transcript.x,
            regions.transcript.y + row as u16,
            regions.transcript.width,
            line,
        );
    }

    let activity = activity_lines(state);
    for (row, line) in activity.into_iter().enumerate() {
        if row >= regions.activity.height as usize {
            break;
        }
        put_line(
            &mut grid,
            regions.activity.x,
            regions.activity.y + row as u16,
            regions.activity.width,
            &line,
        );
    }

    let visual = composer_layout(state, regions.composer.width);
    let composer_start = composer_view_start(&visual, regions.composer.height);
    for (row, line) in visual.rows.into_iter().skip(composer_start).enumerate() {
        if row >= regions.composer.height as usize {
            break;
        }
        let text = line.text.strip_prefix("❯ ").unwrap_or(&line.text);
        let prefix = if composer_start != 0 && row == 0 {
            "┃↑"
        } else {
            "┃ "
        };
        put_text(
            &mut grid,
            regions.composer.x,
            regions.composer.y + row as u16,
            regions.composer.width,
            &format!("{prefix}{text}"),
            theme.style(Role::Text),
        );
    }

    if regions.hint.height != 0 {
        for (row, status) in footer_render_lines(state, registry, regions.hint.width)
            .into_iter()
            .enumerate()
        {
            if row >= regions.hint.height as usize {
                break;
            }
            put_line(
                &mut grid,
                regions.hint.x,
                regions.hint.y + row as u16,
                regions.hint.width,
                &status,
            );
        }
    }

    if regions.menu.height != 0 {
        let completion_rows = slash_menu_lines(state, regions.menu.width);
        for (row, line) in completion_rows.into_iter().enumerate() {
            if row >= regions.menu.height as usize {
                break;
            }
            put_line(
                &mut grid,
                regions.menu.x,
                regions.menu.y + row as u16,
                regions.menu.width,
                &line,
            );
        }
    }
    grid
}

fn footer_lines(state: &AppState, registry: &ProviderRegistry) -> [String; 2] {
    let mut lines = state.footer_lines(registry);
    // Active work is represented by the transient activity row. Keeping the fixed footer
    // stable avoids leaving a stale `Asking` label behind while tools stream.
    lines[0] = lines[0].replace("⏺ Asking · ", "");
    lines
}

fn footer_render_line_count(state: &AppState, registry: &ProviderRegistry, width: u16) -> usize {
    let [primary, secondary] = footer_lines(state, registry);
    wrap_raw_text(&primary, width).len()
        + state
        .footer_notice()
        .map(|(notice, _)| wrap_raw_text(notice, width).len())
        .unwrap_or(0)
        + wrap_raw_text(&secondary, width).len()
}

fn footer_render_lines(
    state: &AppState,
    registry: &ProviderRegistry,
    width: u16,
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
        .or_else(|| {
            text.find(" · effort")
                .map(|end| (0, end))
        });
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
                let style = model_range
                    .is_some_and(|(start, end)| (start..end).contains(&found))
                    .then_some(model)
                    .unwrap_or(muted);
                styles.push(style);
                source_offset = found.saturating_add(character.len_utf8());
            }
            RenderLine::styled(line, muted, styles)
        })
        .collect()
}

fn render_surface(state: &AppState, registry: &ProviderRegistry, width: u16, height: u16) -> Grid {
    let mut grid = Grid::new(width, height);
    let theme = Theme::default();
    let payload = state.surface_lines().map(<[String]>::to_vec);
    let lines: Vec<String> = match state.surface() {
        UiSurface::Help => payload.clone().unwrap_or_else(|| {
            vec![
                "Commands".into(),
                String::new(),
                "General".into(),
                "  /help  show keybindings and commands".into(),
            ]
        }),
        UiSurface::ToolDetail => payload.unwrap_or_else(|| vec!["No transcript yet.".into()]),
        UiSurface::ModelPicker | UiSurface::CustomModel | UiSurface::SessionPicker => state
            .picker_lines_visible(registry, usize::MAX)
            .unwrap_or_default(),
        // Keep this branch forward-compatible with a future full-transcript surface. A
        // temporary surface still owns the whole frame even when its content is not yet
        // specialized here.
        _ => Vec::new(),
    };
    if height == 0 {
        return grid;
    }
    put_text(&mut grid, 0, 0, width, "┃ ", theme.style(Role::Text));
    if height > 1 {
        put_text(
            &mut grid,
            0,
            1,
            width,
            &"─".repeat(usize::from(width)),
            theme.style(Role::Muted),
        );
    }
    let content_limit = height.saturating_sub(2);
    let mut y = 2_u16;
    let surface_start = state.surface_offset().min(lines.len());
    for line in lines.into_iter().skip(surface_start) {
        for wrapped in wrap_lines(&line, width, theme.style(Role::Text)) {
            if y >= content_limit {
                break;
            }
            put_line(&mut grid, 0, y, width, &wrapped);
            y = y.saturating_add(1);
        }
        if y >= content_limit {
            break;
        }
    }
    if height > 2 {
        let divider = height - 2;
        put_text(
            &mut grid,
            0,
            divider,
            width,
            &"─".repeat(usize::from(width)),
            theme.style(Role::Muted),
        );
        let hint = match state.surface() {
            UiSurface::Help => "↑↓ Navigate · Enter Open · Esc Close",
            UiSurface::ToolDetail => "↑↓ Scroll · Ctrl+O Close · Esc Close",
            UiSurface::ModelPicker | UiSurface::CustomModel | UiSurface::SessionPicker => {
                "↑↓ Navigate · Enter Select · Esc Close"
            }
            _ => "Esc Close",
        };
        put_text(
            &mut grid,
            0,
            height - 1,
            width,
            hint,
            theme.style(Role::Muted),
        );
    }
    grid
}

fn activity_lines(state: &AppState) -> Vec<RenderLine> {
    let mut lines = Vec::new();
    if matches!(state.status(), crate::app::UiStatus::Active) {
        lines.push(RenderLine::plain(
            "• Thinking",
            Theme::default().style(Role::Activity),
        ));
    }
    lines
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
    lines.push(RenderLine::plain(
        format!(
            "Results {} · Type to filter",
            state.slash_completion_count()
        ),
        theme.style(Role::Text),
    ));
    lines.push(RenderLine::plain(String::new(), theme.style(Role::Text)));
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

/// Return the count and available row count used by scrolling calculations.
pub fn transcript_metrics(state: &AppState, width: u16, height: u16) -> (usize, usize) {
    let registry = ProviderRegistry::new();
    let regions = frame_for(state, &registry, width, height);
    (
        wrapped_transcript(state, regions.transcript.width).len(),
        regions.transcript.height as usize,
    )
}

/// Return the number of visual rows occupied by the composer.
pub fn composer_height(state: &AppState, width: u16) -> u16 {
    composer_layout(state, width).rows.len().max(1) as u16
}

/// Return the native cursor location for the visible composer.
pub fn composer_cursor_position(state: &AppState, width: u16, height: u16) -> Option<(u16, u16)> {
    if width == 0 || height == 0 {
        return None;
    }
    if !matches!(state.surface(), UiSurface::None) {
        return (width > 2).then_some((2, 0));
    }
    let registry = ProviderRegistry::new();
    let regions = frame_for(state, &registry, width, height);
    if regions.composer.height == 0 {
        return None;
    }
    let visual = composer_layout(state, width);
    let composer_start = composer_view_start(&visual, regions.composer.height);
    let row = visual.cursor_row.saturating_sub(composer_start);
    Some((
        visual
            .cursor_column
            .min(usize::from(width.saturating_sub(1))) as u16,
        regions.composer.y + (row as u16).min(regions.composer.height.saturating_sub(1)),
    ))
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

fn put_text(grid: &mut Grid, x: u16, y: u16, width: u16, text: &str, style: Style) {
    let mut column = 0_u16;
    for symbol in text.chars() {
        if symbol == '\r' {
            continue;
        }
        if symbol == '\n' {
            break;
        }
        let symbol_width = char_width(symbol);
        if symbol_width == 0 {
            continue;
        }
        let symbol_width = symbol_width as u16;
        if column.saturating_add(symbol_width) > width {
            break;
        }
        let _ = grid.set(x.saturating_add(column), y, Cell { symbol, style });
        if symbol_width == 2 && column + 1 < width {
            let _ = grid.set(x.saturating_add(column + 1), y, Cell { symbol: ' ', style });
        }
        column = column.saturating_add(symbol_width);
    }
}

fn put_line(grid: &mut Grid, x: u16, y: u16, width: u16, line: &RenderLine) {
    let mut column = 0_u16;
    for (index, symbol) in line.text.chars().enumerate() {
        if symbol == '\r' {
            continue;
        }
        if symbol == '\n' {
            break;
        }
        let symbol_width = char_width(symbol);
        if symbol_width == 0 {
            continue;
        }
        let symbol_width = symbol_width as u16;
        if column.saturating_add(symbol_width) > width {
            break;
        }
        let style = line
            .character_styles
            .as_ref()
            .and_then(|styles| styles.get(index).copied())
            .unwrap_or(line.style);
        let _ = grid.set(x.saturating_add(column), y, Cell { symbol, style });
        if symbol_width == 2 && column + 1 < width {
            let _ = grid.set(x.saturating_add(column + 1), y, Cell { symbol: ' ', style });
        }
        column = column.saturating_add(symbol_width);
    }
}

fn wrapped_transcript(state: &AppState, width: u16) -> Vec<RenderLine> {
    let mut output = Vec::new();
    for (index, entry) in state.transcript_entries().into_iter().enumerate() {
        if index != 0 {
            output.push(RenderLine::plain(String::new(), Style::default()));
        }
        output.extend(entry_lines_for_entry(&entry, width));
    }
    output
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
                code_scratch.clear();
                markdown.reset();
                output.push(RenderLine::plain("└", {
                    let mut style = Theme::default().style(Role::Muted);
                    style.bold = true;
                    style
                }));
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
                in_code = true;
                let label = if info.is_empty() { "code" } else { info };
                output.push(RenderLine::plain(format!("┌ {label}"), {
                    let mut style = Theme::default().style(Role::Muted);
                    style.bold = true;
                    style
                }));
            }
            index += 1;
            continue;
        }
        if in_code {
            if code_is_diff {
                if code_is_complete {
                    output.extend(diff_code_lines(raw, width));
                } else {
                    output.extend(code_lines(
                        &RenderLine::plain(raw, Theme::default().style(Role::Muted)),
                        width,
                    ));
                }
            } else if let Some(highlighter) = code_highlighter.as_mut() {
                let highlighted = highlighted_line(
                    raw,
                    highlighter,
                    &mut code_scratch,
                    Theme::default().style(Role::Muted),
                );
                output.extend(code_lines(&highlighted, width));
            } else {
                output.extend(code_lines(
                    &RenderLine::plain(raw, Theme::default().style(Role::Muted)),
                    width,
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

fn code_lines(line: &RenderLine, width: u16) -> Vec<RenderLine> {
    let available = width.saturating_sub(2);
    let chunks = wrap_styled_line(line, available, true);
    if chunks.is_empty() {
        return vec![RenderLine::plain("│ ", Theme::default().style(Role::Muted))];
    }
    chunks.into_iter().map(prepend_code_rail).collect()
}

fn prepend_code_rail(line: RenderLine) -> RenderLine {
    let rail_style = Theme::default().style(Role::Muted);
    let mut text = String::from("│ ");
    text.push_str(&line.text);
    let mut styles = vec![rail_style; 2];
    styles.extend(
        line.character_styles
            .unwrap_or_else(|| vec![line.style; line.text.chars().count()]),
    );
    RenderLine::styled(text, line.style, styles)
}

fn diff_code_lines(raw: &str, width: u16) -> Vec<RenderLine> {
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
    code_lines(&RenderLine::plain(raw, style), width)
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
    fn fenced_code_uses_the_declared_language_and_preserves_the_rail() {
        let lines = markdown_lines("```rust\nfn main() { return 1; }\n```", 40, true);
        assert_eq!(lines[0].text, "┌ rust");
        assert_eq!(lines[1].text, "│ fn main() { return 1; }");
        let styles = lines[1]
            .character_styles
            .as_ref()
            .expect("highlighted code line");
        assert_eq!(styles[0].foreground, Some(Color::DarkGrey));
        assert_eq!(styles[2].foreground, Some(Color::Blue));
        assert_eq!(lines[2].text, "└");
    }

    #[test]
    fn unknown_fenced_languages_remain_visible_without_syntax_rules() {
        let lines = markdown_lines("```made-up\ncontent\n```", 40, true);
        assert_eq!(lines[1].text, "│ content");
        assert_eq!(
            lines[1]
                .character_styles
                .as_ref()
                .expect("neutral code styles")[2]
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
                .expect("streaming diff styles")[2]
                .foreground,
            Some(Color::DarkGrey)
        );
        assert_eq!(
            finished[1]
                .character_styles
                .as_ref()
                .expect("finished diff styles")[2]
                .foreground,
            Some(Color::Green)
        );
        assert_eq!(
            finished[2]
                .character_styles
                .as_ref()
                .expect("finished diff styles")[2]
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
    fn live_startup_frame_uses_the_minimal_transcript_flow_rail() {
        let mut state = AppState::new();
        state.welcome_line();
        let grid = render(&state, &ProviderRegistry::new(), 80, 24);
        let registry = ProviderRegistry::new();
        let regions = frame_for(&state, &registry, 80, 24);
        assert_eq!(
            grid.get(regions.composer.x, regions.composer.y)
                .expect("composer cell")
                .symbol,
            '┃'
        );
        assert_eq!(regions.composer.y, 2);
        assert_ne!(
            grid.get(0, regions.composer.y)
                .expect("composer cell")
                .symbol,
            '❯'
        );
    }

    #[test]
    fn footer_model_identity_uses_yellow_for_the_provider_model_segment() {
        let state = AppState::new();
        let registry = ProviderRegistry::new();
        let grid = render(&state, &registry, 80, 8);
        let regions = frame_for(&state, &registry, 80, 8);
        let footer = state.footer_lines(&registry);
        let primary = &footer[0];
        let model_start = primary
            .find("⏺ Asking · ")
            .map(|prefix| prefix + "⏺ Asking · ".len())
            .unwrap_or(0);
        assert_eq!(
            grid.get(regions.hint.x + model_start as u16, regions.hint.y)
                .expect("model footer cell")
                .style
                .foreground,
            Some(Color::Yellow)
        );
    }

    #[test]
    fn footer_renders_the_calm_session_stats_line() {
        let state = AppState::new();
        let registry = ProviderRegistry::new();
        let grid = render(&state, &registry, 80, 8);
        let regions = frame_for(&state, &registry, 80, 8);
        let row = (0..regions.hint.width)
            .filter_map(|column| grid.get(regions.hint.x + column, regions.hint.y + 1))
            .map(|cell| cell.symbol)
            .collect::<String>()
            .trim_end()
            .to_owned();

        assert_eq!(row, state.footer_lines(&registry)[1]);
    }

    #[test]
    fn inline_slash_menu_uses_the_captured_minimal_geometry() {
        let mut state = AppState::new();
        state.welcome_line();
        state.composer_mut().insert('/').expect("insert slash");
        state.update_slash_completion(vec![
            "/help".into(),
            "/model".into(),
            "/thinking".into(),
            "/session".into(),
            "/new".into(),
            "/quit".into(),
        ]);
        let grid = render(&state, &ProviderRegistry::new(), 80, 24);
        assert_eq!(grid.get(0, 2).expect("composer rail").symbol, '┃');
        assert_eq!(grid.get(0, 3).expect("menu divider").symbol, '─');
        assert_eq!(
            (0..9)
                .filter_map(|column| grid.get(column, 4))
                .map(|cell| cell.symbol)
                .collect::<String>(),
            "Results 6"
        );
        assert_eq!(grid.get(0, 13).expect("menu navigation hint").symbol, '↑');
        assert_eq!(composer_cursor_position(&state, 80, 24), Some((3, 2)));
    }

    #[test]
    fn tiny_frames_keep_cursor_targets_in_bounds_and_mark_hidden_composer_rows() {
        let mut state = AppState::new();
        state.welcome_line();
        state
            .composer_mut()
            .replace_from_editor("one\ntwo\nthree\nfour\nfive");
        let grid = render(&state, &ProviderRegistry::new(), 20, 5);
        let regions = frame_for(&state, &ProviderRegistry::new(), 20, 5);
        assert_eq!(
            grid.get(0, regions.composer.y)
                .expect("visible composer rail")
                .symbol,
            '┃'
        );
        assert_eq!(
            grid.get(1, regions.composer.y)
                .expect("hidden composer marker")
                .symbol,
            '↑'
        );
        for (width, height) in [(0, 0), (1, 1), (2, 2)] {
            let grid = render(&state, &ProviderRegistry::new(), width, height);
            assert_eq!((grid.width(), grid.height()), (width, height));
            if let Some((x, y)) = composer_cursor_position(&state, width, height) {
                assert!(
                    x < width && y < height,
                    "cursor must address a drawable cell"
                );
            }
        }
    }
}
