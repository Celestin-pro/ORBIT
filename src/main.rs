//! orbit-player : récepteur de streaming d'ORBIT.
//!
//! v0.1, étapes 1 à 3 : fenêtre SDL3, device Direct3D 11 unique, chaîne d'échange « flip »,
//! réception UDP/SRT du MPEG-TS, décodage D3D11VA sans copie CPU, et affichage NV12 -> RGB
//! en respectant le ratio. Étape 4 : overlay de stats (F3), dont les chiffres sont aussi
//! résumés une fois par seconde dans le journal.

mod config;
mod render;
mod stats;

mod video;

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sdl3::event::{Event, WindowEvent};
use sdl3::keyboard::{Keycode, Mod, Scancode};

use crate::config::Config;
use crate::render::d3d11::{Gpu, Pipeline, Presenter, RenderError, hwnd_de};
use crate::render::letterbox::{Viewport, letterbox};
use crate::render::nv12::Nv12Pool;
use crate::render::scene::{DELAI_SIGNAL_PERDU, EtatSignal, Scene, SuiviSignal};
use crate::stats::{Echantillonneur, Stats};
use crate::video::avcodec_decoder::AvcodecD3d11Decoder;
use crate::video::frame::{DecodedFrame, Mailbox};
use crate::video::lavf_source::LavfSource;
use crate::video::thread::VideoThread;

/// Taille de la fenêtre au démarrage (16:9, confortable sur un portable 1080p).
const TAILLE_INITIALE: (u32, u32) = (1280, 720);

/// Gris très sombre : on voit que le programme dessine, sans éblouir dans le noir.
const COULEUR_FOND: [f32; 4] = [0.02, 0.02, 0.03, 1.0];

/// Délai maximal d'attente de la chaîne d'échange : au-delà, on repasse par la boucle
/// d'événements pour rester réactif à la fermeture de la fenêtre.
const ATTENTE_PRESENTATION_MS: u32 = 100;

/// Délai maximal d'attente d'une nouvelle image décodée. Court, pour continuer à traiter les
/// événements de la fenêtre même quand le flux est muet.
const ATTENTE_IMAGE_MS: u64 = 8;

fn main() -> ExitCode {
    regler_console_en_utf8();

    // `ORBIT_LOG=debug` pour plus de détail ; info par défaut.
    env_logger::Builder::from_env(env_logger::Env::default().filter_or("ORBIT_LOG", "info"))
        .format_timestamp_millis()
        .init();

    match executer() {
        Ok(()) => ExitCode::SUCCESS,
        Err(erreur) => {
            // Code de sortie 0 uniquement si l'utilisateur a fermé la fenêtre (contrat avec
            // bridge.py) ; toute erreur part sur stderr avec sa chaîne de causes.
            eprintln!("orbit-player : {erreur:#}");
            log::error!("arrêt sur erreur : {erreur:#}");
            ExitCode::FAILURE
        }
    }
}

