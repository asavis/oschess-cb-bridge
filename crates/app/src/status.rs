//! The bridge's state as the app shows it: the tray mark's colour and
//! tooltip, and the view the windows draw.

use bridge::snapshot::Snapshot;
use serde::Serialize;

use crate::i18n::Strings;

/// What stops the bridge from serving.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Problem {
    /// Another program listens on the port.
    PortBusy { port: u16 },
    /// The server did not start, or stopped; `reason` is in English.
    Stopped { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseView {
    pub id: String,
    pub name: String,
    pub format: String,
    /// The state as the API names it: `ready`, `missing`, `cloudOnly`,
    /// `downloading`, `opening`, `unsupported` or `unreadable`.
    pub state: String,
    pub records: Option<u32>,
    /// The bytes of its files, while they are kept in the cloud or downloaded.
    pub size: Option<u64>,
    pub progress: Option<Progress>,
}

/// A download's bytes on this computer and in all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Progress {
    pub present: u64,
    pub total: u64,
}

impl Progress {
    /// Whole percent, never 100 before the last byte.
    pub fn percent(self) -> u64 {
        if self.present >= self.total { 100 } else { (self.present * 100 / self.total).min(99) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub version: String,
    pub port: u16,
    pub problem: Option<Problem>,
    /// The databases on the list: one that left it is not shown.
    pub databases: Vec<DatabaseView>,
    /// [`View::tray`]'s name, for the windows to show the same state.
    pub mark: &'static str,
    /// Whether the bridge has work a restart would lose (#61), for an update
    /// to wait for. Not shown in the windows.
    #[serde(skip)]
    pub busy: bool,
}

/// The tray mark's colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tray {
    /// Every listed database that the bridge can serve is ready.
    Ready,
    /// The bridge serves, but a database needs a look or is on its way:
    /// downloading, opening, unreadable (damaged or locked) or not found.
    Attention,
    /// The bridge does not serve at all until this is fixed: a port in use, or
    /// a server that stopped.
    Problem,
}

impl Tray {
    pub fn name(self) -> &'static str {
        match self {
            Tray::Ready => "ready",
            Tray::Attention => "attention",
            Tray::Problem => "problem",
        }
    }
}

/// The taskbar's theme, which the tray mark follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
}

impl Theme {
    pub fn name(self) -> &'static str {
        match self {
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }
}

impl View {
    /// The view of a serving bridge, or of one whose server stopped.
    pub fn of(snapshot: &Snapshot) -> View {
        View {
            version: snapshot.version.to_string(),
            port: snapshot.port,
            problem: snapshot.stopped.clone().map(|reason| Problem::Stopped { reason }),
            databases: snapshot
                .databases
                .iter()
                .filter(|d| d.listed)
                .map(|d| DatabaseView {
                    id: d.id.clone(),
                    name: d.name.clone(),
                    format: d.format.to_string(),
                    state: d.state.name().to_string(),
                    records: d.records,
                    size: d.size,
                    progress: d.progress.map(|(present, total)| Progress { present, total }),
                })
                .collect(),
            mark: "",
            busy: !snapshot.work.is_empty(),
        }
        .marked()
    }

    /// The view of a bridge that could not start.
    pub fn failed(version: &str, port: u16, problem: Problem) -> View {
        View {
            version: version.to_string(),
            port,
            problem: Some(problem),
            databases: Vec::new(),
            mark: "",
            busy: false,
        }
        .marked()
    }

    /// The view with its `mark` set from its state.
    pub fn marked(mut self) -> View {
        self.mark = self.tray().name();
        self
    }

    fn counting(&self, state: &str) -> usize {
        self.databases.iter().filter(|d| d.state == state).count()
    }

    pub fn tray(&self) -> Tray {
        if self.problem.is_some() {
            Tray::Problem
        } else if ["downloading", "opening", "unreadable", "missing"].iter().any(|s| self.counting(s) > 0) {
            Tray::Attention
        } else {
            Tray::Ready
        }
    }

