extern crate config;
extern crate futures;
extern crate gdk;
extern crate gtk;
extern crate glib;
extern crate mpd;
extern crate time;
extern crate tokio_core;
extern crate tokio_timer;

#[macro_use] extern crate relm;
#[macro_use] extern crate relm_derive;

use futures::Stream;
use gtk::{Builder, Continue, Inhibit};
use gtk::{WidgetExt, WindowExt, ButtonExt, ToggleButtonExt, ToValue};
use mpd::idle::{Idle, Subsystem};
use mpd::status::State;
use relm::{EventStream, Relm, RemoteRelm, Widget};
use std::env;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use time::{PreciseTime};
use tokio_core::reactor::Interval;
use tokio_timer::Timer;

const NOW_PLAYING_ID:       u32 = 0;
const NOW_PLAYING_ARTIST:   u32 = 1;
const NOW_PLAYING_ALBUM:    u32 = 2;
const NOW_PLAYING_TITLE:    u32 = 3;
const NOW_PLAYING_LENGTH:   u32 = 4;
const NOW_PLAYING_FILENAME: u32 = 5;
const NOW_PLAYING_WEIGHT:   u32 = 6;

#[derive(Clone)]
struct Model {
    mpd_ctrl: Arc<Mutex<mpd::Client>>,
    mpd_idle: Arc<Mutex<Option<mpd::Client>>>,
    status: mpd::Status,
    status_updated: PreciseTime,
    current_song: Option<mpd::Song>,
    last_song_position: Option<u32>,
}

#[derive(Clone, Debug)]
enum Ctrl {
    Prev,
    Pause(bool),
    Next,
    Seek(f64),
    Volume(i8),
    Repeat(bool),
    Shuffle(bool),
    Switch(u32),
}

#[derive(Debug, Msg)]
enum Msg {
    StartListening,
    Control(Ctrl),
    Update(Vec<Subsystem>),
    UpdateUi,
    UpdateNowPlaying,
    ScrollToCurSong,
    SetMainVisible(bool),
    Tick,
    Ping,
    Quit,
}

#[derive(Clone)]
struct Win {
    window:               gtk::Window,
    stream:               EventStream<Msg>,
    ctrl_prev:            UiElement<gtk::Button>,
    ctrl_next:            UiElement<gtk::Button>,
    ctrl_toggle:          UiElement<gtk::Button>,
    ctrl_repeat:          UiElement<gtk::ToggleButton>,
    ctrl_shuffle:         UiElement<gtk::ToggleButton>,
    adj_seek:             UiElement<gtk::Adjustment>,
    adj_volume:           UiElement<gtk::Adjustment>,
    disp_elapsed:         UiElement<gtk::Label>,
    disp_cursong:         UiElement<gtk::Label>,
    disp_duration:        UiElement<gtk::Label>,
    ctrl_togglemain:      UiElement<gtk::Button>,
    rev_main:             UiElement<gtk::Revealer>,
    sto_nowplaying:       gtk::ListStore,
    sto_nowplaying_iters: Arc<Mutex<Vec<gtk::TreeIter>>>,
    view_nowplaying:      gtk::TreeView,
}

#[derive(Clone)]
struct UiElement<T> {
    widget: T,
    signal: u64,
}

impl<T> UiElement<T>
where T: gtk::IsA<gtk::Object> {
    fn block_signal<F>(&self, f: F)
    where F: FnOnce(&T) {
        glib::signal_handler_block(&self.widget, self.signal);
        f(&self.widget);
        glib::signal_handler_unblock(&self.widget, self.signal);
    }
}

fn debug<T: std::fmt::Debug>(e: T) -> String {
    format!("{:?}", e)
}

fn format_time(dur: time::Duration) -> String {
    if dur.num_hours() > 0 {
        format!("{}:{:02}:{:02}",
                dur.num_hours(),
                dur.num_minutes() - dur.num_hours() * 60,
                dur.num_seconds() - dur.num_minutes() * 60)
    } else {
        format!("{:02}:{:02}",
                dur.num_minutes(),
                dur.num_seconds() - dur.num_minutes() * 60)
    }
}

