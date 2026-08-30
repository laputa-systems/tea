//! The declarative command schema for the `tea` binary.
//!
//! Parser recognition and help rendering both consume these values.  Keeping
//! spellings, value names, defaults, and explanations here prevents a command
//! from acquiring a parser-only option or a help-only option by accident.

/// The identity of an option in the command schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionKey {
    Help,
    Version,
    Provider,
    Model,
    LocalBaseUrl,
    LocalContextWindow,
    Thinking,
    Prompt,
    Cwd,
    TeaHome,
    Root,
    Apply,
    Device,
    NoOpen,
}

/// One option spelling and its user-facing contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OptionSpec {
    pub key: OptionKey,
    pub short: Option<char>,
    pub aliases: &'static [char],
    pub long: &'static str,
    pub value_name: Option<&'static str>,
    pub required: bool,
    pub repeatable: bool,
    pub default: Option<&'static str>,
    pub env: Option<&'static str>,
    pub help: &'static str,
    /// The stable name used in typed parse errors.
    pub error_name: &'static str,
}

/// One positional argument in a command's schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PositionalSpec {
    pub name: &'static str,
    pub required: bool,
    pub repeatable: bool,
    pub help: &'static str,
}

/// One command, including its nested command tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub options: &'static [OptionSpec],
    pub positionals: &'static [PositionalSpec],
    pub subcommands: &'static [&'static CommandSpec],
    pub examples: &'static [&'static str],
}

pub static HELP: OptionSpec = OptionSpec {
    key: OptionKey::Help,
    short: Some('h'),
    aliases: &[],
    long: "help",
    value_name: None,
    required: false,
    repeatable: false,
    default: None,
    env: None,
    help: "Print the complete generated command reference.",
    error_name: "--help",
};

pub static VERSION: OptionSpec = OptionSpec {
    key: OptionKey::Version,
    short: Some('v'),
    aliases: &['V'],
    long: "version",
    value_name: None,
    required: false,
    repeatable: false,
    default: None,
    env: None,
    help: "Print the package version and compile-time Git revision.",
    error_name: "--version",
};

pub static PROVIDER: OptionSpec = OptionSpec {
    key: OptionKey::Provider,
    short: None,
    aliases: &[],
    long: "provider",
    value_name: Some("id"),
    required: false,
    repeatable: false,
    default: None,
    env: None,
    help: "Select a compiled model provider.",
    error_name: "--provider",
};

pub static MODEL: OptionSpec = OptionSpec {
    key: OptionKey::Model,
    short: None,
    aliases: &[],
    long: "model",
    value_name: Some("id"),
    required: false,
    repeatable: false,
    default: None,
    env: None,
    help: "Select a model from the chosen provider.",
    error_name: "--model",
};

pub static LOCAL_BASE_URL: OptionSpec = OptionSpec {
    key: OptionKey::LocalBaseUrl,
    short: None,
    aliases: &[],
    long: "local-base-url",
    value_name: Some("url"),
    required: false,
    repeatable: false,
    default: None,
    env: None,
    help: "Set the local provider API root.",
    error_name: "--local-base-url",
};

pub static LOCAL_CONTEXT_WINDOW: OptionSpec = OptionSpec {
    key: OptionKey::LocalContextWindow,
    short: None,
    aliases: &[],
    long: "local-context-window",
    value_name: Some("tokens"),
    required: false,
    repeatable: false,
    default: None,
    env: None,
    help: "Set local context capacity for automatic compaction.",
    error_name: "--local-context-window",
};

pub static THINKING: OptionSpec = OptionSpec {
    key: OptionKey::Thinking,
    short: None,
    aliases: &[],
    long: "thinking",
    value_name: Some("level"),
    required: false,
    repeatable: false,
    default: Some("off"),
    env: None,
    help: "Set reasoning level (off, minimal, low, medium, high, xhigh, max).",
    error_name: "--thinking",
};

pub static PROMPT: OptionSpec = OptionSpec {
    key: OptionKey::Prompt,
    short: Some('p'),
    aliases: &[],
    long: "prompt",
    value_name: Some("message"),
    required: false,
    repeatable: false,
    default: None,
    env: None,
    help: "Stream one response and exit (requires provider and model).",
    error_name: "-p/--prompt",
};

