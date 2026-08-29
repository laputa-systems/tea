//! Generated help from the command schema.

use std::fmt::Write as _;

use super::command::{CommandSpec, OptionSpec, ROOT_COMMAND, ROOT_OPTIONS, SESSION};

/// Render the complete top-level reference. This is one recursive command
/// reference rather than a short index followed by copied standalone pages.
pub fn render_root() -> String {
    let mut output = String::new();
    writeln!(output, "tea: {}", ROOT_COMMAND.description).ok();
    writeln!(
        output,
        "\nUsage: {}",
        usage(&ROOT_COMMAND, ROOT_COMMAND.name)
    )
    .ok();
    writeln!(
        output,
        "\nCommon options (shown once; command pages show placement):"
    )
    .ok();
    render_options(&mut output, ROOT_OPTIONS, 2);
    writeln!(
        output,
        "\nEvery command also accepts -h/--help and -v/--version."
    )
    .ok();
    writeln!(output, "\nCommand reference:").ok();
    for command in ROOT_COMMAND.subcommands {
        render_reference(&mut output, command, ROOT_COMMAND.name, 2);
    }
    output
}

/// Render the detailed page for one command.
pub fn render_command(command: &'static CommandSpec) -> String {
    let mut output = String::new();
    let path = command_path(command);
    writeln!(output, "tea: {}", command.description).ok();
    writeln!(output, "\nUsage: {}", usage(command, &path)).ok();
    if !command.options.is_empty() {
        writeln!(output, "\nOptions:").ok();
        render_options(&mut output, command.options, 2);
    }
    if command.name != ROOT_COMMAND.name {
        writeln!(
            output,
            "\nCommon options (accepted at the root; placement is shown here):"
        )
        .ok();
        render_inherited_options(&mut output, command, 2);
    }
    if !command.positionals.is_empty() {
        writeln!(output, "\nArguments:").ok();
        render_positionals(&mut output, command, 2);
    }
    if !command.subcommands.is_empty() {
        writeln!(output, "\nSubcommands:").ok();
        for child in command.subcommands {
            render_reference(&mut output, child, &path, 2);
        }
    }
    if !command.examples.is_empty() {
        writeln!(output, "\nExamples:").ok();
        for example in command.examples {
            writeln!(output, "  {example}").ok();
        }
    }
    output
}

/// A concise usage hint suitable for parse failures.
pub fn usage_hint() -> String {
    format!(
        "Usage: {} (try {} --help for the complete reference)",
        usage(&ROOT_COMMAND, ROOT_COMMAND.name),
        ROOT_COMMAND.name
    )
}

fn render_reference(
    output: &mut String,
    command: &'static CommandSpec,
    parent: &str,
    indent: usize,
) {
    let path = format!("{parent} {}", command.name);
    let padding = " ".repeat(indent);
    writeln!(output, "{padding}{}", usage(command, &path)).ok();
    writeln!(output, "{padding}  {}", command.description).ok();
    let local_options = command
        .options
        .iter()
        .filter(|option| {
            !matches!(
                option.key,
                super::command::OptionKey::Help | super::command::OptionKey::Version
            )
        })
        .copied()
        .collect::<Vec<_>>();
    if !local_options.is_empty() {
        writeln!(output, "{padding}  Options:").ok();
        render_options(output, &local_options, indent + 4);
    }
    if !command.positionals.is_empty() {
        writeln!(output, "{padding}  Arguments:").ok();
        render_positionals(output, command, indent + 4);
    }
    for child in command.subcommands {
        render_reference(output, child, &path, indent + 2);
    }
}

fn render_options(output: &mut String, options: &[OptionSpec], indent: usize) {
    for option in options {
        let padding = " ".repeat(indent);
        let mut names = String::new();
        if let Some(short) = option.short {
            write!(names, "-{short}").ok();
            for alias in option.aliases {
                write!(names, ", -{alias}").ok();
            }
            write!(names, ", ").ok();
        }
        write!(names, "--{}", option.long).ok();
        if let Some(value_name) = option.value_name {
            write!(names, " <{value_name}>").ok();
        }
        let mut markers = String::new();
        if option.required {
            markers.push_str(" required");
        }
        if option.repeatable {
            markers.push_str(" repeatable");
        }
        if let Some(default) = option.default {
            write!(markers, " default: {default}").ok();
        }
        if let Some(env) = option.env {
            write!(markers, " env: {env}").ok();
        }
        writeln!(output, "{padding}{names:<32} {}{markers}", option.help).ok();
    }
}

fn render_inherited_options(output: &mut String, command: &CommandSpec, indent: usize) {
    let inherited = ROOT_OPTIONS
        .iter()
        .filter(|root_option| {
            !command
                .options
                .iter()
                .any(|option| option.key == root_option.key)
        })
        .copied()
        .collect::<Vec<_>>();
    render_options(output, &inherited, indent);
}

fn render_positionals(output: &mut String, command: &CommandSpec, indent: usize) {
    for positional in command.positionals {
        let padding = " ".repeat(indent);
        let mut markers = String::new();
        if positional.required {
            markers.push_str(" required");
        }
        if positional.repeatable {
            markers.push_str(" repeatable");
        }
        writeln!(
            output,
            "{padding}{:<24} {}{markers}",
            positional.name, positional.help
        )
        .ok();
    }
}

fn usage(command: &CommandSpec, path: &str) -> String {
    let mut result = path.to_owned();
    if !command.options.is_empty() {
        result.push_str(" [OPTIONS]");
    }
    for positional in command.positionals {
        result.push(' ');
        if positional.required {
            result.push_str(&format!("<{}>", positional.name));
        } else {
            result.push_str(&format!("[{}]", positional.name));
        }
    }
    if !command.subcommands.is_empty() {
        result.push_str(" <COMMAND>");
    }
    result
}

fn command_path(command: &CommandSpec) -> String {
    if command.name == ROOT_COMMAND.name {
        "tea".to_owned()
    } else if ROOT_COMMAND
        .subcommands
        .iter()
        .any(|candidate| candidate.name == command.name)
    {
        format!("tea {}", command.name)
    } else {
        format!("{} {} {}", ROOT_COMMAND.name, SESSION.name, command.name)
    }
}
