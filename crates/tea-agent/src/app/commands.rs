//! Canonical tea command specifications.

/// A direct command exposed by the terminal host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CommandSpec {
    pub(crate) name: &'static str,
    pub(crate) help: &'static str,
    pub(crate) allowed_while_active: bool,
}

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/help",
        help: "show keybindings and commands",
        allowed_while_active: true,
    },
    CommandSpec {
        name: "/model",
        help: "select a provider/model",
        allowed_while_active: false,
    },
    CommandSpec {
        name: "/cost",
        help: "show provider-reported usage and cost",
        allowed_while_active: true,
    },
    CommandSpec {
        name: "/session",
        help: "pick a durable session",
        allowed_while_active: false,
    },
    CommandSpec {
        name: "/resume",
        help: "resume a durable session",
        allowed_while_active: false,
    },
    CommandSpec {
        name: "/new",
        help: "start a fresh durable session",
        allowed_while_active: false,
    },
    CommandSpec {
        name: "/steer",
        help: "queue a prompt for the active turn",
        allowed_while_active: true,
    },
    CommandSpec {
        name: "/followup",
        help: "queue a prompt for the next idle boundary",
        allowed_while_active: true,
    },
    CommandSpec {
        name: "/quit",
        help: "exit after cancellation and settlement",
        allowed_while_active: true,
    },
];

pub(crate) const fn all() -> &'static [CommandSpec] {
    COMMANDS
}

pub(crate) fn find(name: &str) -> Option<CommandSpec> {
    COMMANDS
        .iter()
        .copied()
        .find(|command| command.name == name)
}

pub(crate) fn matching(prefix: &str) -> Vec<CommandSpec> {
    COMMANDS
        .iter()
        .copied()
        .filter(|command| command.name.starts_with(prefix))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_unique_and_contains_the_reduced_surface() {
        for (index, command) in COMMANDS.iter().enumerate() {
            assert!(COMMANDS[index + 1..]
                .iter()
                .all(|other| other.name != command.name));
        }
        assert_eq!(COMMANDS.len(), 9);
        assert!(find("/steer").is_some_and(|command| command.allowed_while_active));
        assert!(find("/model").is_some_and(|command| !command.allowed_while_active));
    }
}
