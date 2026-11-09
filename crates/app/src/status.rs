//! The bridge's state as the app shows it: the tray mark's colour and
//! tooltip, and the view the windows draw.

use std::time::{Duration, Instant};

use bridge::catalog::State;
use bridge::snapshot::{Pulse, Snapshot};
use bridge::version::shown;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

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
    /// Written for the windows by the name the API gives it: `ready`,
    /// `opening`, `missing`, `cloudOnly`, `downloading`, `unsupported` or
    /// `unreadable`.
    #[serde(serialize_with = "state_name")]
    pub state: State,
    pub records: Option<u32>,
    /// The bytes of its files, while they are kept in the cloud or downloaded.
    pub size: Option<u64>,
    pub progress: Option<Progress>,
}

fn state_name<S: Serializer>(state: &State, out: S) -> Result<S::Ok, S::Error> {
    out.serialize_str(state.name())
}

/// A download's bytes on this computer and in all. The windows get its
/// [`Progress::percent`] with them and show that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

impl Serialize for Progress {
    fn serialize<S: Serializer>(&self, out: S) -> Result<S::Ok, S::Error> {
        let mut fields = out.serialize_struct("Progress", 3)?;
        fields.serialize_field("present", &self.present)?;
        fields.serialize_field("total", &self.total)?;
        fields.serialize_field("percent", &self.percent())?;
        fields.end()
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
    /// A paired browser has reached the bridge since it started: the
    /// first-run window stops waiting for the connection.
    pub connected: bool,
    /// Whether the databases were looked at: until then `databases` says
    /// nothing, and a window must not read it as «no databases» (#365).
    pub known: bool,
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
            version: shown(snapshot.version).to_string(),
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
                    state: d.state,
                    records: d.records,
                    size: d.size,
                    progress: d.progress.map(|(present, total)| Progress { present, total }),
                })
                .collect(),
            connected: snapshot.served,
            known: true,
            mark: "",
            busy: !snapshot.work.is_empty(),
        }
        .marked()
    }

    /// The view of a bridge that serves and has looked at no database yet
    /// (#365): at the start none is touched until someone uses the bridge.
    pub fn serving(version: &str, port: u16) -> View {
        View {
            version: shown(version).to_string(),
            port,
            problem: None,
            databases: Vec::new(),
            connected: false,
            known: false,
            mark: "",
            busy: false,
        }
        .marked()
    }

    /// The view of a bridge that could not start.
    pub fn failed(version: &str, port: u16, problem: Problem) -> View {
        View {
            version: shown(version).to_string(),
            port,
            problem: Some(problem),
            databases: Vec::new(),
            connected: false,
            known: true,
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

    fn counting(&self, state: State) -> usize {
        self.databases.iter().filter(|d| d.state == state).count()
    }

    pub fn tray(&self) -> Tray {
        if self.problem.is_some() {
            Tray::Problem
        } else if [State::Downloading, State::Opening, State::Unreadable, State::Missing]
            .into_iter()
            .any(|s| self.counting(s) > 0)
        {
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
            None if !self.known => strings.get("tray.running").to_string(),
            None if self.counting(State::Downloading) > 0 => {
                let first = self.databases.iter().find_map(|d| d.progress.filter(|_| d.state == State::Downloading));
                match first {
                    Some(p) => strings.fill("tray.downloadingPercent", &[("percent", &p.percent().to_string())]),
                    None => strings.get("tray.downloading").to_string(),
                }
            }
            None if self.counting(State::Opening) > 0 => strings.get("tray.opening").to_string(),
            None if self.counting(State::Unreadable) > 0 => {
                strings.plural("tray.unreadable", self.counting(State::Unreadable) as u64, &[])
            }
            None if self.counting(State::Missing) > 0 => {
                strings.plural("tray.missing", self.counting(State::Missing) as u64, &[])
            }
            None => match self.counting(State::Ready) {
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

/// A snapshot looks at every database's files, which with hundreds of
/// databases costs a quarter of a second of processor time, and nothing needs
/// one while nobody uses the bridge: `/v1/status` and `/v1/databases` look for
/// themselves (#365). These are the least times between snapshots:
///
/// while a window shows the state and the bridge works, for its progress;
pub const WINDOW_WORK_EVERY: Duration = Duration::from_secs(1);
/// while a window shows the state and the bridge has nothing to do;
pub const WINDOW_EVERY: Duration = Duration::from_secs(5);
/// while the bridge works and no window is open;
pub const WORK_EVERY: Duration = Duration::from_secs(10);
/// when only the count of work begun and ended changed;
pub const CHANGE_GAP: Duration = Duration::from_secs(5);
/// after a hover on the tray mark;
pub const HOVER_GAP: Duration = Duration::from_secs(10);
/// and while the web app sends requests, each of which looks for itself.
pub const REQUESTS_GAP: Duration = Duration::from_secs(60);

/// How often the status thread ticks while a window is open or the bridge
/// works, and otherwise. A tick reads the pulse and the taskbar's theme: it
/// looks at no database.
pub const TICK: Duration = Duration::from_secs(1);
pub const IDLE_TICK: Duration = Duration::from_secs(5);

/// What asked the status thread for a look at the databases, the stronger
/// last (#365).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Asked {
    #[default]
    Nothing,
    /// The pointer came over the tray mark, whose tooltip is about to show.
    Hover,
    /// A window that shows the state is being opened.
    Window,
}

/// When the status thread takes the bridge's snapshot (#365): when a window
/// is being opened; after a hover on the mark; at once when the bridge's
/// [`Pulse`] says something the user would see changed (a build starts or
/// waits again, an analysis begins or ends, the server stops, a browser is
/// first served, or the bridge starts or stops serving); while a window is
/// open or the bridge works, at the pace above; and after work began or ended
/// or requests came, at most that often. Otherwise never: an idle bridge
/// touches no file, from its start on.
#[derive(Default)]
pub struct Pace {
    /// When the last snapshot was taken.
    at: Option<Instant>,
    /// The pulse of the last snapshot, or of the first tick.
    seen: Option<Option<Pulse>>,
}

impl Pace {
    /// Whether to take a snapshot at `now`, with a window open (`shown`) or
    /// not, what `asked` for one, and the bridge's `pulse` (none when it does
    /// not serve); noted as taken when it is.
    pub fn due(&mut self, now: Instant, shown: bool, asked: Asked, pulse: Option<Pulse>) -> bool {
        let seen = *self.seen.get_or_insert(pulse);
        let since = self.at.map_or(Duration::MAX, |at| now.saturating_duration_since(at));
        let working = pulse.is_some_and(|p| p.working());
        let plain = |p: Pulse| Pulse { changes: 0, requests: 0, ..p };
        let (urgent, counted, requested) = match (seen, pulse) {
            (Some(a), Some(b)) => (plain(a) != plain(b), a.changes != b.changes, a.requests != b.requests),
            (None, None) => (false, false, false),
            _ => (true, false, false),
        };
        let due = match asked {
            Asked::Window => true,
            Asked::Hover if since >= HOVER_GAP => true,
            _ if urgent => true,
            _ if shown => since >= if working { WINDOW_WORK_EVERY } else { WINDOW_EVERY },
            _ if working => since >= WORK_EVERY,
            _ if counted => since >= CHANGE_GAP,
            _ if requested => since >= REQUESTS_GAP,
            _ => false,
        };
        if due {
            (self.at, self.seen) = (Some(now), Some(pulse));
        }
        due
    }
}

/// Snapshots and views for this crate's tests, built in one place so that a
/// field added to either is filled in once.
#[cfg(test)]
pub(crate) mod testing {
    use bridge::catalog::State;
    use bridge::snapshot::Snapshot;

    use super::{DatabaseView, View};

    /// A serving bridge's snapshot: no database, no work, and no browser
    /// served yet.
    pub(crate) fn snapshot() -> Snapshot {
        Snapshot {
            version: "1.2611.801",
            port: 39581,
            stopped: None,
            databases: Vec::new(),
            work: Vec::new(),
            served: false,
        }
    }

    /// The view of a serving bridge with a database in each of `states`, a
    /// ready one of 10 records.
    pub(crate) fn view(states: &[State]) -> View {
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
                    state: *s,
                    records: (*s == State::Ready).then_some(10),
                    size: None,
                    progress: None,
                })
                .collect(),
            connected: false,
            known: true,
            mark: "",
            busy: false,
        }
        .marked()
    }
}

#[cfg(test)]
mod tests {
    use bridge::catalog::State::{CloudOnly, Downloading, Missing, Opening, Ready, Unreadable, Unsupported};
    use bridge::snapshot::Database;

    use super::testing::view;
    use super::*;
    use crate::i18n::Lang;

    fn nothing_looks(
        pace: &mut Pace,
        at: impl Fn(u64) -> Instant,
        seconds: std::ops::RangeInclusive<u64>,
        pulse: Pulse,
    ) {
        for s in seconds {
            assert!(!pace.due(at(s), false, Asked::Nothing, Some(pulse)), "at second {s}");
        }
    }

    /// An idle bridge is looked at by nobody: from its start on, for as long
    /// as nothing asks (#365).
    #[test]
    fn an_idle_bridge_takes_no_snapshot() {
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let mut pace = Pace::default();
        nothing_looks(&mut pace, at, 0..=86_400, Pulse::default());
    }

    /// A window being opened takes one at once; a window that stays open every
    /// five seconds, and each second while the bridge works, for its progress.
    #[test]
    fn a_window_takes_snapshots_at_its_pace() {
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let quiet = Some(Pulse::default());
        let mut pace = Pace::default();
        assert!(pace.due(at(0), false, Asked::Window, quiet), "being opened");
        let taken: Vec<u64> = (1..=12).filter(|&s| pace.due(at(s), true, Asked::Nothing, quiet)).collect();
        assert_eq!(taken, [5, 10], "open");
        let downloading = Some(Pulse { downloading: true, ..Pulse::default() });
        assert!(pace.due(at(13), true, Asked::Nothing, downloading), "the work began");
        assert!(pace.due(at(14), true, Asked::Nothing, downloading), "each second");
        assert!(!pace.due(at(14), true, Asked::Nothing, downloading), "not twice in one");
    }

    /// A hover looks at most every ten seconds, as the pointer may pass often.
    #[test]
    fn a_hover_takes_a_snapshot_at_most_every_ten_seconds() {
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let quiet = Some(Pulse::default());
        let mut pace = Pace::default();
        assert!(pace.due(at(0), false, Asked::Hover, quiet));
        assert!(!pace.due(at(9), false, Asked::Hover, quiet));
        assert!(pace.due(at(10), false, Asked::Hover, quiet));
    }

    /// What the user would see at once takes one at once; running work every
    /// ten seconds; only the count of work every five at most; requests once
    /// a minute at most, each change seen once.
    #[test]
    fn the_bridge_side_takes_snapshots_with_measure() {
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let quiet = Pulse::default();
        for changed in [
            Pulse { indexing: true, ..quiet },
            Pulse { headers: true, ..quiet },
            Pulse { stopped: true, ..quiet },
            Pulse { analysing: true, ..quiet },
            Pulse { served: true, ..quiet },
        ] {
            let mut pace = Pace::default();
            assert!(!pace.due(at(0), false, Asked::Nothing, Some(quiet)));
            assert!(pace.due(at(1), false, Asked::Nothing, Some(changed)), "{changed:?}");
        }
        let mut pace = Pace::default();
        assert!(!pace.due(at(0), false, Asked::Nothing, Some(quiet)));
        assert!(pace.due(at(1), false, Asked::Nothing, None), "the bridge no longer serves");
        assert!(!pace.due(at(2), false, Asked::Nothing, None));

        let working = Pulse { indexing: true, ..quiet };
        let mut pace = Pace::default();
        assert!(pace.due(at(0), false, Asked::Window, Some(working)));
        let taken: Vec<u64> = (1..=25).filter(|&s| pace.due(at(s), false, Asked::Nothing, Some(working))).collect();
        assert_eq!(taken, [10, 20], "while it works");

        let counted = Pulse { changes: 2, ..quiet };
        let mut pace = Pace::default();
        assert!(pace.due(at(0), false, Asked::Window, Some(quiet)));
        assert!(!pace.due(at(4), false, Asked::Nothing, Some(counted)));
        assert!(pace.due(at(5), false, Asked::Nothing, Some(counted)), "the count changed");
        nothing_looks(&mut pace, at, 6..=600, counted);

        let requested = Pulse { requests: 7, ..quiet };
        let mut pace = Pace::default();
        assert!(pace.due(at(0), false, Asked::Window, Some(quiet)));
        nothing_looks(&mut pace, at, 1..=59, requested);
        assert!(pace.due(at(60), false, Asked::Nothing, Some(requested)), "requests came");
        nothing_looks(&mut pace, at, 61..=600, requested);
    }

    /// A view that knows no database yet says the bridge runs: it does not
    /// claim there are none (#365).
    #[test]
    fn a_view_that_looked_at_nothing_says_the_bridge_runs() {
        let uk = Strings::new(Lang::Uk);
        let view = View::serving("0.1.0", 39581);
        assert_eq!(view.tray(), Tray::Ready);
        assert_eq!(view.tooltip(&uk), uk.get("tray.running"));
        assert_ne!(view.tooltip(&uk), uk.get("tray.none"));
    }

    #[test]
    fn the_mark_and_the_tooltip_follow_the_state() {
        let uk = Strings::new(Lang::Uk);
        let cases = [
            (view(&[Ready, Ready, Unsupported]), Tray::Ready, "oschess міст — 2 бази готові"),
            (view(&[Ready]), Tray::Ready, "oschess міст — 1 база готова"),
            (view(&[Ready; 5]), Tray::Ready, "oschess міст — 5 баз готові"),
            (view(&[]), Tray::Ready, "oschess міст — баз поки немає"),
            (view(&[CloudOnly, Unsupported]), Tray::Ready, "oschess міст — готових баз немає"),
            (view(&[Ready, CloudOnly]), Tray::Ready, "oschess міст — 1 база готова"),
            (view(&[Ready, Unreadable]), Tray::Attention, "oschess міст — 1 база не відкривається"),
            (view(&[Unreadable, Unreadable, Missing]), Tray::Attention, "oschess міст — 2 бази не відкриваються"),
            (view(&[Ready, Missing]), Tray::Attention, "oschess міст — 1 базу не знайдено"),
            (view(&[Missing; 5]), Tray::Attention, "oschess міст — 5 баз не знайдено"),
            (view(&[Unreadable, Downloading]), Tray::Attention, "oschess міст — база завантажується"),
            (view(&[Ready, Downloading]), Tray::Attention, "oschess міст — база завантажується"),
            (view(&[Opening]), Tray::Attention, "oschess міст — база відкривається"),
            (downloading(420, 1000), Tray::Attention, "oschess міст — завантаження бази: 42 %"),
            (
                View::failed("0.1.0", 39581, Problem::PortBusy { port: 39581 }),
                Tray::Problem,
                "oschess міст — порт 39581 зайнятий",
            ),
            (
                View { problem: Some(Problem::Stopped { reason: "x".into() }), ..view(&[Ready]) }.marked(),
                Tray::Problem,
                "oschess міст — не працює",
            ),
        ];
        for (view, tray, tooltip) in cases {
            assert_eq!((view.tray(), view.tooltip(&uk).as_str()), (tray, tooltip), "{view:?}");
            assert_eq!(view.mark, tray.name());
        }
        let en = Strings::new(Lang::En);
        assert_eq!(view(&[Ready, Ready]).tooltip(&en), "oschess bridge — 2 databases ready");
        assert_eq!(view(&[Unreadable]).tooltip(&en), "oschess bridge — 1 database cannot be opened");
        assert_eq!(view(&[Missing, Missing]).tooltip(&en), "oschess bridge — 2 databases not found");
    }

    #[test]
    fn a_snapshot_becomes_a_view() {
        let snapshot = Snapshot {
            port: 40000,
            databases: vec![Database {
                id: "0123456789abcdef".into(),
                name: "Mega".into(),
                format: "2cbh",
                state: State::Ready,
                records: Some(7),
                generation: Some(1),
                size: None,
                progress: None,
                listed: true,
                writable: false,
                folder: Vec::new(),
                created: None,
                modified: None,
            }],
            served: true,
            ..testing::snapshot()
        };
        let view = View::of(&snapshot);
        assert_eq!(view.problem, None);
        assert_eq!(View::of(&Snapshot { version: "1.4.0", ..testing::snapshot() }).version, "dev", "a build by hand");
        assert_eq!(View::serving("1.4.0", 1).version, "dev");
        assert_eq!(View::failed("1.4.0", 1, Problem::PortBusy { port: 1 }).version, "dev");
        let json = serde_json::to_string(&view).unwrap();
        assert_eq!(
            json,
            r#"{"version":"1.2611.801","port":40000,"problem":null,"databases":[{"id":"0123456789abcdef","name":"Mega","format":"2cbh","state":"ready","records":7,"size":null,"progress":null}],"connected":true,"known":true,"mark":"ready"}"#
        );
        let stopped = View::of(&Snapshot { stopped: Some("accept failed".into()), ..snapshot });
        assert_eq!(stopped.problem, Some(Problem::Stopped { reason: "accept failed".into() }));
        assert_eq!(stopped.mark, "problem");
        assert_eq!(serde_json::to_string(&Problem::PortBusy { port: 1 }).unwrap(), r#"{"kind":"portBusy","port":1}"#);
    }

    /// The windows get each state by the name the API gives it (#69).
    #[test]
    fn the_windows_get_the_api_names() {
        let states = [Ready, Opening, Missing, CloudOnly, Downloading, Unsupported, Unreadable];
        let json = serde_json::to_value(view(&states)).unwrap();
        let names: Vec<&str> =
            json["databases"].as_array().unwrap().iter().map(|d| d["state"].as_str().unwrap()).collect();
        assert_eq!(names, ["ready", "opening", "missing", "cloudOnly", "downloading", "unsupported", "unreadable"]);
    }

    /// A view with one database downloading, `present` of `total` bytes.
    fn downloading(present: u64, total: u64) -> View {
        let mut view = view(&[Downloading]);
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
        assert!(json.ends_with(r#""size":10,"progress":{"present":5,"total":10,"percent":50}}"#), "{json}");
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