pub static CWD: OptionSpec = OptionSpec {
    key: OptionKey::Cwd,
    short: None,
    aliases: &[],
    long: "cwd",
    value_name: Some("path"),
    required: false,
    repeatable: false,
    default: None,
    env: None,
    help: "Use path as the explicit workspace.",
    error_name: "--cwd",
};

pub static TEA_HOME: OptionSpec = OptionSpec {
    key: OptionKey::TeaHome,
    short: None,
    aliases: &[],
    long: "tea-home",
    value_name: Some("path"),
    required: false,
    repeatable: false,
    default: Some("~/.tea"),
    env: None,
    help: "Use path as the explicit Tea extension home.",
    error_name: "--tea-home",
};

pub static ROOT: OptionSpec = OptionSpec {
    key: OptionKey::Root,
    short: None,
    aliases: &[],
    long: "root",
    value_name: Some("artifact-id"),
    required: false,
    repeatable: true,
    default: None,
    env: None,
    help: "Retain one additional immutable artifact root (repeatable).",
    error_name: "--root",
};

pub static APPLY: OptionSpec = OptionSpec {
    key: OptionKey::Apply,
    short: None,
    aliases: &[],
    long: "apply",
    value_name: None,
    required: false,
    repeatable: false,
    default: None,
    env: None,
    help: "Apply the garbage-collection plan instead of previewing it.",
    error_name: "--apply",
};

pub static DEVICE: OptionSpec = OptionSpec {
    key: OptionKey::Device,
    short: None,
    aliases: &[],
    long: "device",
    value_name: None,
    required: false,
    repeatable: false,
    default: None,
    env: None,
    help: "Use the headless ChatGPT device authorization flow.",
    error_name: "--device",
};

pub static NO_OPEN: OptionSpec = OptionSpec {
    key: OptionKey::NoOpen,
    short: None,
    aliases: &[],
    long: "no-open",
    value_name: None,
    required: false,
    repeatable: false,
    default: None,
    env: None,
    help: "Print the browser authorization URL without launching it.",
    error_name: "--no-open",
};

pub static ROOT_OPTIONS: &[OptionSpec] = &[
    HELP,
    VERSION,
    PROVIDER,
    MODEL,
    LOCAL_BASE_URL,
    LOCAL_CONTEXT_WINDOW,
    THINKING,
    PROMPT,
    CWD,
    TEA_HOME,
];

pub static SESSION_OPTIONS: &[OptionSpec] = &[HELP, VERSION];
pub static INSPECT_OPTIONS: &[OptionSpec] = &[HELP, VERSION, TEA_HOME];
pub static DUMP_OPTIONS: &[OptionSpec] = &[HELP, VERSION, TEA_HOME];
pub static VERIFY_OPTIONS: &[OptionSpec] = &[HELP, VERSION, ROOT];
pub static GC_OPTIONS: &[OptionSpec] = &[HELP, VERSION, ROOT, APPLY];
pub static EXPORT_OPTIONS: &[OptionSpec] = &[HELP, VERSION, ROOT];
pub static EMPTY_OPTIONS: &[OptionSpec] = &[HELP, VERSION];
pub static AUTH_OPTIONS: &[OptionSpec] = &[HELP, VERSION];
pub static AUTH_LOGIN_OPTIONS: &[OptionSpec] = &[HELP, VERSION, TEA_HOME, DEVICE, NO_OPEN];
pub static AUTH_STATUS_OPTIONS: &[OptionSpec] = &[HELP, VERSION, TEA_HOME];

