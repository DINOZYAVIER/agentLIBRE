//! How a new artifact is named (AGL-067, D7, D12; AGL-124, D15): the slug
//! of its title after a prefix and a number of the artifacts of its type.

/// The slug of a title: the lowercase letters and digits of any
/// script; apostrophes are dropped, a run of anything else is one `-`,
/// trimmed.
pub fn slug(title: &str) -> String {
    let mapped: String = title
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, '\'' | '’' | 'ʼ'))
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    mapped
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// The prefix and the number of the next artifact of a type, from the
/// `names` of the artifacts of that type (AGL-124, D15). With the
/// description's `prefix`, one past the highest `<prefix>-NNN`; without
/// it, the prefix most of the numbered names carry, the first by the
/// alphabet when two have as many, `agl` when none is numbered, and one
/// past the highest with that prefix (AGL-067, D7, D12). A numbered name
/// is `<letters>-<digits>-<rest>`; the others are not counted.
pub fn next_number(names: &[String], prefix: Option<&str>) -> (String, u32) {
    // The prefix, how many names carry it and the highest number.
    let mut prefixes: std::collections::BTreeMap<&str, (usize, u32)> = Default::default();
    for name in names {
        if let Some((found, number)) = numbered(name) {
            let entry = prefixes.entry(found).or_default();
            entry.0 += 1;
            entry.1 = entry.1.max(number);
        }
    }
    if let Some(prefix) = prefix {
        let max = prefixes.get(prefix).map(|(_, max)| *max).unwrap_or(0);
        return (prefix.to_owned(), max + 1);
    }
    // `BTreeMap` goes by the alphabet; only a larger count replaces.
    let mut best: Option<(&str, (usize, u32))> = None;
    for (found, counted) in prefixes {
        if best.is_none_or(|(_, held)| counted.0 > held.0) {
            best = Some((found, counted));
        }
    }
    match best {
        Some((found, (_, max))) => (found.to_owned(), max + 1),
        None => ("agl".to_owned(), 1),
    }
}

/// The prefix and the number of a numbered name, `<letters>-<digits>-<rest>`.
fn numbered(name: &str) -> Option<(&str, u32)> {
    let (prefix, rest) = name.split_once('-')?;
    let (digits, rest) = rest.split_once('-')?;
    let all = |text: &str, test: fn(&char) -> bool| !text.is_empty() && text.chars().all(|c| test(&c));
    if !all(prefix, char::is_ascii_alphabetic) || !all(digits, char::is_ascii_digit) || rest.is_empty() {
        return None;
    }
    Some((prefix, digits.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slug() {
        assert_eq!(
            slug("Forge core: the task-spec store"),
            "forge-core-the-task-spec-store"
        );
        assert_eq!(slug("AGL 007"), "agl-007");
        assert_eq!(slug("--weird__name--"), "weird-name");
        assert_eq!(slug(""), "");
        // Any script; the apostrophes of all three kinds are dropped.
        assert_eq!(slug("Пам'ять агента"), "память-агента");
        assert_eq!(slug("Пам’ять агента"), "память-агента");
        assert_eq!(slug("Памʼять агента"), "память-агента");
        assert_eq!(slug("ЗАДАЧА №7: Café"), "задача-7-café");
        assert_eq!(slug("it's a title"), "its-a-title");
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn the_next_number_is_one_past_the_max() {
        assert_eq!(next_number(&[], None), ("agl".to_string(), 1));
        assert_eq!(
            next_number(&names(&["agl-003-x", "agl-001-y"]), None),
            ("agl".to_string(), 4)
        );
        // A name that is not numbered does not count (AGL-067, D12).
        assert_eq!(
            next_number(&names(&["note-1", "agl-x", "agl-005-z"]), None),
            ("agl".to_string(), 6)
        );
    }

    #[test]
    fn a_store_numbers_with_the_prefix_most_of_its_specs_carry() {
        // AGL-067, D7: the prefix most of the specs carry, one past the
        // highest number with that prefix.
        assert_eq!(
            next_number(&names(&["ks-005-a", "ks-032-b", "agl-040-c"]), None),
            ("ks".to_string(), 33)
        );
        // D12: as many specs each, the first prefix by the alphabet.
        assert_eq!(
            next_number(&names(&["ks-005-a", "agl-009-b"]), None),
            ("agl".to_string(), 10)
        );
        // D12: `<letters>-<digits>-<rest>` is a numbered name; the others are
        // not counted, so a store of them is a store with no numbered spec.
        assert_eq!(
            next_number(&names(&["notes", "ks-1", "ks-x-y", "-002-x", "7-002-x", "ks-003-"]), None),
            ("agl".to_string(), 1)
        );
        // A number that does not fit is not a number.
        assert_eq!(
            next_number(&names(&["ks-99999999999-a", "ks-002-b"]), None),
            ("ks".to_string(), 3)
        );
    }

    #[test]
    fn a_description_s_prefix_numbers_its_own_series() {
        // AGL-124, D15: one past the highest `<prefix>-NNN`, the other
        // prefixes not counted.
        assert_eq!(next_number(&[], Some("trial")), ("trial".to_string(), 1));
        assert_eq!(
            next_number(&names(&["trial-002-a", "agl-040-c", "agl-041-d"]), Some("trial")),
            ("trial".to_string(), 3)
        );
    }
}