impl Win {
    fn handle_result(&self, result: Result<(), String>) {
        match result {
            Ok(_) => (),
            Err(e) => println!("an error occurred: {}", e)
        }
    }

    fn handle_update(&self, subsystems: Vec<Subsystem>, model: &mut Model) {
        for subsystem in subsystems {
            match subsystem {
                Subsystem::Player |
                Subsystem::Mixer |
                Subsystem::Options => {
                    let mut mpd = model.mpd_ctrl.lock().unwrap();
                    model.last_song_position = model.status.song.map(|s| s.pos);
                    model.status = mpd.status().unwrap_or_default();
                    model.status_updated = PreciseTime::now();
                    model.current_song = mpd.currentsong().ok().unwrap_or_default();
                    self.update_ui(model);
                },
                _ => ()
            }
        }
    }

    fn control(&self, model: &mut Model, ctrl: Ctrl) -> Result<(), String> {
        let mut mpd = model.mpd_ctrl.lock().map_err(debug)?;

        use Ctrl::*;
        match ctrl {
            Prev       => mpd.prev(),
            Next       => mpd.next(),
            Pause(b)   => mpd.pause(b),
            Seek(f)    => mpd.rewind(f),
            Volume(i)  => mpd.volume(i),
            Repeat(b)  => mpd.repeat(b),
            Shuffle(b) => mpd.random(b),
            Switch(i)  => mpd.switch(i),
        }.map_err(debug)
    }

    fn update_ui(&self, model: &Model) {
        if let Some(cursong) = model.current_song.as_ref() {
            let artist = cursong.tags.get("Artist");
            let album = cursong.tags.get("Album");
            let title = cursong.title.as_ref();

            let label = match (artist, album, title) {
                (Some(artist), Some(album), Some(title))
                    => format!("{} → {}\n{}", artist, album, title),

                (Some(artist), None, Some(title))
                    => format!("{}\n{}", artist, title),

                _ => cursong.file.to_string()
            };

            self.disp_cursong.widget.set_label(&label);
        }

        self.ctrl_repeat.block_signal(|w| w.set_active(model.status.repeat));
        self.ctrl_shuffle.block_signal(|w| w.set_active(model.status.random));

        self.ctrl_toggle.widget.set_label(match model.status.state {
            State::Play => "pause",
            _           => "play_arrow"
        });

        self.adj_volume.block_signal(|a| a.set_value(model.status.volume as f64));
        self.adj_seek.block_signal(|a| {
            a.set_upper(model.status.duration
                        .or(model.status.time.map(|t| t.1))
                        .map(|dur| dur.num_milliseconds() as f64 / 1000.0)
                        .unwrap_or(0.0));

            a.set_value(model.status.elapsed
                        .or(model.status.time.map(|t| t.0))
                        .map(|el| el.num_milliseconds() as f64 / 1000.0)
                        .unwrap_or(0.0));
        });

        self.disp_elapsed.widget.set_label(&model.status.elapsed
                                                 .or(model.status.time.map(|t| t.1))
                                                 .map(format_time)
                                                 .unwrap_or("".into()));

        self.disp_duration.widget.set_label(&model.status.duration
                                                 .or(model.status.time.map(|t| t.1))
                                                 .map(format_time)
                                                 .unwrap_or("".into()));

        if model.status.song.map(|s| s.pos) // current song position
        .and_then(|p| model.last_song_position.map(|lp| lp != p)) // true if different from last song position
        .unwrap_or(false) {
            self.update_now_playing_cursong(&model);

            if self.view_nowplaying.get_selection().count_selected_rows() == 0 {
                self.stream.emit(Msg::ScrollToCurSong);
            }
        }
    }