fn executer() -> Result<()> {
    let config = Config::parse_config();
    log::info!(
        "Démarrage : hôte {}, transport {}, vidéo {}, vsync {}",
        config.host,
        config.transport,
        config.url_video(),
        config.vsync
    );

    let debug_d3d = cfg!(debug_assertions);

    let sdl = sdl3::init().map_err(anyhow::Error::msg).context("initialisation de SDL3")?;
    let video = sdl
        .video()
        .map_err(anyhow::Error::msg)
        .context("initialisation du sous-système vidéo de SDL3")?;

    let mut constructeur = video.window(&config.title, TAILLE_INITIALE.0, TAILLE_INITIALE.1);
    constructeur.position_centered().resizable();
    if config.fullscreen {
        constructeur.fullscreen();
    }
    let mut fenetre = constructeur.build().context("création de la fenêtre SDL3")?;

    let hwnd = hwnd_de(&fenetre).context("récupération du HWND de la fenêtre SDL3")?;

    let gpu = Arc::new(Gpu::nouveau(debug_d3d).context("création du device Direct3D 11")?);
    log::debug!("Niveau de fonctionnalité Direct3D : {:#x}", gpu.niveau());

    Nv12Pool::verifier_support_nv12(&gpu)
        .context("cette carte graphique ne peut pas échantillonner du NV12")?;
    let pipeline = Pipeline::nouveau(&gpu).context("compilation du pipeline NV12 -> RGB")?;
    let mut presenter = Presenter::nouveau(
        &gpu,
        hwnd,
        fenetre.size_in_pixels(),
        config.vsync.est_active(),
        debug_d3d,
    )
    .context("création de la chaîne d'échange")?;

    let mut scene = Scene::nouveau(&gpu).context("préparation de l'overlay")?;

    let stats = Arc::new(Stats::new());
    // Tant que le flux n'est pas ouvert, on raisonne en 16:9 : la zone utile est déjà
    // calculée et les bandes noires visibles. Le thread vidéo corrigera la résolution.
    stats.enregistrer_resolution(1920, 1080);
    let mut echantillonneur = Echantillonneur::new(&stats, Instant::now());

    // Pool de textures NV12 + boîte aux lettres d'une place : c'est la frontière entre le
    // thread vidéo (qui copie et publie) et ce thread (qui dessine et présente).
    let boite = Arc::new(Mailbox::new());
    let pool =
        Arc::new(Nv12Pool::nouveau(Arc::clone(&gpu), Arc::clone(&boite), Arc::clone(&stats)));

    let decodeur = AvcodecD3d11Decoder::nouveau(Arc::clone(&gpu))
        .context("création du décodeur matériel D3D11VA")?;

    // Thread vidéo : il ouvre l'entrée (ce qui peut bloquer), décode et publie.
    // Le drapeau d'arrêt sert aussi de rappel d'interruption à libavformat, ce qui rend la
    // fermeture immédiate même sur un socket silencieux.
    let arret_video = Arc::new(AtomicBool::new(false));
    let url_video = config.url_video();
    let arret_source = Arc::clone(&arret_video);
    let thread_video = VideoThread::demarrer(
        move || LavfSource::ouvrir(&url_video, arret_source),
        Box::new(decodeur),
        Arc::clone(&pool) as Arc<dyn crate::video::decoder::FrameSink>,
        arret_video,
        Arc::clone(&stats),
    )
    .context("démarrage du thread vidéo")?;

    let mut pompe = sdl
        .event_pump()
        .map_err(anyhow::Error::msg)
        .context("création de la pompe d'événements")?;
    let mut overlay_visible = false;
    // Texte de l'overlay, recalculé une fois par seconde.
    let mut texte_stats = vec!["Mesures dans une seconde…".to_owned()];
    let mut souris_capturee = false;
    // Image actuellement à l'écran : son emplacement NV12 n'est libéré qu'après la
    // présentation suivante, et elle est redessinée quand l'affichage change sans image neuve.
    let mut image_affichee: Option<DecodedFrame> = None;
    // Image retirée de la boîte aux lettres mais pas encore présentée (la chaîne d'échange
    // n'était pas prête) : elle est conservée d'une itération à l'autre.
    let mut image_a_presenter: Option<DecodedFrame> = None;
    // Vrai quand l'affichage doit changer sans nouvelle image (overlay, redimensionnement) :
    // on représente alors l'image courante. Vrai au départ pour peindre le fond.
    let mut a_redessiner = true;
    // « En attente du flux… » puis « Signal perdu » après 2 s sans image.
    let mut suivi_signal = SuiviSignal::nouveau();
    let mut etat_affiche = EtatSignal::EnAttente;

    log::info!(
        "Fenêtre prête. Ctrl+Alt+Shift+Q quitter, Ctrl+Alt+Shift+Z souris, F3 stats, F11 plein écran."
    );

    'boucle: loop {
        for evenement in pompe.poll_iter() {
            match evenement {
                Event::Quit { .. } => break 'boucle,

                Event::Window { win_event, .. } => match win_event {
                    WindowEvent::CloseRequested => break 'boucle,
                    // PixelSizeChanged est la taille en pixels réels : c'est celle de la
                    // chaîne d'échange (Resized est en coordonnées logiques).
                    WindowEvent::PixelSizeChanged(largeur, hauteur) => {
                        let taille = (largeur.max(0) as u32, hauteur.max(0) as u32);
                        if let Err(erreur) = presenter.redimensionner(&gpu, taille) {
                            // Un échec de redimensionnement est fatal : la cible de rendu
                            // n'existe plus.
                            return Err(anyhow::Error::new(erreur)
                                .context("redimensionnement de la chaîne d'échange"));
                        }
                        a_redessiner = true;
                    }
                    _ => {}
                },

                Event::KeyDown { scancode, keycode, keymod, repeat: false, .. } => {
                    log::trace!(
                        "touche enfoncée : scancode {scancode:?}, keycode {keycode:?},                          modificateurs {keymod:?}"
                    );
                    match action_clavier(scancode, keycode, keymod) {
                        Some(Action::Quitter) => break 'boucle,
                        Some(Action::BasculerPleinEcran) => {
                            let plein_ecran =
                                fenetre.fullscreen_state() != sdl3::video::FullscreenType::Off;
                            if let Err(erreur) = fenetre.set_fullscreen(!plein_ecran) {
                                log::warn!("Basculement plein écran refusé par SDL : {erreur}");
                            }
                        }
                        Some(Action::BasculerOverlay) => {
                            overlay_visible = !overlay_visible;
                            a_redessiner = true;
                            log::debug!(
                                "Overlay de stats {}",
                                if overlay_visible { "affiché" } else { "masqué" }
                            );
                        }
                        Some(Action::BasculerCaptureSouris) => {
                            souris_capturee = !souris_capturee;
                            log::info!(
                                "Capture de la souris : {} (effective à l'étape 2)",
                                if souris_capturee { "active" } else { "libérée" }
                            );
                        }
                        None => {}
                    }
                }

                _ => {}
            }
        }

        // Une fois par seconde : un seul échantillon pour l'overlay et le journal.
        if let Some(echantillon) = echantillonneur.echantillonner(&stats, Instant::now()) {
            let lignes = crate::stats::lignes(&echantillon);
            if echantillon.actif() {
                log::info!("stats | {}", crate::stats::ligne_journal(&lignes));
            }
            texte_stats = crate::stats::texte_overlay(&lignes);
            a_redessiner |= overlay_visible;
        }

        // Le bandeau change même sans image : c'est justement l'absence d'image qu'il signale.
        let etat = suivi_signal.etat(Instant::now());
        if etat != etat_affiche {
            journaliser_signal(etat_affiche, etat);
            etat_affiche = etat;
            a_redessiner = true;
        }

        // Dernière image décodée, ou rien si le flux est muet. Règle 3 : la boîte aux
        // lettres ne rend que la plus récente.
        if image_a_presenter.is_none() {
            image_a_presenter = boite.attendre_et_retirer(Duration::from_millis(ATTENTE_IMAGE_MS));
        }

        // Rien de neuf et rien à changer à l'écran : on garde ce qui y est déjà plutôt que de
        // présenter une image identique, ce qui économise le GPU.
        if image_a_presenter.is_none() && !a_redessiner {
            continue;
        }

        // L'attente de la chaîne d'échange ne vient qu'*après* avoir quelque chose à montrer.
        // L'objet de latence est à réarmement automatique : l'attendre sans présenter
        // consomme le signal, et plus rien ne le réarme avant le `Present` suivant. La
        // boucle resterait alors bloquée tant que le flux est muet.
        if !presenter.attendre_disponibilite(ATTENTE_PRESENTATION_MS) {
            // On garde l'image pour la prochaine itération : elle n'est pas perdue.
            continue;
        }

        // La chaîne d'échange était peut-être prête depuis un moment : s'il y a plus récent,
        // on l'affiche à la place (règle 3) et on recycle l'emplacement abandonné.
        let nouvelle = match (image_a_presenter.take(), boite.retirer()) {
            (Some(ancienne), Some(plus_recente)) => {
                pool.liberer(ancienne.slot);
                stats.compter_image_jetee();
                Some(plus_recente)
            }
            (image, None) => image,
            (None, plus_recente) => plus_recente,
        };

        // Image neuve, sinon celle déjà à l'écran (redessin), sinon le fond seul.
        let a_dessiner = nouvelle.or(image_affichee);
        if nouvelle.is_some() {
            suivi_signal.image_recue(Instant::now());
            let etat = suivi_signal.etat(Instant::now());
            if etat != etat_affiche {
                journaliser_signal(etat_affiche, etat);
                etat_affiche = etat;
            }
        }

        presenter.effacer(&gpu, COULEUR_FOND);
        if let Some(image) = a_dessiner {
            let zone: Viewport = letterbox(presenter.taille(), (image.width, image.height));
            match pool.vues(image.slot) {
                Some((luma, chroma)) => {
                    presenter.dessiner(&gpu, &pipeline, (&luma, &chroma), zone);
                }
                // L'emplacement a disparu (réallocation pour un changement de résolution) :
                // on présente le fond, l'image suivante sera correcte.
                None => log::debug!("Emplacement {} introuvable au moment du rendu.", image.slot),
            }
        }
        scene.dessiner(
            &gpu,
            &presenter,
            overlay_visible.then_some(texte_stats.as_slice()),
            etat_affiche,
        );

        let resultat = presenter.presenter();
        a_redessiner = false;

        if let Some(image) = nouvelle {
            // L'emplacement de l'image *précédente* n'est rendu qu'ici : la présentation
            // qu'on vient d'attendre garantit que le GPU a fini de la lire. Sans ce décalage
            // d'une image, le décodeur pourrait réécrire une texture encore en lecture.
            if let Some(precedente) = image_affichee.replace(image) {
                pool.liberer(precedente.slot);
            }
        }

        match resultat {
            Ok(()) => {
                if let Some(image) = nouvelle {
                    stats.compter_image_presentee(image.duree_jusqu_a(Instant::now()));
                }
            }
            Err(RenderError::Api { operation, source }) => {
                // Une présentation qui échoue durablement signifie device perdu : on sort
                // proprement plutôt que de tourner à vide.
                return Err(
                    anyhow::Error::new(source).context(format!("présentation ({operation})"))
                );
            }
            Err(autre) => return Err(anyhow::Error::new(autre).context("présentation")),
        }
    }

    log::info!("Fenêtre fermée par l'utilisateur, arrêt.");
    // Avant de rendre la main : on coupe la réception, ce qui réveille le thread vidéo
    // bloqué sur le réseau, et on attend sa fin pour refermer l'entrée proprement.
    // La boîte aux lettres est arrêtée pour qu'aucune publication ne reste en attente.
    boite.arreter();
    thread_video.arreter();
    Ok(())
}

