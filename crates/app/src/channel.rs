//! Where this copy of the bridge came from (#112). The NSIS installer from
//! GitHub gives the direct channel: the app updates itself from GitHub and
//! starts with Windows through the Run key. The Microsoft Store installs the
//! same executable in an MSIX package: the app updates it through the Store
//! (#153), and Windows starts it through the package's startup task. The app
//! tells the two apart by asking Windows for its package identity.

/// The startup task the package manifest declares; «Start with Windows»
/// enables it in the Store channel.
pub const STARTUP_TASK: &str = "oschessBridgeStartup";

/// The bridge's page in the Microsoft Store app (#153), which «Check for
/// updates» opens when Windows would not install silently and could not show
/// its own update dialogs either (#232).
pub const STORE_PAGE: &str = "ms-windows-store://pdp/?productid=9P65J7XR0RPZ";

/// Where the app came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Direct,
    Store,
}

impl Channel {
    pub fn is_store(self) -> bool {
        self == Channel::Store
    }
}
