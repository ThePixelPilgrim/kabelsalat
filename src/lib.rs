use relm4::RelmApp;
use relm4::adw;
use relm4::gtk::gio;
use relm4::gtk::prelude::*;

mod app;
pub mod browser;
mod cli;
mod control;
pub mod state;
pub mod tmuxctl;

pub const APP_ID: &str = "de.nereide.kabelsalat";

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
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
        // A subcommand must never start a GUI. Registering tells us whether
        // somebody else already owns the name: if not, we are alone and there
        // is nothing to talk to, so bail out before `startup` builds a window.
        if app.register(gio::Cancellable::NONE).is_err() || !app.is_remote() {
            eprintln!("kabelsalat: not running");
            std::process::exit(cli::EXIT_NOT_RUNNING.into());
        }
        // RelmApp::run drops this exit code, so drive the application directly.
        let code = app.run_with_args(&args);
        std::process::exit(code.get().into());
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
         .group-c0 { border-left: 4px solid #3584e4; }
         .group-c1 { border-left: 4px solid #33d17a; }
         .group-c2 { border-left: 4px solid #ff7800; }
         .group-c3 { border-left: 4px solid #9141ac; }
         .group-c4 { border-left: 4px solid #2190a4; }
         .group-c5 { border-left: 4px solid #986a44; }",
    );
    RelmApp::from_app(app).with_args(args).run::<app::App>(());
}
