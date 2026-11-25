// Pas de console en plus de la fenêtre en version finale (Windows)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    orbit_lib::run()
}