    fn update_now_playing(&mut self, model: &Model) {
        {
            self.sto_nowplaying.clear();
            let mut iters = self.sto_nowplaying_iters.lock().unwrap();
            iters.clear();

            let mut mpd = model.mpd_ctrl.lock().unwrap();
            let queue = mpd.queue().unwrap();
            let empty_string = "".to_string();
            for ref song in queue.iter() {
                let tags   = &song.tags;

                let id       = &song.place.unwrap().id.0;
                let artist   = tags.get("Artist").unwrap_or(&empty_string);
                let album    = tags.get("Album").unwrap_or(&empty_string);
                let title    = song.title.as_ref().unwrap_or(&empty_string);
                let length   = &song.duration.map(|d| format_time(d)).unwrap_or("".to_string());
                let filename = &song.file;

                let iter = self.sto_nowplaying.insert_with_values(None,
                    &[ NOW_PLAYING_ID, NOW_PLAYING_ARTIST, NOW_PLAYING_ALBUM, NOW_PLAYING_TITLE,
                    NOW_PLAYING_LENGTH, NOW_PLAYING_FILENAME, NOW_PLAYING_WEIGHT ],
                    &[ id, artist, album, title, length, filename, &400 ]);

                iters.push(iter);
            }
        }

        self.scroll_to_current_song(&model);
        self.update_now_playing_cursong(&model);
    }

    fn update_now_playing_cursong(&self, model: &Model) {
        let iters = self.sto_nowplaying_iters.lock().unwrap();
        if iters.len() == 0 { return }

        if let Some(lastpos) = model.last_song_position {
            let iter = iters.get(lastpos as usize).unwrap();
            self.sto_nowplaying.set_value(&iter, NOW_PLAYING_WEIGHT, &400.to_value());
        }

        if let Some(song) = model.status.song {
            let iter = iters.get(song.pos as usize).unwrap();
            self.sto_nowplaying.set_value(&iter, NOW_PLAYING_WEIGHT, &700.to_value());
        }
    }

    fn scroll_to_current_song(&self, model: &Model) {
        if let Some(songpos) = model.status.song {
            let mut path = gtk::TreePath::new();
            path.append_index(songpos.pos as i32);
            self.view_nowplaying.scroll_to_cell(&path, None, true, 0.5, 0.0);
        }
    }

    fn set_main_visible(&self, visible: bool) {
        self.rev_main.widget.set_reveal_child(visible);
        self.ctrl_togglemain.widget.set_label(match visible {
            true  => "expand_less",
            false => "expand_more",
        });

        self.window.set_resizable(false);
        let window = self.window.clone();
        gtk::timeout_add(250, move || {
            window.set_resizable(true);
            Continue(false)
        });
    }

    fn tick(&self, model: &Model) {
        if let State::Play = model.status.state {
            let elapsed = model.status.elapsed.or(model.status.time.map(|t| t.1)).unwrap()
                + model.status_updated.to(PreciseTime::now());

            self.adj_seek.block_signal(|a| {
                a.set_value(elapsed.num_milliseconds() as f64 / 1000.0);
            });

            self.disp_elapsed.widget.set_label(&format_time(elapsed));
        }
    }
}

impl Widget for Win {
    type Model = Model;
    type ModelParam = config::Config;
    type Msg = Msg;
    type Root = gtk::Window;

    // Return the initial model.
    fn model(config: config::Config) -> Self::Model {
        let config_mpd = config.get_table("mpd").expect("config: mpd must be a table");

        let host = config_mpd.get("host").expect("config: missing mpd.host")
            .clone().into_str().expect("config: invalid mpd.host");
        let port = config_mpd.get("port").expect("config: missing mpd.host")
            .clone().into_int().expect("config: invalid mpd.port") as u16;

        let mut mpd_ctrl = mpd::Client::connect((host.as_str(), port)).expect("unable to connect to mpd");
        let mpd_idle = mpd::Client::connect((host.as_str(), port)).expect("unable to connect to mpd");

        let status = mpd_ctrl.status().unwrap_or_default();
        let status_updated = PreciseTime::now();
        let current_song = mpd_ctrl.currentsong().ok().unwrap_or_default();
        let mpd_ctrl = Arc::new(Mutex::new(mpd_ctrl));
        let mpd_idle = Arc::new(Mutex::new(Some(mpd_idle)));
        let last_song_position = None;

        Model {
            mpd_ctrl, mpd_idle,
            status, status_updated, current_song,
            last_song_position,
        }
    }

