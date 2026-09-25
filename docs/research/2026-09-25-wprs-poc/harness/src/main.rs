//! PoC harness: one klamottenkiste WaylandPane in a GTK4 window; spawns the command given
//! after `--` with WAYLAND_DISPLAY pointed at the pane's nested socket. Writes the
//! socket names to $KST_INFO (default ./kst-info) as `wayland=<name>\ncontrol=<path>\n`.
use gtk::glib;
use gtk::prelude::*;
use klamottenkiste::WaylandPane;
use std::process::Command;

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let cmd: Vec<String> = match args.iter().position(|a| a == "--") {
        Some(i) => args[i + 1..].to_vec(),
        None => vec![],
    };
    let info = std::env::var("KST_INFO").unwrap_or_else(|_| "kst-info".into());
    let w: i32 = std::env::var("KST_W").ok().and_then(|v| v.parse().ok()).unwrap_or(1280);
    let h: i32 = std::env::var("KST_H").ok().and_then(|v| v.parse().ok()).unwrap_or(800);
    let app = gtk::Application::builder()
        .application_id("org.kabelsalat.poc.WprsHarness")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        let pane = WaylandPane::new();
        if let Some(err) = pane.startup_error() {
            eprintln!("compositor failed to start: {err}");
        }
        let sock = pane.wayland_socket().unwrap_or_default();
        let ctl = pane
            .control_socket_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        std::fs::write(&info, format!("wayland={sock}\ncontrol={ctl}\n")).ok();
        println!("wayland={sock} control={ctl}");
        if !cmd.is_empty() {
            match Command::new(&cmd[0]).args(&cmd[1..]).env("WAYLAND_DISPLAY", &sock).spawn() {
                Ok(c) => println!("spawned pid {}", c.id()),
                Err(e) => eprintln!("spawn failed: {e}"),
            }
        }
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("kabelsalat wprs PoC")
            .default_width(w)
            .default_height(h)
            .child(&pane)
            .build();
        window.present();
    });
    app.run_with_args(&args[..1])
}
