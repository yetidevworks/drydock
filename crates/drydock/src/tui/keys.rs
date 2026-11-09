//! The keys that hand a repo off to another tool, which `[keys]` can move.
//!
//! Only these five move. Everything else in the dashboard keeps its key, and
//! a hand-off key can't be put on top of one of them, so a key means the same
//! thing in every view.

use crossterm::event::KeyCode;

use crate::config::KeysConfig;

/// What a hand-off key does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandOff {
    Editor,
    GitClient,
    FileManager,
    Terminal,
    /// The repo's path, or the commit hash in the history view.
    Copy,
}

/// The hand-off keys in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keys {
    pub editor: char,
    pub git_client: char,
    pub file_manager: char,
    pub terminal: char,
    pub copy: char,
}

impl Default for Keys {
    fn default() -> Self {
        Self {
            editor: 'O',
            git_client: 't',
            file_manager: 'o',
            terminal: 'T',
            copy: 'y',
        }
    }
}

/// Every key with a fixed meaning in some view: the table, the detail view,
/// the column picker, help and history. A new binding in `mod.rs` or
/// `history.rs` belongs here too, or a config could put a hand-off on it.
const BUILT_IN: &str = " qC?jkdurNHhbcixenas&S[]01234/wRfFJKWp";

impl Keys {
    /// Check a `[keys]` table. Anything wrong with it and none of it is used:
    /// a key falling back on its own could land on another one, and the
    /// defaults are a set that's known to work.
    pub fn resolve(cfg: &KeysConfig) -> Result<Self, String> {
        let fields = [
            ("editor", &cfg.editor),
            ("git_client", &cfg.git_client),
            ("file_manager", &cfg.file_manager),
            ("terminal", &cfg.terminal),
            ("copy", &cfg.copy),
        ];
        let mut problems = Vec::new();
        let mut chosen: Vec<(&str, char)> = Vec::new();
        for (name, raw) in fields {
            let mut chars = raw.chars();
            let key = match (chars.next(), chars.next()) {
                (Some(c), None) if !c.is_whitespace() && !c.is_control() => c,
                _ => {
                    problems.push(format!("{name} = {raw:?} is not a single key"));
                    continue;
                }
            };
            if BUILT_IN.contains(key) {
                problems.push(format!("{name} = {raw:?} is already a drydock key"));
                continue;
            }
            if let Some((other, _)) = chosen.iter().find(|(_, c)| *c == key) {
                problems.push(format!("{other} and {name} are both {raw:?}"));
                continue;
            }
            chosen.push((name, key));
        }
        if !problems.is_empty() {
            return Err(format!(
                "[keys] not used, keeping the default keys: {}",
                problems.join("; ")
            ));
        }
        let get = |n: &str| chosen.iter().find(|(name, _)| *name == n).unwrap().1;
        Ok(Self {
            editor: get("editor"),
            git_client: get("git_client"),
            file_manager: get("file_manager"),
            terminal: get("terminal"),
            copy: get("copy"),
        })
    }

    pub fn key(&self, action: HandOff) -> char {
        match action {
            HandOff::Editor => self.editor,
            HandOff::GitClient => self.git_client,
            HandOff::FileManager => self.file_manager,
            HandOff::Terminal => self.terminal,
            HandOff::Copy => self.copy,
        }
    }

    /// The hand-off a key press asks for, if it's one of these keys.
    pub fn action(&self, code: KeyCode) -> Option<HandOff> {
        let KeyCode::Char(c) = code else { return None };
        [
            HandOff::Editor,
            HandOff::GitClient,
            HandOff::FileManager,
            HandOff::Terminal,
            HandOff::Copy,
        ]
        .into_iter()
        .find(|&action| self.key(action) == c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pairs: &[(&str, &str)]) -> KeysConfig {
        let mut cfg = KeysConfig::default();
        for (name, key) in pairs {
            let slot = match *name {
                "editor" => &mut cfg.editor,
                "git_client" => &mut cfg.git_client,
                "file_manager" => &mut cfg.file_manager,
                "terminal" => &mut cfg.terminal,
                "copy" => &mut cfg.copy,
                _ => unreachable!(),
            };
            *slot = key.to_string();
        }
        cfg
    }

    #[test]
    fn the_default_table_gives_the_default_keys() {
        assert_eq!(Keys::resolve(&KeysConfig::default()), Ok(Keys::default()));
    }

    // The request that started this: nvim, lazygit and yazi on v, g and y,
    // with copying moved out of the way to Y.
    #[test]
    fn hand_offs_can_follow_the_tools_they_open() {
        let keys = Keys::resolve(&table(&[
            ("editor", "v"),
            ("git_client", "g"),
            ("file_manager", "y"),
            ("copy", "Y"),
        ]))
        .unwrap();
        assert_eq!(keys.action(KeyCode::Char('v')), Some(HandOff::Editor));
        assert_eq!(keys.action(KeyCode::Char('g')), Some(HandOff::GitClient));
        assert_eq!(keys.action(KeyCode::Char('y')), Some(HandOff::FileManager));
        assert_eq!(keys.action(KeyCode::Char('Y')), Some(HandOff::Copy));
        assert_eq!(keys.action(KeyCode::Char('T')), Some(HandOff::Terminal));
        // The keys they moved off do nothing now.
        assert_eq!(keys.action(KeyCode::Char('O')), None);
        assert_eq!(keys.action(KeyCode::Char('o')), None);
        assert_eq!(keys.action(KeyCode::Char('t')), None);
    }

    // Swapping two keys is a reasonable thing to want, and only works if the
    // check looks at the new table rather than the old keys.
    #[test]
    fn two_keys_can_swap() {
        let keys = Keys::resolve(&table(&[("editor", "o"), ("file_manager", "O")])).unwrap();
        assert_eq!(keys.action(KeyCode::Char('o')), Some(HandOff::Editor));
        assert_eq!(keys.action(KeyCode::Char('O')), Some(HandOff::FileManager));
    }

    #[test]
    fn a_key_used_twice_is_refused() {
        let err = Keys::resolve(&table(&[("editor", "v"), ("terminal", "v")])).unwrap_err();
        assert!(err.contains("editor and terminal are both \"v\""), "{err}");
        // Moving one key onto another's default without moving that one too.
        let err = Keys::resolve(&table(&[("editor", "t")])).unwrap_err();
        assert!(
            err.contains("editor and git_client are both \"t\""),
            "{err}"
        );
    }

    // History's `n` and `p` count as much as the table's own keys: a key that
    // opened an editor in the table and stepped through files in history would
    // be worse than no setting at all.
    #[test]
    fn a_key_drydock_already_uses_is_refused_in_any_view() {
        for key in ["q", "j", "f", "/", "n", "p", "W", "J", "C", " "] {
            let err = Keys::resolve(&table(&[("editor", key)])).unwrap_err();
            assert!(err.contains("editor ="), "{key:?}: {err}");
        }
    }

    #[test]
    fn a_key_has_to_be_one_character() {
        for key in ["", "gg", "ctrl-g", "\t"] {
            let err = Keys::resolve(&table(&[("git_client", key)])).unwrap_err();
            assert!(err.contains("is not a single key"), "{key:?}: {err}");
        }
    }

    // Everything that's wrong is said at once, so fixing the config takes
    // one restart rather than one per mistake.
    #[test]
    fn every_problem_is_reported_together() {
        let err = Keys::resolve(&table(&[("editor", "q"), ("copy", "yy")])).unwrap_err();
        assert!(err.contains("editor = \"q\""), "{err}");
        assert!(err.contains("copy = \"yy\""), "{err}");
    }
}