    /// The tooltip, which names the state in words: the colour alone is not
    /// enough for everyone.
    pub fn tooltip(&self, strings: &Strings) -> String {
        match &self.problem {
            Some(Problem::PortBusy { port }) => strings.fill("tray.portBusy", &[("port", &port.to_string())]),
            Some(Problem::Stopped { .. }) => strings.get("tray.stopped").to_string(),
            None if self.counting("downloading") > 0 => {
                let first = self.databases.iter().find_map(|d| d.progress.filter(|_| d.state == "downloading"));
                match first {
                    Some(p) => strings.fill("tray.downloadingPercent", &[("percent", &p.percent().to_string())]),
                    None => strings.get("tray.downloading").to_string(),
                }
            }
            None if self.counting("opening") > 0 => strings.get("tray.opening").to_string(),
            None if self.counting("unreadable") > 0 => {
                strings.plural("tray.unreadable", self.counting("unreadable") as u64, &[])
            }
            None if self.counting("missing") > 0 => {
                strings.plural("tray.missing", self.counting("missing") as u64, &[])
            }
            None => match self.counting("ready") {
                0 if self.databases.is_empty() => strings.get("tray.none").to_string(),
                0 => strings.get("tray.noneReady").to_string(),
                n => strings.plural("tray.ready", n as u64, &[]),
            },
        }
    }
}

/// The tray icon's size in pixels for a display scale: 16 px at 100 %, and
/// the drawn size nearest to 16 × scale above that.
pub fn icon_size(scale: f64) -> u32 {
    let want = 16.0 * scale;
    [16, 20, 24, 32]
        .into_iter()
        .min_by(|a, b| (*a as f64 - want).abs().total_cmp(&(*b as f64 - want).abs()))
        .unwrap_or(16)
}

/// The tray icon's file in `icons/tray`, without its folder.
pub fn icon_file(theme: Theme, tray: Tray, size: u32) -> String {
    format!("{}-{}-{size}.png", theme.name(), tray.name())
}

#[cfg(test)]
mod tests {
    use bridge::catalog::State;
    use bridge::snapshot::Database;

    use super::*;
    use crate::i18n::Lang;

    fn view(states: &[&str]) -> View {
        View {
            version: "0.1.0".into(),
            port: 39581,
            problem: None,
            databases: states
                .iter()
                .enumerate()
                .map(|(i, s)| DatabaseView {
                    id: format!("{i:016x}"),
                    name: format!("Base {i}"),
                    format: "2cbh".into(),
                    state: s.to_string(),
                    records: (*s == "ready").then_some(10),
                    size: None,
                    progress: None,
                })
                .collect(),
            mark: "",
            busy: false,
        }
        .marked()
    }

    #[test]
    fn the_mark_and_the_tooltip_follow_the_state() {
        let uk = Strings::new(Lang::Uk);
        let cases = [
            (view(&["ready", "ready", "unsupported"]), Tray::Ready, "oschess міст — 2 бази готові"),
            (view(&["ready"]), Tray::Ready, "oschess міст — 1 база готова"),
            (view(&["ready"; 5]), Tray::Ready, "oschess міст — 5 баз готові"),
            (view(&[]), Tray::Ready, "oschess міст — баз поки немає"),
            (view(&["cloudOnly", "unsupported"]), Tray::Ready, "oschess міст — готових баз немає"),
            (view(&["ready", "cloudOnly"]), Tray::Ready, "oschess міст — 1 база готова"),
            (view(&["ready", "unreadable"]), Tray::Attention, "oschess міст — 1 база не відкривається"),
            (view(&["unreadable", "unreadable", "missing"]), Tray::Attention, "oschess міст — 2 бази не відкриваються"),
            (view(&["ready", "missing"]), Tray::Attention, "oschess міст — 1 базу не знайдено"),
            (view(&["missing"; 5]), Tray::Attention, "oschess міст — 5 баз не знайдено"),
            (view(&["unreadable", "downloading"]), Tray::Attention, "oschess міст — база завантажується"),
            (view(&["ready", "downloading"]), Tray::Attention, "oschess міст — база завантажується"),
            (view(&["opening"]), Tray::Attention, "oschess міст — база відкривається"),
            (downloading(420, 1000), Tray::Attention, "oschess міст — завантаження бази: 42 %"),
            (
                View::failed("0.1.0", 39581, Problem::PortBusy { port: 39581 }),
                Tray::Problem,
                "oschess міст — порт 39581 зайнятий",
            ),
            (
                View { problem: Some(Problem::Stopped { reason: "x".into() }), ..view(&["ready"]) }.marked(),
                Tray::Problem,
                "oschess міст — не працює",
            ),
        ];
        for (view, tray, tooltip) in cases {
            assert_eq!((view.tray(), view.tooltip(&uk).as_str()), (tray, tooltip), "{view:?}");
            assert_eq!(view.mark, tray.name());
        }
        let en = Strings::new(Lang::En);
        assert_eq!(view(&["ready", "ready"]).tooltip(&en), "oschess bridge — 2 databases ready");
        assert_eq!(view(&["unreadable"]).tooltip(&en), "oschess bridge — 1 database cannot be opened");
        assert_eq!(view(&["missing", "missing"]).tooltip(&en), "oschess bridge — 2 databases not found");
    }

