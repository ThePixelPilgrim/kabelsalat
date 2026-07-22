use relm4::RelmApp;

mod app;
pub mod state;
pub mod tmuxctl;

pub const APP_ID: &str = "de.nereide.kabelsalat";

pub fn run() {
    let app = RelmApp::new(APP_ID);
    relm4::set_global_css(
        ".tmux-warning { color: #e5a50a; }
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
         .group-c0 { border-left: 4px solid #3584e4; }
         .group-c1 { border-left: 4px solid #33d17a; }
         .group-c2 { border-left: 4px solid #ff7800; }
         .group-c3 { border-left: 4px solid #9141ac; }
         .group-c4 { border-left: 4px solid #2190a4; }
         .group-c5 { border-left: 4px solid #986a44; }",
    );
    app.run::<app::App>(());
}