pub static INSPECT_POSITIONALS: &[PositionalSpec] = &[PositionalSpec {
    name: "SESSION_ID",
    required: true,
    repeatable: false,
    help: "Session identifier to inspect below Tea home.",
}];
pub static DUMP_POSITIONALS: &[PositionalSpec] = INSPECT_POSITIONALS;
pub static DIRECTORY_POSITIONALS: &[PositionalSpec] = &[PositionalSpec {
    name: "DIRECTORY",
    required: true,
    repeatable: false,
    help: "Authoritative session directory.",
}];
pub static VERIFY_POSITIONALS: &[PositionalSpec] = DIRECTORY_POSITIONALS;
pub static GC_POSITIONALS: &[PositionalSpec] = DIRECTORY_POSITIONALS;
pub static EXPORT_POSITIONALS: &[PositionalSpec] = &[
    PositionalSpec {
        name: "SOURCE",
        required: true,
        repeatable: false,
        help: "Source session directory.",
    },
    PositionalSpec {
        name: "DESTINATION",
        required: true,
        repeatable: false,
        help: "Non-overwriting export directory.",
    },
];
pub static RESTORE_POSITIONALS: &[PositionalSpec] = &[
    PositionalSpec {
        name: "SOURCE",
        required: true,
        repeatable: false,
        help: "Source export directory.",
    },
    PositionalSpec {
        name: "DESTINATION",
        required: true,
        repeatable: false,
        help: "Non-overwriting restore directory.",
    },
];
pub static AUTH_PROVIDER_POSITIONALS: &[PositionalSpec] = &[PositionalSpec {
    name: "PROVIDER",
    required: true,
    repeatable: false,
    help: "Provider identity; currently only `codex` is supported.",
}];

pub static INSPECT: CommandSpec = CommandSpec {
    name: "inspect",
    description: "Inspect a durable session addressed by ID.",
    options: INSPECT_OPTIONS,
    positionals: INSPECT_POSITIONALS,
    subcommands: &[],
    examples: &["tea session inspect SESSION_ID"],
};
pub static DUMP: CommandSpec = CommandSpec {
    name: "dump",
    description: "Dump authoritative JSONL records addressed by ID.",
    options: DUMP_OPTIONS,
    positionals: DUMP_POSITIONALS,
    subcommands: &[],
    examples: &["tea session dump SESSION_ID"],
};
pub static REPAIR: CommandSpec = CommandSpec {
    name: "repair",
    description: "Remove only an unterminated final JSONL tail.",
    options: EMPTY_OPTIONS,
    positionals: DIRECTORY_POSITIONALS,
    subcommands: &[],
    examples: &["tea session repair SESSION_DIRECTORY"],
};
pub static REBUILD_META: CommandSpec = CommandSpec {
    name: "rebuild-meta",
    description: "Rebuild disposable session metadata caches.",
    options: EMPTY_OPTIONS,
    positionals: DIRECTORY_POSITIONALS,
    subcommands: &[],
    examples: &["tea session rebuild-meta SESSION_DIRECTORY"],
};
pub static VERIFY: CommandSpec = CommandSpec {
    name: "verify",
    description: "Replay and verify session-owned immutable objects.",
    options: VERIFY_OPTIONS,
    positionals: VERIFY_POSITIONALS,
    subcommands: &[],
    examples: &["tea session verify SESSION_DIRECTORY --root ARTIFACT_ID"],
};
pub static GC: CommandSpec = CommandSpec {
    name: "gc",
    description: "Plan artifact collection, or apply the plan explicitly.",
    options: GC_OPTIONS,
    positionals: GC_POSITIONALS,
    subcommands: &[],
    examples: &[
        "tea session gc SESSION_DIRECTORY",
        "tea session gc SESSION_DIRECTORY --apply",
    ],
};
pub static EXPORT: CommandSpec = CommandSpec {
    name: "export",
    description: "Create a non-overwriting portable export.",
    options: EXPORT_OPTIONS,
    positionals: EXPORT_POSITIONALS,
    subcommands: &[],
    examples: &["tea session export SOURCE DESTINATION"],
};
pub static RESTORE: CommandSpec = CommandSpec {
    name: "restore",
    description: "Restore an export into a new non-overwriting directory.",
    options: EMPTY_OPTIONS,
    positionals: RESTORE_POSITIONALS,
    subcommands: &[],
    examples: &["tea session restore SOURCE DESTINATION"],
};

pub static AUTH_LOGIN: CommandSpec = CommandSpec {
    name: "login",
    description: "Authorize a Tea-owned provider credential.",
    options: AUTH_LOGIN_OPTIONS,
    positionals: AUTH_PROVIDER_POSITIONALS,
    subcommands: &[],
    examples: &[
        "tea auth login codex",
        "tea auth login codex --device",
        "tea auth login codex --no-open",
    ],
};

pub static AUTH_LOGOUT: CommandSpec = CommandSpec {
    name: "logout",
    description: "Revoke when possible and remove a Tea-owned provider credential.",
    options: AUTH_STATUS_OPTIONS,
    positionals: AUTH_PROVIDER_POSITIONALS,
    subcommands: &[],
    examples: &["tea auth logout codex"],
};

