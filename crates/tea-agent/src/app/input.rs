use crate::editor::Editor;
use crate::terminal::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, TerminalEvent, TerminalGuard,
};

use super::commands;
use super::error::AppError;
use super::runtime::App;
use super::state::UiSurface;

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
                if self.state.history_search_is_active() {
                    self.state.reset_history_search_selection();
                } else {
                    self.refresh_command_completion();
                }
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
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            self.state.begin_or_advance_history_search();
            return Ok(());
        }
        if self.state.history_search_is_active() {
            return self.handle_history_search_key(key);
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
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {}
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
            KeyCode::PageUp | KeyCode::PageDown => {}
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

    /// Handle the small, inline history-search mode before normal composer
    /// shortcuts. It keeps matching entirely local to the session-derived
    /// history cache and only replaces the draft after an explicit Enter.
    fn handle_history_search_key(&mut self, key: KeyEvent) -> Result<(), AppError> {
        match key.code {
            KeyCode::Esc => {
                self.state.cancel_history_search();
                self.refresh_command_completion();
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.cancel_history_search();
                self.refresh_command_completion();
            }
            KeyCode::Up => self.state.move_history_search(-1),
            KeyCode::Down => self.state.move_history_search(1),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.state.composer_mut().insert_newline();
                self.state.reset_history_search_selection();
            }
            KeyCode::Enter => {
                let has_match = self
                    .state
                    .history_search_results()
                    .is_some_and(|results| !results.matches.is_empty());
                if has_match {
                    self.state.accept_history_search();
                    self.refresh_command_completion();
                } else {
                    self.state.notice("no matching session messages");
                }
            }
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                if let Err(error) = self.state.composer_mut().insert(character) {
                    self.state.notice(error.to_string());
                } else {
                    self.state.reset_history_search_selection();
                }
            }
            KeyCode::Backspace => {
                self.state.composer_mut().backspace();
                self.state.reset_history_search_selection();
            }
            KeyCode::Delete => {
                self.state.composer_mut().delete();
                self.state.reset_history_search_selection();
            }
            KeyCode::Left => self.state.composer_mut().move_left(),
            KeyCode::Right => self.state.composer_mut().move_right(),
            KeyCode::Home => self.state.composer_mut().home(),
            KeyCode::End => self.state.composer_mut().end(),
            _ => {}
        }
        Ok(())
    }

    fn handle_control_c(&mut self) {
        if self.durable_task.is_some() || self.agent_is_active() {
            self.request_root_abort(true);
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

    pub(super) fn submit_composer(&mut self) -> Result<(), AppError> {
        let input = self.state.composer_mut().take();
        if input.trim().is_empty() {
            return Ok(());
        }
        if input.starts_with('/') {
            self.dispatch_command(&input)
        } else {
            // A saved model can remain visible when its provider cannot be configured (for
            // example, because a required API key is missing). Keep a fresh submission untouched
            // in that error state so Enter does not discard the draft or open an unrelated picker;
            // slash commands, including `/models`, remain the explicit recovery path.
            if self.configured_provider.is_none() && self.state.selected_model.is_some() {
                self.state.composer_mut().replace_from_editor(input);
                return Ok(());
            }
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
        let matches = self.matching_commands(prefix);
        let Some(command) = self
            .state
            .selected_slash_completion()
            .or_else(|| matches.first().map(String::as_str))
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
        self.state
            .update_slash_completion(self.matching_commands(prefix));
    }

    fn matching_commands(&self, prefix: &str) -> Vec<String> {
        commands::matching(prefix)
            .into_iter()
            .map(|command| command.name.to_owned())
            .chain(self.state.matching_extension_commands(prefix))
            .collect()
    }

    pub(super) fn dispatch_command(&mut self, input: &str) -> Result<(), AppError> {
        self.state.slash_completion = None;
        let mut words = input.split_whitespace();
        let command = words.next().unwrap_or_default().to_owned();
        let arguments = input
            .get(command.len()..)
            .unwrap_or_default()
            .trim()
            .to_owned();
        let allowed_while_active = commands::find(&command)
            .map(|spec| spec.allowed_while_active)
            .or_else(|| {
                self.state
                    .extension_command(&command)
                    .map(|spec| spec.allowed_while_active)
            });
        if command != "/new"
            && self.agent_is_active()
            && allowed_while_active.is_some_and(|allowed| !allowed)
        {
            self.state
                .notice(format!("{command} is unavailable while a run is active"));
            return Ok(());
        }
        match command.as_str() {
            "/help" => {
                self.state.set_surface_lines(
                    UiSurface::Help,
                    help_surface_lines(&self.state.extension_commands),
                );
            }
            "/models" => {
                if let (Some(provider), Some(model)) = (words.next(), words.next()) {
                    self.select_model(provider.to_owned(), model.to_owned())?;
                    self.open_thinking_picker();
                } else {
                    self.open_model_picker();
                }
            }
            "/resume" => {
                if let Err(error) = self.open_session_picker() {
                    self.state.notice(error.to_string());
                }
            }
            "/new" => {
                if let Err(error) = self.new_session() {
                    self.state.notice(error.to_string());
                }
            }
            command if self.state.extension_command(command).is_some() => {
                self.dispatch_extension_command(command, arguments)?;
            }
            command => self.state.notice(format!("unknown command {command}")),
        }
        Ok(())
    }

    /// Ask the durable supervisor to cancel the root operation without using
    /// `is_active` as a gate: the durable receiver exists one scheduling turn
    /// before a newly accepted epoch installs its core agent.
    pub(super) fn request_root_abort(&mut self, report: bool) -> bool {
        let Some(harness) = self.durable_harness.as_ref() else {
            return false;
        };
        match harness.abort_root() {
            Ok(true) => {
                if report {
                    self.state.notice(if self.quitting {
                        "cancelling before exit"
                    } else {
                        "cancelling"
                    });
                }
                true
            }
            Ok(false) => {
                if report && self.durable_task.is_some() {
                    self.state.notice("waiting for durable epoch startup");
                }
                false
            }
            Err(error) => {
                if report {
                    self.state.notice(error.to_string());
                }
                false
            }
        }
    }

    fn dispatch_extension_command(
        &mut self,
        command: &str,
        arguments: String,
    ) -> Result<(), AppError> {
        if self.configured_provider.is_none() && self.durable_harness.is_none() {
            self.state.notice("select a model first");
            self.open_model_picker();
            return Ok(());
        }
        let harness = match self.ensure_durable_harness() {
            Ok(harness) => harness,
            Err(error) => {
                self.state.notice(error.to_string());
                return Ok(());
            }
        };
        if self.agent_is_active() {
            self.queued_extension_commands
                .push((command.to_owned(), arguments));
            self.state
                .notice(format!("{command} queued until the active run settles"));
            return Ok(());
        }
        match harness.dispatch_extension_command(command, arguments) {
            Ok(dispatch) => {
                if let Some(notice) = dispatch.result.notice {
                    self.state.extension_notice(notice);
                }
                if let Some(input) = dispatch.result.internal_input {
                    self.spawn_extension_continuation(harness, dispatch.extension_id, input);
                }
            }
            Err(error) => self.state.notice(error.to_string()),
        }
        Ok(())
    }
}

fn help_surface_lines(
    extensions: &[tea_core::harness::extension::ExtensionHostCommandDescription],
) -> Vec<String> {
    const GROUPS: &[(&str, &[&str])] = &[
        ("General", &["/help"]),
        ("Session", &["/new", "/resume"]),
        ("Runtime", &["/models"]),
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
    if !extensions.is_empty() {
        lines.push(String::new());
        lines.push("Extensions".into());
        for command in extensions {
            lines.push(format!("  {:<20} {}", command.name, command.help));
        }
    }
    lines.push(String::new());
    lines.push("Input".into());
    lines.push("  Ctrl+R               search messages in this session".into());
    lines
}