    // Return the root of this widget.
    fn root(&self) -> &Self::Root {
        &self.window
    }

    // The model may be updated when a message is received.
    // Widgets may also be updated in this function.
    fn update(&mut self, event: Msg, mut model: &mut Model) {
        match event {
            Msg::StartListening    => (), // async
            Msg::Control(ctrl)     => self.handle_result(self.control(&mut model, ctrl)),
            Msg::Update(subs)      => self.handle_update(subs, &mut model),
            Msg::UpdateUi          => self.update_ui(&model),
            Msg::UpdateNowPlaying  => self.update_now_playing(&model),
            Msg::ScrollToCurSong   => self.scroll_to_current_song(&model),
            Msg::SetMainVisible(b) => self.set_main_visible(b),
            Msg::Tick              => self.tick(&model),
            Msg::Ping              => model.mpd_ctrl.lock().unwrap().ping().unwrap(),
            Msg::Quit              => gtk::main_quit(),
        }
    }

    // Create the widgets.
    fn view(relm: &RemoteRelm<Self>, _model: &Self::Model) -> Self {
        // GTK+ widgets are used normally within a `Widget`.
        let builder = Builder::new();
        let _ = builder.add_from_string(include_str!("main.glade"));

        let window: gtk::Window = builder.get_object("window").unwrap();

        fn get_object<T>(b: &Builder, s: &'static str) -> UiElement<T>
        where T: gtk::IsA<gtk::Object> {
            UiElement {
                widget: b.get_object(s).expect("fatal bug: mismatching ui ids"),
                signal: 0,
            }
        }

        let mut ctrl_prev:       UiElement<gtk::Button>       = get_object(&builder, "control-prev");
        let mut ctrl_next:       UiElement<gtk::Button>       = get_object(&builder, "control-next");
        let mut ctrl_toggle:     UiElement<gtk::Button>       = get_object(&builder, "control-toggle");
        let mut ctrl_repeat:     UiElement<gtk::ToggleButton> = get_object(&builder, "control-repeat");
        let mut ctrl_shuffle:    UiElement<gtk::ToggleButton> = get_object(&builder, "control-shuffle");
        let mut adj_seek:        UiElement<gtk::Adjustment>   = get_object(&builder, "adjustment-seek");
        let mut adj_volume:      UiElement<gtk::Adjustment>   = get_object(&builder, "adjustment-volume");
        let     disp_elapsed:    UiElement<gtk::Label>        = get_object(&builder, "display-elapsed");
        let     disp_cursong:    UiElement<gtk::Label>        = get_object(&builder, "display-currentsong");
        let     disp_duration:   UiElement<gtk::Label>        = get_object(&builder, "display-duration");
        let mut ctrl_togglemain: UiElement<gtk::Button>       = get_object(&builder, "control-togglemain");
        let     rev_main:        UiElement<gtk::Revealer>     = get_object(&builder, "revealer-main");

        let screen = window.get_screen().unwrap();
        let css_provider = gtk::CssProvider::new();
        let _ = css_provider.load_from_data(include_str!("main.css"));
        gtk::StyleContext::add_provider_for_screen(&screen, &css_provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

        ctrl_prev.signal = connect!(relm, ctrl_prev.widget,
                 connect_clicked(_),
                 Msg::Control(Ctrl::Prev));

        ctrl_next.signal = connect!(relm, ctrl_next.widget,
                 connect_clicked(_),
                 Msg::Control(Ctrl::Next));

        ctrl_toggle.signal = connect!(relm, ctrl_toggle.widget,
                 connect_clicked(w),
                 Msg::Control(Ctrl::Pause(w.get_label().map(|s| s == "pause".to_string()).unwrap_or(false))));

        ctrl_repeat.signal = connect!(relm, ctrl_repeat.widget,
                 connect_toggled(w),
                 Msg::Control(Ctrl::Repeat(w.get_active())));

        ctrl_shuffle.signal = connect!(relm, ctrl_shuffle.widget,
                 connect_toggled(w),
                 Msg::Control(Ctrl::Shuffle(w.get_active())));

        adj_seek.signal = connect!(relm, adj_seek.widget,
                 connect_value_changed(adj),
                 Msg::Control(Ctrl::Seek(adj.get_value())));

        adj_volume.signal = connect!(relm, adj_volume.widget,
                 connect_value_changed(adj),
                 Msg::Control(Ctrl::Volume(adj.get_value() as i8)));

        ctrl_togglemain.signal = connect!(relm, ctrl_togglemain.widget,
                 connect_clicked(w),
                 Msg::SetMainVisible(w.get_label().map(|s| s == "expand_more".to_string()).unwrap_or(false)));

        // TODO: connect rev_main's notify::child-revealed to re-enable resizing on the window
        //       instead of the timeout hack

        let sto_nowplaying: gtk::ListStore = builder.get_object("store-now-playing").unwrap();
        let view_nowplaying: gtk::TreeView = builder.get_object("view-now-playing").unwrap();
        let sto_nowplaying_iters = Arc::new(Mutex::new(vec![]));

        connect!(relm, view_nowplaying, connect_row_activated(_, path, _),
                 Msg::Control(Ctrl::Switch(path.get_indices()[0] as u32)));

        connect!(relm, window, connect_delete_event(_, _) (Msg::Quit, Inhibit(false)));

        // select "Now playing" in the sidebar
        let sidebar: gtk::ListBox = builder.get_object("list-sidebar").unwrap();
        let first_row = sidebar.get_row_at_index(0).unwrap();
        sidebar.select_row(&first_row);

        window.resize(600, 120);
        window.show_all();

        let stream = relm.stream().clone();
        stream.emit(Msg::StartListening);
        stream.emit(Msg::UpdateUi);
        stream.emit(Msg::UpdateNowPlaying);

        Win {
            window, stream,
            ctrl_prev, ctrl_next, ctrl_toggle, ctrl_repeat, ctrl_shuffle,
            adj_seek, adj_volume,
            disp_elapsed, disp_cursong, disp_duration,
            ctrl_togglemain, rev_main,
            sto_nowplaying, sto_nowplaying_iters, view_nowplaying,
        }
    }

    // The next methods are optional.

    // Futures and streams can be connected to send a message when a value is ready.
    // However, since the tokio event loop runs in another thread, they cannot be
    // connected in the `update` function which is ran in the main thread.
    // Thus, they must be added in the `update_command()` method which is ran in
    // the tokio thread.
    fn update_command(relm: &Relm<Msg>, event: Msg, model: &mut Model) {
        match event {
            Msg::StartListening => {
                let (sx, rx) = mpsc::channel::<Vec<Subsystem>>();
                let mut mpd = model.mpd_idle.lock().unwrap().take().unwrap();
                let _thread = thread::spawn(move || {
                    loop {
                        let _ = sx.send(mpd.wait(&[]).unwrap());
                    }
                });

                let timer  = Interval::new(Duration::from_millis(30), relm.handle()).unwrap();
                let stream = timer.filter_map(move |_| rx.try_recv().ok());
                relm.connect_exec_ignore_err(stream, Msg::Update);
            },
            _ => ()
        }
    }

    // Futures and streams can be connected when the `Widget` is created in the
    // `subscriptions()` method.
    fn subscriptions(relm: &Relm<Msg>) {
        let timer = Timer::default().interval(Duration::from_millis(150));
        relm.connect_exec_ignore_err(timer, |_| Msg::Tick);

        let timer = Timer::default().interval(Duration::from_secs(10));
        relm.connect_exec_ignore_err(timer, |_| Msg::Ping);
    }
}

fn main() {
    let mut config_path = PathBuf::from(env::var("HOME").unwrap_or(".".to_string()));
    config_path.push(".config/vinyl/config.toml");

    let mut settings = config::Config::default()
        .merge(config::File::from_str(include_str!("../default_config.toml"), config::FileFormat::Toml))
        .unwrap();

    let _ = settings.merge(config::File::with_name(config_path.to_str().unwrap()));

    relm::run::<Win>(settings).unwrap();
}
