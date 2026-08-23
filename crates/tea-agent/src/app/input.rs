use crate::editor::Editor;
use crate::terminal::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, TerminalEvent, TerminalGuard,
};

use super::commands;
use super::error::AppError;
use super::runtime::App;
use super::state::UiSurface;
use super::support::{parse_thinking_level, thinking_level_name};

impl App {
    pub(super) fn handle_terminal_event(
        &mut self,
        terminal: &mut TerminalGuard,
        event: TerminalEvent,
    ) -> Result<(), AppError> {
        match event {
            TerminalEvent::Key(key) if key.kind != KeyEventKind::Release => {
                self.handle_key(terminal, key)
            }
            TerminalEvent::Paste(text)
                if self.state.picker.is_none()
                    && matches!(self.state.surface(), UiSurface::None) =>
            {
                self.state.composer_mut().insert_str_multiline(&text);
                self.refresh_command_completion();
                Ok(())
            }
            TerminalEvent::Paste(text) => self.picker_insert(&text),
            TerminalEvent::Resize(_, _)
            | TerminalEvent::FocusGained
            | TerminalEvent::FocusLost
            | TerminalEvent::Mouse => Ok(()),
            _ => Ok(()),
        }
    }

    fn handle_key(&mut self, terminal: &mut TerminalGuard, key: KeyEvent) -> Result<(), AppError> {
        if self.state.picker.is_some() {
            return self.handle_picker_key(key);
        }
        if self.state.surface() == UiSurface::ToolDetail
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('o')
        {
            self.state.toggle_tool_detail();
            return Ok(());
        }
        if self.state.surface() == UiSurface::ToolDetail {
            let page = usize::from(terminal.size()?.1.saturating_sub(3)).max(1);
            match key.code {
                KeyCode::PageUp | KeyCode::Up => self.state.page_surface_up(page),
                KeyCode::PageDown | KeyCode::Down => self.state.page_surface_down(page),
                _ => {}
            }
            if matches!(
                key.code,
                KeyCode::PageUp | KeyCode::Up | KeyCode::PageDown | KeyCode::Down
            ) {
                return Ok(());
            }
        }
        if !matches!(self.state.surface(), UiSurface::None) && key.code == KeyCode::Esc {
            self.state.close_surface();
            return Ok(());
        }
        if !matches!(self.state.surface(), UiSurface::None) {
            return Ok(());
        }
        if self.state.slash_completion.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.state.slash_completion = None;
                    return Ok(());
                }
                KeyCode::Up => {
                    self.state.move_slash_completion(-1);
                    return Ok(());
                }
                KeyCode::Down => {
                    self.state.move_slash_completion(1);
                    return Ok(());
                }
                KeyCode::Tab => {
                    self.complete_command();
                    return Ok(());
                }
                KeyCode::Enter => {
                    if let Some(command) = self.state.selected_slash_completion().map(str::to_owned)
                    {
                        self.state
                            .composer_mut()
                            .replace_from_editor(format!("{command} "));
                        self.state.slash_completion = None;
                        return self.submit_composer();
                    }
                }
                _ => {}
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('g') {
            self.state.slash_completion = None;
            let current = self.state.composer().text().to_owned();
            match Editor::open(terminal, &current) {
                Ok(replacement) => {
                    self.state.composer_mut().replace_from_editor(replacement);
                    self.previous_grid = None;
                }
                Err(error) => self.state.notice(error.to_string()),
            }
            return Ok(());
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.handle_control_c();
            return Ok(());
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.state.toggle_tool_detail();
            self.previous_grid = None;
            return Ok(());
        }
        match key.code {
            KeyCode::Tab => self.complete_command(),
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                if let Err(error) = self.state.composer_mut().insert(character) {
                    self.state.notice(error.to_string());
                }
            }
            KeyCode::Backspace => self.state.composer_mut().backspace(),
            KeyCode::Delete => self.state.composer_mut().delete(),
            KeyCode::Left => self.state.composer_mut().move_left(),
            KeyCode::Right => self.state.composer_mut().move_right(),
            KeyCode::Home => self.state.composer_mut().home(),
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.follow_end()
            }
            KeyCode::End => self.state.composer_mut().end(),
            KeyCode::Up => {
                if self.state.restore_queued_message() {
                    self.state.notice("queued message restored");
                } else {
                    let width = terminal.size()?.0;
                    if !self.state.composer_mut().move_visual_line_up(width) {
                        self.state.begin_history_navigation();
                        if let Some(history) = self.state.history_previous() {
                            self.state.composer_mut().replace_from_editor(history);
                        }
                    }
                }
            }
            KeyCode::Down => {
                let width = terminal.size()?.0;
                if !self.state.composer_mut().move_visual_line_down(width) {
                    if let Some(history) = self.state.history_next() {
                        self.state.composer_mut().replace_from_editor(history);
                    }
                }
            }
            KeyCode::PageUp => self.state.page_up(5),
            KeyCode::PageDown => self.state.page_down(5),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.state.composer_mut().insert_newline()
            }
            KeyCode::Enter => self.submit_composer()?,
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.state.composer_mut().move_word_left()
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.state.composer_mut().move_word_right()
            }
            _ => {}
        }
        self.refresh_command_completion();
        Ok(())
    }

    fn handle_control_c(&mut self) {
        let active_harness = self
            .durable_harness
            .as_ref()
            .filter(|harness| harness.is_active())
            .cloned();
        if let Some(harness) = active_harness {
            match harness.abort() {
                Ok(true) => self.state.notice("cancelling"),
                Ok(false) => self.state.notice("waiting for durable epoch startup"),
                Err(error) => self.state.notice(error.to_string()),
            }
            self.state.slash_completion = None;
            return;
        }
        if self.state.composer().text().is_empty() {
            self.quitting = true;
        } else {
            self.state.composer_mut().clear();
        }
        self.state.slash_completion = None;
    }

    fn submit_composer(&mut self) -> Result<(), AppError> {
        let input = self.state.composer_mut().take();
        if input.trim().is_empty() {
            return Ok(());
        }
        self.state.record_history(&input);
        if input.starts_with('/') {
            self.dispatch_command(&input)
        } else {
            if self.agent_is_active() {
                self.state.queue_message(input);
                self.state.notice("next message queued");
                return Ok(());
            }
            let input = if self.state.queued_message().is_some() {
                self.state.queue_message(input);
                self.state
                    .take_queued_message()
                    .expect("a queued message was just stored")
            } else {
                input
            };
            if self.configured_provider.is_none() {
                self.state.notice("select a model first");
                self.open_model_picker();
            } else {
                match self.ensure_durable_harness() {
                    Ok(harness) => {
                        self.submitted_prompt = Some(input.clone());
                        self.spawn_durable_prompt(harness, input);
                    }
                    Err(error) => {
                        self.state.composer_mut().replace_from_editor(input);
                        self.state.notice(error.to_string());
                    }
                }
            }
            Ok(())
        }
    }

    pub(super) fn complete_command(&mut self) {
        let input = self.state.composer().text().to_owned();
        let Some(prefix) = input.split_whitespace().next() else {
            return;
        };
        if !prefix.starts_with('/') || input.chars().any(char::is_whitespace) {
            return;
        }
        let matches = commands::matching(prefix);
        let Some(command) = self
            .state
            .selected_slash_completion()
            .or_else(|| matches.first().map(|command| command.name))
            .map(str::to_owned)
        else {
            return;
        };
        self.state
            .composer_mut()
            .replace_from_editor(format!("{command} "));
        self.state.slash_completion = None;
    }

    fn refresh_command_completion(&mut self) {
        let input = self.state.composer().text();
        let Some(prefix) = input.split_whitespace().next() else {
            self.state.slash_completion = None;
            return;
        };
        if !prefix.starts_with('/') || input.chars().any(char::is_whitespace) {
            self.state.slash_completion = None;
            return;
        }
        self.state.update_slash_completion(
            commands::matching(prefix)
                .into_iter()
                .map(|command| command.name.to_owned())
                .collect(),
        );
    }

    pub(super) fn dispatch_command(&mut self, input: &str) -> Result<(), AppError> {
        self.state.slash_completion = None;
        let mut words = input.split_whitespace();
        let command = words.next().unwrap_or_default();
        if self.agent_is_active()
            && commands::find(command).is_some_and(|spec| !spec.allowed_while_active)
        {
            self.state
                .notice(format!("{command} is unavailable while a run is active"));
            return Ok(());
        }
        match command {
            "/help" => {
                self.state
                    .set_surface_lines(UiSurface::Help, help_surface_lines());
            }
            "/model" => {
                if let (Some(provider), Some(model)) = (words.next(), words.next()) {
                    self.select_model(provider.to_owned(), model.to_owned())?;
                } else {
                    self.open_model_picker();
                }
            }
            "/thinking" => self.dispatch_thinking(words.next(), words.next())?,
            "/session" | "/resume" => {
                if let Err(error) = self.open_session_picker() {
                    self.state.notice(error.to_string());
                }
            }
            "/new" => {
                if let Err(error) = self.new_session() {
                    self.state.notice(error.to_string());
                }
            }
            "/quit" => {
                self.quitting = true;
                let active_harness = self
                    .durable_harness
                    .as_ref()
                    .filter(|harness| harness.is_active())
                    .cloned();
                if let Some(harness) = active_harness {
                    match harness.abort() {
                        Ok(true) => self.state.notice("cancelling before exit"),
                        Ok(false) => self.state.notice("waiting for durable epoch startup"),
                        Err(error) => self.state.notice(error.to_string()),
                    }
                }
            }
            command => self.state.notice(format!("unknown command {command}")),
        }
        Ok(())
    }

    fn dispatch_thinking(
        &mut self,
        value: Option<&str>,
        extra: Option<&str>,
    ) -> Result<(), AppError> {
        let Some(value) = value else {
            self.state.notice(format!(
                "usage: /thinking <off|minimal|low|medium|high|xhigh|max> (current {})",
                thinking_level_name(self.state.thinking_level())
            ));
            return Ok(());
        };
        if extra.is_some() {
            self.state.notice("usage: /thinking <level>");
            return Ok(());
        }
        let Some(level) = parse_thinking_level(value) else {
            self.state.notice(format!(
                "unknown thinking level {value}; expected off, minimal, low, medium, high, xhigh, or max"
            ));
            return Ok(());
        };
        self.set_thinking_level(level)?;
        self.state
            .notice(format!("reasoning effort set to {}", thinking_level_name(level)));
        Ok(())
    }
}

fn help_surface_lines() -> Vec<String> {
    const GROUPS: &[(&str, &[&str])] = &[
        ("General", &["/help", "/quit"]),
        ("Session", &["/new", "/session", "/resume"]),
        ("Runtime", &["/model", "/thinking"]),
    ];

    let mut lines = Vec::new();
    for (index, (heading, names)) in GROUPS.iter().enumerate() {
        if index != 0 {
            lines.push(String::new());
        }
        lines.push((*heading).into());
        for name in *names {
            let spec = commands::find(name).expect("help groups use registered commands");
            lines.push(format!("  {:<20} {}", spec.name, spec.help));
        }
    }
    lines
}
