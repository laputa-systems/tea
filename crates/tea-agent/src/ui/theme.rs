//! The small, non-persisted palette used by the tea terminal projection.

use crate::grid::{Color, Style};

/// Named presentation roles. Keeping roles separate from terminal colors makes snapshots
/// readable and prevents renderer code from accidentally inventing a second palette.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    /// Unemphasized body text.
    Plain,
    Accent,
    Muted,
    /// Provider/model identity in the persistent footer.
    Model,
    Text,
    Error,
    Success,
    Activity,
    CodeKeyword,
    CodeType,
    CodeString,
    CodeComment,
    CodeNumber,
    CodeBracket,
    CodeOperator,
    CodeFunction,
    CodeConstant,
    CodeMacro,
}

/// Runtime-only palette choice. Tea intentionally does not persist this setting.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

/// The default dark palette modelled after fx's contrast hierarchy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    pub accent: Color,
    pub muted: Color,
    pub text: Color,
    pub error: Color,
    pub success: Color,
    pub activity: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: Color::Cyan,
            muted: Color::DarkGrey,
            text: Color::White,
            error: Color::Red,
            success: Color::Green,
            activity: Color::Yellow,
        }
    }
}

impl Theme {
    /// Return the light counterpart without introducing persisted configuration.
    pub const fn light() -> Self {
        Self {
            accent: Color::Blue,
            muted: Color::DarkGrey,
            text: Color::Black,
            error: Color::DarkRed,
            success: Color::DarkGreen,
            activity: Color::DarkYellow,
        }
    }

    /// Select one of the built-in palettes for this process.
    pub fn for_mode(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self::default(),
            ThemeMode::Light => Self::light(),
        }
    }

    /// Return the cell style for a semantic role.
    pub const fn style(self, role: Role) -> Style {
        let foreground = match role {
            Role::Plain => self.text,
            Role::Accent => self.accent,
            Role::Muted => self.muted,
            Role::Model => Color::Yellow,
            Role::Text => self.text,
            Role::Error => self.error,
            Role::Success => self.success,
            Role::Activity => self.activity,
            Role::CodeKeyword | Role::CodeFunction => Color::Blue,
            Role::CodeType => Color::Cyan,
            Role::CodeString => Color::Green,
            Role::CodeComment => Color::DarkGrey,
            Role::CodeNumber | Role::CodeConstant => Color::Yellow,
            Role::CodeBracket => Color::White,
            Role::CodeOperator | Role::CodeMacro => Color::Magenta,
        };
        Style {
            foreground: Some(foreground),
            background: None,
            bold: matches!(role, Role::Accent | Role::Text | Role::Error),
        }
    }
}
