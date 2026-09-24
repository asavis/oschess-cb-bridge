//! The order of engine choices (#54). A choice in the settings window takes
//! effect at once; installing Stockfish takes minutes, and chooses the build
//! it installed only if the user chose nothing else while it ran. The build
//! stays in the engine list either way.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use bridge::config;

/// Counts the choices saved.
pub struct Choices(AtomicU64);

/// The count when a longer operation began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket(u64);

impl Default for Choices {
    fn default() -> Self {
        Self::new()
    }
}

impl Choices {
    pub const fn new() -> Self {
        Choices(AtomicU64::new(0))
    }

    /// Taken when an operation that may choose later begins.
    pub fn ticket(&self) -> Ticket {
        Ticket(self.0.load(Ordering::SeqCst))
    }

    /// Saves `engine` as the choice in the `bridge.toml` at `config_path`, now.
    pub fn choose(&self, config_path: &Path, engine: PathBuf) -> Result<(), String> {
        let next = config::Config { engine: Some(engine), ..config::load_or_create(config_path)? };
        config::save(config_path, &next)?;
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Saves `engine` as the choice only if nothing was chosen since `ticket`;
    /// whether it was saved. The caller holds the lock every choice takes.
    pub fn choose_if_current(&self, ticket: Ticket, config_path: &Path, engine: PathBuf) -> Result<bool, String> {
        if self.0.load(Ordering::SeqCst) != ticket.0 {
            return Ok(false);
        }
        self.choose(config_path, engine)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_in(config_path: &Path) -> Option<PathBuf> {
        config::load_or_create(config_path).unwrap().engine
    }

    #[test]
    fn an_installation_does_not_replace_a_choice_made_while_it_ran() {
        let dir = std::env::temp_dir().join(format!("bridge-app-choices-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let toml = dir.join("bridge.toml");
        let choices = Choices::new();

        // Installing begins; the user chooses Lc0 meanwhile; the installation ends.
        let installing = choices.ticket();
        choices.choose(&toml, PathBuf::from("lc0.exe")).unwrap();
        assert!(!choices.choose_if_current(installing, &toml, PathBuf::from("stockfish.exe")).unwrap());
        assert_eq!(engine_in(&toml), Some(PathBuf::from("lc0.exe")));

        // With no choice in between, the installed build is chosen.
        let installing = choices.ticket();
        assert!(choices.choose_if_current(installing, &toml, PathBuf::from("stockfish.exe")).unwrap());
        assert_eq!(engine_in(&toml), Some(PathBuf::from("stockfish.exe")));

        // An installation's own choice counts too: an older one that ends later
        // does not take it back.
        let older = choices.ticket();
        let newer = choices.ticket();
        assert!(choices.choose_if_current(newer, &toml, PathBuf::from("stockfish-20.exe")).unwrap());
        assert!(!choices.choose_if_current(older, &toml, PathBuf::from("stockfish-19.exe")).unwrap());
        assert_eq!(engine_in(&toml), Some(PathBuf::from("stockfish-20.exe")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