    #[test]
    fn a_snapshot_becomes_a_view() {
        let snapshot = Snapshot {
            version: "0.1.0",
            port: 40000,
            stopped: None,
            databases: vec![Database {
                id: "0123456789abcdef".into(),
                name: "Mega".into(),
                format: "2cbh",
                state: State::Ready,
                records: Some(7),
                size: None,
                progress: None,
                listed: true,
            }],
            work: Vec::new(),
        };
        let view = View::of(&snapshot);
        assert_eq!(view.problem, None);
        let json = serde_json::to_string(&view).unwrap();
        assert_eq!(
            json,
            r#"{"version":"0.1.0","port":40000,"problem":null,"databases":[{"id":"0123456789abcdef","name":"Mega","format":"2cbh","state":"ready","records":7,"size":null,"progress":null}],"mark":"ready"}"#
        );
        let stopped = View::of(&Snapshot { stopped: Some("accept failed".into()), ..snapshot });
        assert_eq!(stopped.problem, Some(Problem::Stopped { reason: "accept failed".into() }));
        assert_eq!(stopped.mark, "problem");
        assert_eq!(serde_json::to_string(&Problem::PortBusy { port: 1 }).unwrap(), r#"{"kind":"portBusy","port":1}"#);
    }

    /// A view with one database downloading, `present` of `total` bytes.
    fn downloading(present: u64, total: u64) -> View {
        let mut view = view(&["downloading"]);
        view.databases[0].size = Some(total);
        view.databases[0].progress = Some(Progress { present, total });
        view.marked()
    }

    #[test]
    fn download_percent() {
        let p = |present, total| Progress { present, total }.percent();
        assert_eq!(
            [p(0, 10), p(1, 3), p(42, 100), p(999, 1000), p(1000, 1000), p(0, 0), p(12, 10)],
            [0, 33, 42, 99, 100, 100, 100]
        );
        let json = serde_json::to_string(&downloading(5, 10).databases[0]).unwrap();
        assert!(json.ends_with(r#""size":10,"progress":{"present":5,"total":10}}"#), "{json}");
    }

    #[test]
    fn icons_by_scale_theme_and_state() {
        assert_eq!([1.0, 1.25, 1.5, 1.75, 2.0, 3.0].map(icon_size), [16, 20, 24, 24, 32, 32]);
        assert_eq!(icon_file(Theme::Dark, Tray::Attention, 24), "dark-attention-24.png");
        let icons = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("icons/tray");
        for theme in [Theme::Light, Theme::Dark] {
            for tray in [Tray::Ready, Tray::Attention, Tray::Problem] {
                for size in [16, 20, 24, 32] {
                    let bytes = std::fs::read(icons.join(icon_file(theme, tray, size))).unwrap();
                    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
                    let (w, h) = (
                        u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
                        u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
                    );
                    assert_eq!((w, h), (size, size));
                }
            }
        }
    }
}