/// Trace les changements d'état du signal : ce sont les moments à retrouver dans le journal
/// après une coupure.
fn journaliser_signal(avant: EtatSignal, apres: EtatSignal) {
    match (avant, apres) {
        (_, EtatSignal::Perdu) => log::warn!(
            "Signal perdu : aucune image depuis {} s, la dernière reste affichée.",
            DELAI_SIGNAL_PERDU.as_secs()
        ),
        (EtatSignal::EnAttente, EtatSignal::Recu) => log::info!("Première image affichée."),
        (EtatSignal::Perdu, EtatSignal::Recu) => log::info!("Signal rétabli."),
        _ => {}
    }
}

/// Passe la console en UTF-8, sinon les accents des journaux sont illisibles : Rust écrit
/// de l'UTF-8 alors que la console Windows utilise par défaut une page de code héritée.
///
/// Un échec (sortie redirigée vers un fichier, pas de console attachée) est sans gravité.
///
/// **Seule exception à la règle 5** (`unsafe` confiné à `render/` et aux deux modules
/// FFmpeg) : un appel système sans pointeur ni allocation, qui ne peut pas corrompre la
/// mémoire. Le déplacer dans `render/` serait moins clair que de l'assumer ici.
fn regler_console_en_utf8() {
    use windows::Win32::System::Console::SetConsoleOutputCP;

    /// Page de code UTF-8 (`CP_UTF8`), déclarée ici pour ne pas tirer toute la
    /// fonctionnalité `Win32_Globalization` de la crate `windows`.
    const CP_UTF8: u32 = 65_001;

    // SAFETY: SetConsoleOutputCP n'a pas de précondition et ne prend aucun pointeur ;
    // elle agit sur la console attachée au processus, ou échoue sans effet.
    let _ = unsafe { SetConsoleOutputCP(CP_UTF8) };
}

