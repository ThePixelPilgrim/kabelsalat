use relm4::RelmApp;
use relm4::adw;
use relm4::gtk::gio;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

mod app;
pub mod browser;
mod cli;
mod control;
pub mod remote;
pub mod remote_worker;
pub mod state;
pub mod tmuxctl;

pub const APP_ID: &str = "de.nereide.kabelsalat";

/// Whether some process already owns `APP_ID` on the session bus.
///
/// This is a direct `org.freedesktop.DBus.NameHasOwner` call, not
/// `app.register()` followed by `app.is_remote()`. Registering has a side
/// effect: if nobody else owns the name yet, registering makes *this*
/// process the primary instance, which fires `GtkApplication`'s `startup`
/// signal, which calls `gtk_init()`. `gtk_init()` fails hard when there is no
/// display — it prints `Gtk-WARNING: Failed to open display` and calls
/// `exit(1)` from inside GTK, before our own "not running" message ever gets
/// a chance to print. A bus query never touches GTK, so it is safe to run
/// before we know whether a display exists at all (ssh session, tty, systemd
/// user unit, ...). Do not "simplify" this back to register()/is_remote().
fn instance_is_running() -> Result<bool, String> {
    let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
        .map_err(|err| err.to_string())?;
    let args = glib::Variant::tuple_from_iter([APP_ID.to_variant()]);
    let reply = bus
        .call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameHasOwner",
            Some(&args),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
        )
        .map_err(|err| err.to_string())?;
    reply
        .child_value(0)
        .get::<bool>()
        .ok_or_else(|| "unexpected reply to NameHasOwner".to_string())
}

pub fn run() {
    // args_os + lossy conversion, not args(): the latter panics on non-UTF-8
    // arguments, which would abort with an undocumented exit code instead of
    // one of cli::EXIT_*. handle_command_line in control.rs already does the
    // same lossy conversion for forwarded arguments.
    let args: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    // Usage errors are decided here, before anything touches the bus, so a
    // typo reports as a typo whether or not a GUI is running.
    let parsed = match cli::parse(&args) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("kabelsalat: {}", err.0);
            eprint!("{}", cli::help_text());
            std::process::exit(cli::EXIT_USAGE.into());
        }
    };
    if parsed == cli::Cli::Help {
        print!("{}", cli::help_text());
        return;
    }

    // Constructing a GtkApplication is safe before GTK is initialised (the
    // gtk4 builder has no init assertion), so the subcommand path below never
    // needs a display.
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.connect_command_line(control::handle_command_line);

    if parsed.needs_instance() {
        // A subcommand must never start a GUI. See instance_is_running for why
        // this is a bus query rather than app.register()/app.is_remote().
        match instance_is_running() {
            Ok(true) => {
                // RelmApp::run drops this exit code, so drive the application
                // directly. run_with_args registers as a remote instance
                // itself and forwards the command line to the primary one.
                let code = app.run_with_args(&args);
                std::process::exit(code.get().into());
            }
            Ok(false) => {
                eprintln!("kabelsalat: not running");
                std::process::exit(cli::EXIT_NOT_RUNNING.into());
            }
            Err(err) => {
                // Bus unreachable, call failed, or reply undecodable — either
                // way there is nobody to talk to, but say why so a broken bus
                // is distinguishable from nothing running.
                eprintln!("kabelsalat: not running ({err})");
                std::process::exit(cli::EXIT_NOT_RUNNING.into());
            }
        }
    }

    // RelmApp::new calls relm4's private init(); from_app does not, and
    // set_global_css builds a CssProvider, which needs GTK up. So do here
    // exactly what relm4::init() would have done — but only on this path,
    // where a display is genuinely required.
    relm4::gtk::init().expect("failed to initialise GTK");
    adw::init().expect("failed to initialise libadwaita");

    relm4::set_global_css(
        ".tmux-warning { color: #e5a50a; }
         .browser-hidden { color: #3584e4; }
         .select-hint { color: #3584e4; animation: ks-pulse 1s ease-in-out infinite; }
         @keyframes ks-pulse { 0%, 100% { opacity: 1; } 50% { opacity: 0.35; } }
         .tab-crashed label { color: #e01b24; font-weight: bold; }
         button.tab-active label { font-weight: bold; }
         button.tab-active { background: alpha(currentColor, 0.12); }
         button.group-c0, button.group-c1, button.group-c2,
         button.group-c3, button.group-c4, button.group-c5 {
             border-left: none;
             border-radius: 0;
             border-bottom-width: 3px;
             border-bottom-style: solid;
         }
         button.group-c0 { border-bottom-color: #3584e4; }
         button.group-c1 { border-bottom-color: #33d17a; }
         button.group-c2 { border-bottom-color: #ff7800; }
         button.group-c3 { border-bottom-color: #9141ac; }
         button.group-c4 { border-bottom-color: #2190a4; }
         button.group-c5 { border-bottom-color: #986a44; }
         row.group-header { min-height: 0; padding-top: 1px; padding-bottom: 1px; background: alpha(currentColor, 0.08); }
         row.group-header label { font-size: 0.75em; font-weight: bold; opacity: 0.6; margin-left: 6px; }
         row.group-header-placeholder label { font-weight: normal; font-style: italic; opacity: 0.35; }
         row.host-disconnected { opacity: 0.55; }
         .group-c0 { border-left: 4px solid #3584e4; }
         .group-c1 { border-left: 4px solid #33d17a; }
         .group-c2 { border-left: 4px solid #ff7800; }
         .group-c3 { border-left: 4px solid #9141ac; }
         .group-c4 { border-left: 4px solid #2190a4; }
         .group-c5 { border-left: 4px solid #986a44; }",
    );
    RelmApp::from_app(app).with_args(args).run::<app::App>(());
}
