extern crate core;
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
use gtk::{Builder, Inhibit};
use gtk::{WidgetExt, ButtonExt, ToggleButtonExt};
use mpd::idle::{Idle, Subsystem};
use mpd::status::State;
use relm::{Relm, RemoteRelm, Widget};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use time::{PreciseTime};
use tokio_core::reactor::Interval;
use tokio_timer::Timer;

#[derive(Clone)]
struct Model {
    mpd_ctrl: Arc<Mutex<mpd::Client>>,
    mpd_idle: Arc<Mutex<Option<mpd::Client>>>,
    status: mpd::Status,
    status_updated: PreciseTime,
    current_song: Option<mpd::Song>,
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
}

#[derive(Debug, Msg)]
enum Msg {
    StartListening,
    Control(Ctrl),
    Update(Vec<Subsystem>),
    UpdateUi,
    Tick,
    Ping,
    Quit,
}

#[derive(Clone)]
struct Win {
    window: gtk::Window,
    elems:  Rc<UiElements>,
}

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

struct UiElements {
    ctrl_prev:    UiElement<gtk::Button>,
    ctrl_next:    UiElement<gtk::Button>,
    ctrl_toggle:  UiElement<gtk::Button>,
    ctrl_repeat:  UiElement<gtk::ToggleButton>,
    ctrl_shuffle: UiElement<gtk::ToggleButton>,
    adj_seek:     UiElement<gtk::Adjustment>,
    adj_volume:   UiElement<gtk::Adjustment>,
    disp_cursong: UiElement<gtk::Label>,
}

fn debug<T: std::fmt::Debug>(e: T) -> String {
    format!("{:?}", e)
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
        }.map_err(debug)
    }

    fn update_ui(&self, model: &Model) {
        if let Some(cursong) = model.current_song.as_ref() {
            let artist = cursong.tags.get("Artist");
            let title = cursong.title.as_ref();

            let label = if let (Some(artist), Some(title)) = (artist, title) {
                format!("{}\n{}", artist, title)
            } else {
                cursong.file.to_string()
            };

            self.elems.disp_cursong.widget.set_label(&label);
        }

        self.elems.ctrl_repeat.block_signal(|w| w.set_active(model.status.repeat));
        self.elems.ctrl_shuffle.block_signal(|w| w.set_active(model.status.random));

        self.elems.ctrl_toggle.widget.set_label(match model.status.state {
            State::Play => "pause",
            _           => "play_arrow"
        });

        self.elems.adj_volume.block_signal(|a| a.set_value(model.status.volume as f64));
        self.elems.adj_seek.block_signal(|a| {
            a.set_upper(match (model.status.duration, model.status.time) {
                (Some(duration), _)             => duration.num_milliseconds() as f64 / 1000.0,
                (_, Some((_elapsed, duration))) => duration.num_seconds() as f64,
                _                               => 0.0,
            });

            a.set_value(match (model.status.elapsed, model.status.time) {
                (Some(elapsed), _)              => elapsed.num_milliseconds() as f64 / 1000.0,
                (_, Some((elapsed, _duration))) => elapsed.num_seconds() as f64,
                _                               => 0.0,
            });
        });
    }

    fn tick(&self, model: &Model) {
        if let State::Play = model.status.state {
            self.elems.adj_seek.block_signal(|a| {
                a.set_value(model.status.elapsed.unwrap().num_milliseconds() as f64 / 1000.0
                            + model.status_updated.to(PreciseTime::now()).num_milliseconds() as f64 / 1000.0);
            });
        }
    }
}

impl Widget for Win {
    type Model = Model;
    type ModelParam = (String, u16);
    type Msg = Msg;
    type Root = gtk::Window;

    // Return the initial model.
    fn model((address, port): Self::ModelParam) -> Self::Model {
        let mut mpd_ctrl = mpd::Client::connect((address.as_str(), port)).expect("unable to connect to mpd");
        let mpd_idle = mpd::Client::connect((address.as_str(), port)).expect("unable to connect to mpd");

        let status = mpd_ctrl.status().unwrap_or_default();
        let status_updated = PreciseTime::now();
        let current_song = mpd_ctrl.currentsong().ok().unwrap_or_default();
        let mpd_ctrl = Arc::new(Mutex::new(mpd_ctrl));
        let mpd_idle = Arc::new(Mutex::new(Some(mpd_idle)));

        Model {
            mpd_ctrl, mpd_idle, status, status_updated, current_song
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
            Msg::StartListening => (), // async
            Msg::Control(ctrl)  => self.handle_result(self.control(&mut model, ctrl)),
            Msg::Update(subs)   => self.handle_update(subs, &mut model),
            Msg::UpdateUi       => self.update_ui(&model),
            Msg::Tick           => self.tick(&model),
            Msg::Ping           => model.mpd_ctrl.lock().unwrap().ping().unwrap(),
            Msg::Quit           => gtk::main_quit(),
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

        let mut elems = UiElements {
            ctrl_prev:    get_object(&builder, "control-prev"),
            ctrl_next:    get_object(&builder, "control-next"),
            ctrl_toggle:  get_object(&builder, "control-toggle"),
            ctrl_repeat:  get_object(&builder, "control-repeat"),
            ctrl_shuffle: get_object(&builder, "control-shuffle"),
            adj_seek:     get_object(&builder, "adjustment-seek"),
            adj_volume:   get_object(&builder, "adjustment-volume"),
            disp_cursong: get_object(&builder, "display-currentsong"),
        };

        let screen = window.get_screen().unwrap();
        let css_provider = gtk::CssProvider::new();
        let _ = css_provider.load_from_data(include_str!("main.css"));
        gtk::StyleContext::add_provider_for_screen(&screen, &css_provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

        elems.ctrl_prev.signal = connect!(relm, elems.ctrl_prev.widget,
                 connect_clicked(_),
                 Msg::Control(Ctrl::Prev));

        elems.ctrl_next.signal = connect!(relm, elems.ctrl_next.widget,
                 connect_clicked(_),
                 Msg::Control(Ctrl::Next));

        elems.ctrl_toggle.signal = connect!(relm, elems.ctrl_toggle.widget,
                 connect_clicked(w),
                 Msg::Control(Ctrl::Pause(w.get_label().map(|s| s == "pause".to_string()).unwrap_or(false))));

        elems.ctrl_repeat.signal = connect!(relm, elems.ctrl_repeat.widget,
                 connect_toggled(w),
                 Msg::Control(Ctrl::Repeat(w.get_active())));

        elems.ctrl_shuffle.signal = connect!(relm, elems.ctrl_shuffle.widget,
                 connect_toggled(w),
                 Msg::Control(Ctrl::Shuffle(w.get_active())));

        elems.adj_seek.signal = connect!(relm, elems.adj_seek.widget,
                 connect_value_changed(adj),
                 Msg::Control(Ctrl::Seek(adj.get_value())));

        elems.adj_volume.signal = connect!(relm, elems.adj_volume.widget,
                 connect_value_changed(adj),
                 Msg::Control(Ctrl::Volume(adj.get_value() as i8)));


        connect!(relm, window, connect_delete_event(_, _) (Msg::Quit, Inhibit(false)));

        window.show_all();

        relm.stream().emit(Msg::StartListening);
        relm.stream().emit(Msg::UpdateUi);

        Win {
            window: window,
            elems: Rc::new(elems),
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
    relm::run::<Win>(("127.0.0.1".into(), 6600)).unwrap();
}
