pub struct Command {
    pub title: &'static str,
    pub action: &'static str,
    keywords: &'static str,
}

pub const COMMANDS: &[Command] = &[
    Command {
        title: "New Tab",
        action: "new",
        keywords: "create terminal shell session",
    },
    Command {
        title: "New Tab in Directory…",
        action: "open-folder",
        keywords: "open folder path project",
    },
    Command {
        title: "Add Workspace…",
        action: "new-workspace",
        keywords: "create new group project",
    },
    Command {
        title: "Manage Workspaces",
        action: "manage-workspaces",
        keywords: "edit remove delete groups projects",
    },
    Command {
        title: "Split Right",
        action: "split-right",
        keywords: "horizontal pane side",
    },
    Command {
        title: "Split Down",
        action: "split-down",
        keywords: "vertical pane below bottom",
    },
    Command {
        title: "Close Pane",
        action: "close",
        keywords: "remove terminal",
    },
    Command {
        title: "Find in Visible Terminal…",
        action: "find",
        keywords: "search text match locate",
    },
    Command {
        title: "Rename Tab…",
        action: "rename",
        keywords: "edit name title",
    },
    Command {
        title: "Toggle Sidebar",
        action: "sidebar",
        keywords: "show hide tabs panel",
    },
    Command {
        title: "Preferences",
        action: "preferences",
        keywords: "settings options configuration font theme appearance cursor",
    },
    Command {
        title: "Keyboard Shortcuts",
        action: "shortcuts",
        keywords: "help keys keybindings bindings accelerators",
    },
    Command {
        title: "Toggle Fullscreen",
        action: "fullscreen",
        keywords: "maximize full screen window",
    },
];

pub struct Query {
    phrase: String,
    words: Vec<String>,
}

impl Query {
    pub fn new(text: &str) -> Self {
        let words = words(text);
        Self {
            phrase: words.join(" "),
            words,
        }
    }

    // Lower scores rank first. Exact labels precede keywords and typo matches.
    pub fn score(&self, command: &Command) -> Option<u32> {
        if self.words.is_empty() {
            return Some(0);
        }
        let title = words(command.title);
        let phrase = title.join(" ");
        if phrase == self.phrase {
            return Some(0);
        }
        if phrase.starts_with(&self.phrase) {
            return Some(1);
        }
        if phrase.contains(&self.phrase) {
            return Some(2);
        }
        let keywords = words(command.keywords);
        self.words.iter().try_fold(0, |score, query| {
            let best = title
                .iter()
                .filter_map(|word| word_score(query, word))
                .chain(
                    keywords
                        .iter()
                        .filter_map(|word| word_score(query, word).map(|score| score + 8)),
                )
                .min()?;
            Some(score + best)
        })
    }
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn word_score(query: &str, word: &str) -> Option<u32> {
    if word == query {
        return Some(4);
    }
    if word.starts_with(query) {
        return Some(6);
    }
    if word.contains(query) {
        return Some(8);
    }
    let query: Vec<_> = query.chars().collect();
    // Short terms must match literally to avoid unrelated one-letter results.
    let limit = match query.len() {
        0..=3 => return None,
        4..=7 => 1,
        _ => 2,
    };
    let word: Vec<_> = word.chars().collect();
    if query.len().abs_diff(word.len()) > limit {
        return None;
    }
    let distance = edit_distance(&query, &word);
    (distance <= limit).then_some(20 + distance as u32)
}

// Include adjacent transpositions, a common typing mistake.
fn edit_distance(query: &[char], word: &[char]) -> usize {
    let mut before_previous = vec![0; word.len() + 1];
    let mut previous: Vec<_> = (0..=word.len()).collect();
    let mut current = vec![0; word.len() + 1];
    for (i, &letter) in query.iter().enumerate() {
        current[0] = i + 1;
        for (j, &other) in word.iter().enumerate() {
            current[j + 1] = (current[j] + 1)
                .min(previous[j + 1] + 1)
                .min(previous[j] + usize::from(letter != other));
            if i > 0 && j > 0 && letter == word[j - 1] && query[i - 1] == other {
                current[j + 1] = current[j + 1].min(before_previous[j - 1] + 1);
            }
        }
        std::mem::swap(&mut before_previous, &mut previous);
        std::mem::swap(&mut previous, &mut current);
    }
    previous[word.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn results(query: &str) -> Vec<&'static str> {
        let query = Query::new(query);
        let mut matches: Vec<_> = COMMANDS
            .iter()
            .filter_map(|command| query.score(command).map(|score| (score, command.action)))
            .collect();
        matches.sort_by_key(|&(score, _)| score);
        matches.into_iter().map(|(_, action)| action).collect()
    }

    #[test]
    fn finds_keywords_and_combines_words_in_any_order() {
        for query in ["settings", "font", "appearance"] {
            assert_eq!(results(query), ["preferences"]);
        }
        assert_eq!(results("keybindings"), ["shortcuts"]);
        assert_eq!(results("new directory"), ["open-folder"]);
        assert_eq!(results("RIGHT split"), ["split-right"]);
        assert_eq!(results("  New Tab in Directory…  ")[0], "open-folder");
    }

    #[test]
    fn tolerates_insertions_deletions_substitutions_and_transpositions() {
        for query in [
            "prefereences",
            "prefernces",
            "preferenxes",
            "preferneces",
            "settigns",
        ] {
            assert_eq!(results(query)[0], "preferences", "{query}");
        }
        assert_eq!(results("splti riht"), ["split-right"]);
    }

    #[test]
    fn ranks_labels_before_keywords_and_preserves_the_default_order() {
        assert_eq!(
            results(""),
            COMMANDS
                .iter()
                .map(|command| command.action)
                .collect::<Vec<_>>()
        );
        assert_eq!(results("new")[..2], ["new", "open-folder"]);
        assert_eq!(results("split"), ["split-right", "split-down"]);
        assert_eq!(results("tab")[0], "new");
    }

    #[test]
    fn rejects_unrelated_and_short_fuzzy_queries() {
        for query in [
            "nonsense",
            "xyz",
            "slt",
            "new cursor",
            "界界界界",
            &"x".repeat(10000),
        ] {
            assert!(results(query).is_empty(), "{query}");
        }
    }
}
