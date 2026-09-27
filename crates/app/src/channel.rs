//! Where this copy of the bridge came from (#112). The NSIS installer from
//! GitHub gives the direct channel: the app updates itself and starts with
//! Windows through the Run key. The Microsoft Store installs the same
//! executable in an MSIX package: the Store updates it, Windows starts it
//! through the package's startup task, and the package carries Stockfish. The
//! app tells the two apart by asking Windows for its package identity.

use std::path::{Component, Path, PathBuf};

/// The startup task the package manifest declares; «Start with Windows»
/// enables it in the Store channel.
pub const STARTUP_TASK: &str = "oschessBridgeStartup";

/// Where the app came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Channel {
    Direct,
    Store(Package),
}

impl Channel {
    pub fn package(&self) -> Option<&Package> {
        match self {
            Channel::Store(package) => Some(package),
            Channel::Direct => None,
        }
    }

    pub fn is_store(&self) -> bool {
        matches!(self, Channel::Store(_))
    }
}

/// The package the app runs from, as Windows names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    /// This version's install folder, `WindowsApps\<full name>`: every
    /// version of the package has its own.
    pub install: PathBuf,
    /// The family name, `<name>_<publisher id>`, the same for every version.
    pub family: String,
}

impl Package {
    /// Whether `path` lies in the install folder of any version of this
    /// package. Windows names those folders
    /// `<name>_<version>_<architecture>_<resource id>_<publisher id>`, side by
    /// side.
    pub fn owns(&self, path: &Path) -> bool {
        let (Some(parent), Some((name, publisher))) = (self.install.parent(), self.family.rsplit_once('_')) else {
            return false;
        };
        let Ok(rest) = path.strip_prefix(parent) else { return false };
        match rest.components().next() {
            // Between the name and the publisher id: the version, the
            // architecture and the resource id, which may be empty.
            Some(Component::Normal(folder)) => folder
                .to_str()
                .and_then(|f| f.strip_prefix(name)?.strip_prefix('_')?.strip_suffix(publisher)?.strip_suffix('_'))
                .is_some_and(|between| between.contains('_')),
            _ => false,
        }
    }
}

/// The engine the Store channel chooses at start in place of `chosen`: the
/// build the package carries, `carried`, when no engine is chosen, when the
/// chosen file is gone, or when it is a build an earlier version of the
/// package carried, whose folder goes with that version. `None` keeps the
/// choice.
pub fn engine_to_choose(
    chosen: Option<&Path>,
    carried: &Path,
    package: &Package,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let keep = match chosen {
        None => false,
        Some(path) => path == carried || (exists(path) && !package.owns(path)),
    };
    (!keep).then(|| carried.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPS: &str = r"C:\Program Files\WindowsApps";

    fn package(version: &str) -> Package {
        Package {
            install: Path::new(APPS).join(format!("oschess.bridge_{version}_x64__8wekyb3d8bbwe")),
            family: "oschess.bridge_8wekyb3d8bbwe".into(),
        }
    }

    fn carried(package: &Package) -> PathBuf {
        package.install.join(r"engines\stockfish-19\stockfish-windows-x86-64-universal.exe")
    }

    #[test]
    fn owns_the_install_folders_of_every_version_and_nothing_else() {
        let now = package("0.2.0.0");
        assert!(now.owns(&carried(&now)));
        assert!(now.owns(&carried(&package("0.1.0.0"))));
        for other in [
            Path::new(APPS).join(r"oschess.bridgex_0.1.0.0_x64__8wekyb3d8bbwe\engines\sf.exe"),
            Path::new(APPS).join(r"oschess.bridge_0.1.0.0_x64__otherpublisher\engines\sf.exe"),
            Path::new(APPS).join("oschess.bridge_8wekyb3d8bbwe"),
            PathBuf::from(r"C:\Engines\stockfish.exe"),
        ] {
            assert!(!now.owns(&other), "{}", other.display());
        }
    }

    #[test]
    fn chooses_the_carried_build_until_the_user_chooses_another() {
        let now = package("0.2.0.0");
        let carried = carried(&now);
        let everything = |_: &Path| true;
        assert_eq!(engine_to_choose(None, &carried, &now, everything), Some(carried.clone()));
        assert_eq!(engine_to_choose(Some(&carried), &carried, &now, everything), None);
        let own = PathBuf::from(r"C:\Engines\lc0.exe");
        assert_eq!(engine_to_choose(Some(&own), &carried, &now, everything), None, "the user's engine stays");
        assert_eq!(
            engine_to_choose(Some(&own), &carried, &now, |_| false),
            Some(carried.clone()),
            "a gone file does not"
        );
    }

    #[test]
    fn moves_from_an_earlier_version_s_build_to_this_one() {
        let now = package("0.2.0.0");
        let earlier = carried(&package("0.1.0.0"));
        // Windows may keep the earlier folder for a while after the update.
        assert_eq!(engine_to_choose(Some(&earlier), &carried(&now), &now, |_| true), Some(carried(&now)));
    }
}
