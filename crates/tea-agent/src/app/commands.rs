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
        name: "/models",
        help: "select a provider/model",
        allowed_while_active: false,
    },
    CommandSpec {
        name: "/resume",
        help: "pick and resume a durable session",
        allowed_while_active: false,
    },
    CommandSpec {
        name: "/new",
        help: "start a fresh durable session",
        allowed_while_active: false,
    },
];

pub(crate) const fn all() -> &'static [CommandSpec] {
    COMMANDS
}

/// Return native names that immutable extension commands must never shadow.
pub(crate) fn names() -> impl Iterator<Item = &'static str> {
    COMMANDS.iter().map(|command| command.name)
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
        assert_eq!(COMMANDS.len(), 4);
        assert!(find("/models").is_some_and(|command| !command.allowed_while_active));
        assert!(find("/model").is_none());
        assert!(find("/thinking").is_none());
        assert!(find("/session").is_none());
        assert!(find("/quit").is_none());
        assert!(find("/steer").is_none());
        assert!(find("/followup").is_none());
    }
}
