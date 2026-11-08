//! The text of `--help` (AGL-061, D3): the help of the program, or of one
//! command alone, its lines of usage and its section.

use crate::cli::args::HelpTopic;
use crate::i18n::{Lang, Msg, m};

/// The usage and the section of each command, in the order of the help.
const COMMANDS: [(HelpTopic, Msg, Msg); 6] = [
    (HelpTopic::Init, m::HELP_INIT_USAGE, m::HELP_INIT),
    (HelpTopic::Server, m::HELP_SERVER_USAGE, m::HELP_SERVER),
    (HelpTopic::Function, m::HELP_FUNCTION_USAGE, m::HELP_FUNCTION),
    (HelpTopic::Search, m::HELP_SEARCH_USAGE, m::HELP_SEARCH),
    (HelpTopic::Memory, m::HELP_MEMORY_USAGE, m::HELP_MEMORY),
    (HelpTopic::Forge, m::HELP_FORGE_USAGE, m::HELP_FORGE),
];

/// The help of `topic` in `lang`.
pub fn text(lang: Lang, topic: HelpTopic) -> String {
    let (usages, sections): (Vec<Msg>, Vec<Msg>) = match topic {
        HelpTopic::Program => (
            std::iter::once(m::HELP_RUN_USAGE)
                .chain(COMMANDS.iter().map(|(_, usage, _)| *usage))
                .collect(),
            std::iter::once(m::HELP_RUN)
                .chain(COMMANDS.iter().map(|(_, _, section)| *section))
                .collect(),
        ),
        topic => COMMANDS
            .iter()
            .filter(|(of, _, _)| *of == topic)
            .map(|(_, usage, section)| (*usage, *section))
            .unzip(),
    };
    let prefix = m::HELP_USAGE.text(lang);
    let pad = " ".repeat(prefix.chars().count());
    let mut out = prefix.to_string();
    let lines = usages.iter().flat_map(|usage| usage.text(lang).lines());
    for (i, line) in lines.enumerate() {
        if i > 0 {
            out.push('\n');
            out.push_str(&pad);
        }
        out.push_str(line);
    }
    for section in sections {
        out.push_str("\n\n");
        out.push_str(section.text(lang));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LANGS: [Lang; 2] = [Lang::En, Lang::Uk];

    #[test]
    fn a_command_has_its_usage_and_its_section_alone() {
        for lang in LANGS {
            for (topic, usage, section) in COMMANDS {
                let help = text(lang, topic);
                let first = usage.text(lang).lines().next().unwrap();
                assert!(
                    help.starts_with(&format!("{}{first}", m::HELP_USAGE.text(lang))),
                    "{help}"
                );
                assert!(help.ends_with(section.text(lang)), "{help}");
                for (other, _, other_section) in COMMANDS {
                    if other != topic {
                        assert!(!help.contains(other_section.text(lang)), "{help}");
                    }
                }
                assert!(!help.contains(m::HELP_RUN.text(lang)), "{help}");
            }
        }
    }

    #[test]
    fn the_program_has_every_usage_and_every_section() {
        for lang in LANGS {
            let help = text(lang, HelpTopic::Program);
            assert!(help.contains(m::HELP_RUN.text(lang)));
            for (topic, _, section) in COMMANDS {
                assert!(help.contains(section.text(lang)), "{topic:?}");
                let usage = text(lang, topic);
                let usage = usage.split("\n\n").next().unwrap();
                let first = usage.lines().next().unwrap();
                let first = &first[m::HELP_USAGE.text(lang).len()..];
                assert!(help.contains(first), "{topic:?}");
            }
        }
    }

    #[test]
    fn the_lines_of_usage_are_under_the_first() {
        let help = text(Lang::Uk, HelpTopic::Server);
        let mut lines = help.lines();
        assert!(lines.next().unwrap().starts_with("Використання: agl server start"));
        assert!(lines.next().unwrap().starts_with("              agl server command"));
    }
}