pub static AUTH_STATUS: CommandSpec = CommandSpec {
    name: "status",
    description: "Show non-secret status for a Tea-owned provider credential.",
    options: AUTH_STATUS_OPTIONS,
    positionals: AUTH_PROVIDER_POSITIONALS,
    subcommands: &[],
    examples: &["tea auth status codex"],
};

pub static AUTH: CommandSpec = CommandSpec {
    name: "auth",
    description: "Manage explicit Tea-owned provider authorizations.",
    options: AUTH_OPTIONS,
    positionals: &[],
    subcommands: &[&AUTH_LOGIN, &AUTH_LOGOUT, &AUTH_STATUS],
    examples: &["tea auth login codex --device"],
};

pub static SESSION: CommandSpec = CommandSpec {
    name: "session",
    description: "Inspect, repair, verify, collect, export, or restore durable sessions.",
    options: SESSION_OPTIONS,
    positionals: &[],
    subcommands: &[
        &INSPECT,
        &DUMP,
        &REPAIR,
        &REBUILD_META,
        &VERIFY,
        &GC,
        &EXPORT,
        &RESTORE,
    ],
    examples: &["tea session inspect SESSION_ID"],
};

pub static ROOT_COMMAND: CommandSpec = CommandSpec {
    name: "tea",
    description: "Minimal interactive terminal host and durable-session operator.",
    options: ROOT_OPTIONS,
    positionals: &[],
    subcommands: &[&SESSION, &AUTH],
    examples: &[
        "tea --provider PROVIDER --model MODEL",
        "tea auth login codex",
        "tea session inspect SESSION_ID",
    ],
};

/// Find an option by the lexopt token that introduced it.
pub fn find_option<'a>(
    argument: &lexopt::Arg<'a>,
    options: &'static [OptionSpec],
) -> Option<&'static OptionSpec> {
    match argument {
        lexopt::Arg::Short(short) => options
            .iter()
            .find(|spec| spec.short == Some(*short) || spec.aliases.contains(short)),
        lexopt::Arg::Long(long) => options.iter().find(|spec| spec.long == *long),
        lexopt::Arg::Value(_) => None,
    }
}

/// Validate the declarative tree.  This is also used by metadata tests so a
/// missing explanation or spelling collision cannot silently ship.
pub fn validate_schema() -> Result<(), &'static str> {
    fn validate_command(command: &CommandSpec, path: &str) -> Result<(), &'static str> {
        if command.name.is_empty() || command.description.trim().is_empty() {
            return Err("every command needs a non-empty name and description");
        }
        for (index, option) in command.options.iter().enumerate() {
            if option.long.is_empty() || option.help.trim().is_empty() {
                return Err("every option needs a name and explanation");
            }
            if option
                .value_name
                .is_some_and(|value_name| value_name.trim().is_empty())
            {
                return Err("option value names must not be empty");
            }
            for other in command.options.iter().skip(index + 1) {
                if option.long == other.long
                    || option
                        .short
                        .is_some_and(|short| option.aliases.contains(&short))
                    || other
                        .short
                        .is_some_and(|short| other.aliases.contains(&short))
                    || option.short.is_some_and(|short| {
                        other.short == Some(short) || other.aliases.contains(&short)
                    })
                    || option
                        .aliases
                        .iter()
                        .any(|short| other.short == Some(*short) || other.aliases.contains(short))
                {
                    return Err("option spellings must be unique within a command");
                }
            }
        }
        for positional in command.positionals {
            if positional.name.is_empty() || positional.help.trim().is_empty() {
                return Err("every positional needs a name and explanation");
            }
        }
        for (index, positional) in command.positionals.iter().enumerate() {
            for other in command.positionals.iter().skip(index + 1) {
                if positional.name == other.name {
                    return Err("positional names must be unique within a command");
                }
            }
        }
        for child in command.subcommands {
            if command
                .subcommands
                .iter()
                .filter(|candidate| candidate.name == child.name)
                .count()
                > 1
            {
                return Err("command names must be unique within a scope");
            }
            let child_path = format!("{path} {}", child.name);
            let _ = child_path;
            validate_command(child, path)?;
        }
        Ok(())
    }

    validate_command(&ROOT_COMMAND, "tea")
}
