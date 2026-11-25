//! Coque Tauri d'ORBIT : fenêtre + démarrage du pont Python (bridge.py).
//!
//! Tout le travail réseau / système est dans le pont : ce fichier ne fait que
//!   1. générer un jeton aléatoire,
//!   2. lancer `python bridge.py --port 47900 --token <jeton> --parent-pid <pid>`,
//!   3. donner l'adresse et le jeton à l'interface (commande `bridge_info`).
//! Le pont surveille notre PID : quand ORBIT se ferme, il termine la session proprement et s'arrête.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;
use tauri::{Manager, State};

const BRIDGE_PORT: u16 = 47900;

struct Bridge {
    token: String,
    child: Mutex<Option<Child>>,
}

#[derive(serde::Serialize)]
struct BridgeInfo {
    url: String,
    token: String,
}

#[tauri::command]
fn bridge_info(bridge: State<Bridge>) -> BridgeInfo {
    BridgeInfo {
        url: format!("http://127.0.0.1:{}", BRIDGE_PORT),
        token: bridge.token.clone(),
    }
}

#[tauri::command]
fn toggle_fullscreen(window: tauri::WebviewWindow) -> Result<(), String> {
    let full = window.is_fullscreen().map_err(|e| e.to_string())?;
    window.set_fullscreen(!full).map_err(|e| e.to_string())
}

/// Dossier du pont : à côté de l'appli installée (ressources), sinon le dossier du projet (dev).
fn bridge_dir(app: &tauri::App) -> PathBuf {
    if let Ok(res) = app.path().resource_dir() {
        let packaged = res.join("bridge");
        if packaged.join("bridge.py").exists() {
            return packaged;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("bridge")
}

fn spawn_bridge(dir: &PathBuf, token: &str) -> std::io::Result<Child> {
    // ORBIT_PYTHON permet de choisir un autre interpréteur (ex. "py" ou un chemin complet)
    let python = std::env::var("ORBIT_PYTHON").unwrap_or_else(|_| "python".into());
    let mut cmd = Command::new(python);
    cmd.current_dir(dir)
        .arg(dir.join("bridge.py"))
        .args(["--port", &BRIDGE_PORT.to_string()])
        .args(["--token", token])
        .args(["--parent-pid", &std::process::id().to_string()]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW : pas de console qui s'ouvre
    }
    cmd.spawn()
}

pub fn run() {
    let token = uuid::Uuid::new_v4().simple().to_string();

    tauri::Builder::default()
        .manage(Bridge { token: token.clone(), child: Mutex::new(None) })
        .setup(move |app| {
            let dir = bridge_dir(app);
            match spawn_bridge(&dir, &token) {
                Ok(child) => {
                    *app.state::<Bridge>().child.lock().unwrap() = Some(child);
                }
                Err(e) => eprintln!("Impossible de lancer le pont ({}) : {}", dir.display(), e),
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![bridge_info, toggle_fullscreen])
        .run(tauri::generate_context!())
        .expect("erreur au démarrage d'ORBIT");
}
