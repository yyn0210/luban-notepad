//! Shared helpers for the command palette.
//!
//! The UI state lives in `app.rs`; this module builds the grouped command
//! model so it can be unit-tested without a window.

use crate::commands::{CommandCategory, CommandSpec, COMMANDS};

/// Commands grouped by category, preserving registry order.
pub fn command_groups() -> Vec<(CommandCategory, Vec<&'static CommandSpec>)> {
    CommandCategory::ALL
        .iter()
        .filter_map(|category| {
            let commands: Vec<_> = COMMANDS
                .iter()
                .filter(|command| command.category == *category)
                .collect();
            (!commands.is_empty()).then_some((*category, commands))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_cover_every_command_once() {
        let groups = command_groups();
        let total: usize = groups.iter().map(|(_, commands)| commands.len()).sum();
        assert_eq!(total, COMMANDS.len());
    }
}