/// Actions associées aux touches réservées de CLAUDE.md. Ces combinaisons ne seront jamais
/// transmises à l'hôte (étape 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Quitter,
    BasculerCaptureSouris,
    BasculerOverlay,
    BasculerPleinEcran,
}

/// Traduit une touche enfoncée en action réservée, ou `None` si la touche doit être
/// transmise à l'hôte.
///
/// Les lettres sont reconnues par leur **keycode**, c'est-à-dire le caractère imprimé sur la
/// touche, et non par leur scancode, qui désigne une *position physique* : sur un clavier
/// AZERTY, le scancode `Q` est la touche étiquetée « A » et le scancode `Z` celle étiquetée
/// « W ». Ctrl+Alt+Shift+Q doit marcher sur la touche qui dit Q, quelle que soit la
/// disposition.
///
/// Les touches de fonction, elles, sont identiques partout : le scancode suffit, et il reste
/// valable même si une disposition exotique remappe F3 ou F11.
///
/// Fonction pure, donc testable sans fenêtre.
fn action_clavier(
    scancode: Option<Scancode>,
    keycode: Option<Keycode>,
    keymod: Mod,
) -> Option<Action> {
    let ctrl = keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD);
    let alt = keymod.intersects(Mod::LALTMOD | Mod::RALTMOD);
    let shift = keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);
    let trio = ctrl && alt && shift;

    if trio {
        match keycode {
            Some(Keycode::Q) => return Some(Action::Quitter),
            Some(Keycode::Z) => return Some(Action::BasculerCaptureSouris),
            _ => {}
        }
    }

    match scancode {
        Some(Scancode::F3) => Some(Action::BasculerOverlay),
        Some(Scancode::F11) => Some(Action::BasculerPleinEcran),
        _ => None,
    }
}

