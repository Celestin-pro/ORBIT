//! Script de compilation d'orbit-player.
//!
//! Deux responsabilités :
//! 1. indiquer à l'éditeur de liens où trouver `SDL3.lib` (et plus tard les `.lib` FFmpeg) ;
//! 2. copier les DLL de run-time (SDL3, FFmpeg) à côté de l'exécutable, pour qu'il démarre
//!    sans manipuler le `PATH`.
//!
//! Les chemins viennent des variables `SDL3_DIR` et `FFMPEG_DIR`, écrites dans
//! `.cargo/config.toml` par `scripts/setup.ps1`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo::rerun-if-env-changed=SDL3_DIR");
    println!("cargo::rerun-if-env-changed=FFMPEG_DIR");

    let dossier_exe = match dossier_executable() {
        Some(chemin) => chemin,
        None => {
            println!("cargo::warning=OUT_DIR inattendu : les DLL ne seront pas copiées.");
            return;
        }
    };

    configurer_sdl3(&dossier_exe);
    copier_dll_ffmpeg(&dossier_exe);
}

/// Dossier de sortie des exécutables (`target/<profil>`), déduit de `OUT_DIR`.
///
/// Cargo a changé la disposition de ses dossiers de travail : `OUT_DIR` vaut
/// `target/<profil>/build/<paquet>-<empreinte>/out` sur les anciennes versions et
/// `target/<profil>/build/<paquet>/<empreinte>/out` sur les récentes. Plutôt que de compter
/// un nombre fixe de niveaux, on remonte jusqu'au dossier `build` et on prend son parent.
fn dossier_executable() -> Option<PathBuf> {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR")?);
    out_dir
        .ancestors()
        .find(|dossier| dossier.file_name().is_some_and(|nom| nom == "build"))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

/// Chemin de recherche de `SDL3.lib` + copie de `SDL3.dll`.
fn configurer_sdl3(dossier_exe: &Path) {
    let Some(sdl3_dir) = env::var_os("SDL3_DIR").map(PathBuf::from) else {
        println!(
            "cargo::warning=SDL3_DIR non définie : lancez scripts/setup.ps1. \
             L'édition de liens échouera si SDL3.lib n'est pas déjà visible."
        );
        return;
    };

    let dossier_lib = sdl3_dir.join("lib").join("x64");
    if !dossier_lib.join("SDL3.lib").is_file() {
        println!(
            "cargo::warning=SDL3.lib introuvable dans {} : relancez scripts/setup.ps1.",
            dossier_lib.display()
        );
        return;
    }

    // `sdl3-sys` émet déjà `cargo::rustc-link-lib=SDL3` ; il ne manque que le chemin.
    println!("cargo::rustc-link-search=native={}", dossier_lib.display());

    copier_si_plus_recent(&dossier_lib.join("SDL3.dll"), dossier_exe);
}

/// Copie les DLL FFmpeg à côté de l'exécutable (sans effet si `FFMPEG_DIR` n'est pas définie,
/// ce qui est le cas tant que la réception n'est pas implémentée).
fn copier_dll_ffmpeg(dossier_exe: &Path) {
    let Some(ffmpeg_dir) = env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
        return;
    };

    let dossier_bin = ffmpeg_dir.join("bin");
    let Ok(entrees) = fs::read_dir(&dossier_bin) else {
        println!(
            "cargo::warning=Dossier {} illisible : DLL FFmpeg non copiées.",
            dossier_bin.display()
        );
        return;
    };

    for entree in entrees.flatten() {
        let chemin = entree.path();
        if chemin.extension().is_some_and(|e| e.eq_ignore_ascii_case("dll")) {
            copier_si_plus_recent(&chemin, dossier_exe);
        }
    }
}

/// Copie `source` dans `dossier_cible` si la destination est absente ou plus ancienne.
/// Une DLL verrouillée (exe en cours d'exécution) produit un avertissement, pas un échec.
fn copier_si_plus_recent(source: &Path, dossier_cible: &Path) {
    let Some(nom) = source.file_name() else {
        return;
    };
    let destination = dossier_cible.join(nom);

    println!("cargo::rerun-if-changed={}", source.display());

    let a_copier = match (source.metadata(), destination.metadata()) {
        (Ok(src), Ok(dst)) => match (src.modified(), dst.modified()) {
            (Ok(t_src), Ok(t_dst)) => t_src > t_dst,
            _ => true,
        },
        (Ok(_), Err(_)) => true,
        (Err(_), _) => {
            println!("cargo::warning=Source introuvable : {}", source.display());
            return;
        }
    };

    if !a_copier {
        return;
    }

    if let Err(erreur) = fs::create_dir_all(dossier_cible) {
        println!("cargo::warning=Création de {} impossible : {erreur}", dossier_cible.display());
        return;
    }

    if let Err(erreur) = fs::copy(source, &destination) {
        println!(
            "cargo::warning=Copie de {} vers {} impossible : {erreur}",
            source.display(),
            destination.display()
        );
    }
}