impl Config {
    /// Analyse les arguments. Isolé ici pour que `config.rs` reste sans dépendance au binaire.
    fn parse_config() -> Self {
        <Self as clap::Parser>::parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRIO: Mod = Mod::LCTRLMOD.union(Mod::LALTMOD).union(Mod::LSHIFTMOD);

    /// Raccourci de lecture : une touche décrite par sa position physique et son étiquette.
    fn touche(scancode: Scancode, keycode: Keycode, keymod: Mod) -> Option<Action> {
        action_clavier(Some(scancode), Some(keycode), keymod)
    }

    #[test]
    fn le_trio_reserve_quitter_et_la_souris_en_qwerty() {
        // Clavier QWERTY : étiquette et position coïncident.
        assert_eq!(touche(Scancode::Q, Keycode::Q, TRIO), Some(Action::Quitter));
        assert_eq!(touche(Scancode::Z, Keycode::Z, TRIO), Some(Action::BasculerCaptureSouris));
    }

    #[test]
    fn le_trio_fonctionne_sur_un_clavier_azerty() {
        // Sur AZERTY, la touche étiquetée « Q » occupe la position du A américain,
        // et celle étiquetée « Z » la position du W américain.
        assert_eq!(touche(Scancode::A, Keycode::Q, TRIO), Some(Action::Quitter));
        assert_eq!(touche(Scancode::W, Keycode::Z, TRIO), Some(Action::BasculerCaptureSouris));

        // Et les touches étiquetées A et W, elles, partent à l'hôte.
        assert_eq!(touche(Scancode::Q, Keycode::A, TRIO), None);
        assert_eq!(touche(Scancode::W, Keycode::Z, Mod::NOMOD), None);
    }

    #[test]
    fn q_et_z_seuls_sont_transmis_a_l_hote() {
        assert_eq!(touche(Scancode::A, Keycode::Q, Mod::NOMOD), None);
        assert_eq!(touche(Scancode::W, Keycode::Z, Mod::NOMOD), None);
        // Un modificateur manquant ne suffit pas.
        assert_eq!(touche(Scancode::A, Keycode::Q, Mod::LCTRLMOD | Mod::LALTMOD), None);
        assert_eq!(touche(Scancode::W, Keycode::Z, Mod::LCTRLMOD | Mod::LSHIFTMOD), None);
    }

    #[test]
    fn les_modificateurs_droits_fonctionnent_aussi() {
        // AltGr est rapporté par Windows comme Ctrl gauche + Alt droit : le trio doit
        // rester reconnaissable.
        let trio_droit = Mod::RCTRLMOD | Mod::RALTMOD | Mod::RSHIFTMOD;
        assert_eq!(touche(Scancode::A, Keycode::Q, trio_droit), Some(Action::Quitter));

        let altgr = Mod::LCTRLMOD | Mod::RALTMOD | Mod::LSHIFTMOD;
        assert_eq!(touche(Scancode::A, Keycode::Q, altgr), Some(Action::Quitter));
    }

    #[test]
    fn f3_et_f11_sont_reserves_sans_modificateur() {
        assert_eq!(touche(Scancode::F3, Keycode::F3, Mod::NOMOD), Some(Action::BasculerOverlay));
        assert_eq!(
            touche(Scancode::F11, Keycode::F11, Mod::NOMOD),
            Some(Action::BasculerPleinEcran)
        );
        // Verr. num. ou Maj. verrouillée ne doivent pas désactiver ces touches.
        assert_eq!(touche(Scancode::F3, Keycode::F3, Mod::NUMMOD), Some(Action::BasculerOverlay));
    }

    #[test]
    fn les_autres_touches_ne_sont_pas_reservees() {
        let touches = [
            (Scancode::A, Keycode::Q),
            (Scancode::W, Keycode::Z),
            (Scancode::Space, Keycode::Space),
            (Scancode::Escape, Keycode::Escape),
        ];
        for (scancode, keycode) in touches {
            // Sans le trio, rien n'est réservé (hors touches de fonction).
            assert_eq!(touche(scancode, keycode, Mod::NOMOD), None, "{scancode:?}");
        }
        assert_eq!(touche(Scancode::Space, Keycode::Space, TRIO), None);
        assert_eq!(touche(Scancode::Escape, Keycode::Escape, TRIO), None);
    }

    #[test]
    fn une_touche_sans_code_connu_est_ignoree() {
        // SDL peut rapporter un événement sans scancode ni keycode (touche exotique) :
        // il ne doit rien déclencher, et surtout pas paniquer.
        assert_eq!(action_clavier(None, None, TRIO), None);
        assert_eq!(action_clavier(None, Some(Keycode::Q), TRIO), Some(Action::Quitter));
        assert_eq!(
            action_clavier(Some(Scancode::F11), None, Mod::NOMOD),
            Some(Action::BasculerPleinEcran)
        );
    }
}
