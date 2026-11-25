//! `LavfSource` : implémentation de [`VideoSource`] au-dessus de libavformat.
//!
//! Elle ouvre une entrée UDP ou SRT, démultiplexe le MPEG-TS, et livre les unités d'accès
//! H.264/HEVC. C'est l'un des deux seuls modules autorisés à importer `ffmpeg-sys-next`
//! (l'autre étant `avcodec_decoder.rs`, étape 3) : aucun type FFmpeg ne sort d'ici.
//!
//! Points délicats, tous traités explicitement plus bas :
//! - **arrêt propre** : `av_read_frame` bloque sur un socket silencieux. On installe un
//!   rappel d'interruption (`AVFormatContext::interrupt_callback`), que FFmpeg interroge
//!   pendant ses attentes réseau ; c'est le seul moyen de rendre la fermeture immédiate.
//! - **ouverture en deux temps** : le rappel doit être posé *avant* `avformat_open_input`,
//!   donc on alloue le contexte nous-mêmes avec `avformat_alloc_context`.
//! - **pas de panique** (règle 6) : chaque code `AVERROR` est traduit en [`SourceError`].

use std::ffi::{CStr, CString, c_int, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

use ffmpeg_sys_next as ff;

use crate::video::source::{AccessUnit, SourceError, StreamInfo, VideoCodec, VideoSource};

/// `avformat_network_init` n'a besoin d'être appelée qu'une fois par processus.
static RESEAU: Once = Once::new();

/// Options passées à libavformat, communes à UDP et SRT (CLAUDE.md, chemin vidéo § 1).
///
/// `flags=low_delay` n'est pas ici : c'est une option du *décodeur*, posée à l'étape 3.
const OPTIONS_COMMUNES: &[(&str, &str)] = &[
    // On ne veut aucun tampon de démultiplexage : chaque paquet part dès qu'il est complet.
    ("fflags", "nobuffer"),
    // Analyse minimale à l'ouverture : on sait déjà que c'est du MPEG-TS H.264/HEVC.
    ("probesize", "32768"),
    ("analyzeduration", "100000"),
    // MPEG-TS : lire toutes les PMT évite de rater le flux si la première est incomplète.
    ("scan_all_pmts", "1"),
];

/// Source vidéo fondée sur libavformat.
pub struct LavfSource {
    /// Contexte de démultiplexage ; nul entre une fermeture et une réouverture.
    contexte: *mut ff::AVFormatContext,
    /// Paquet réutilisé à chaque lecture (aucune allocation par image).
    paquet: *mut ff::AVPacket,
    /// Indice du flux vidéo retenu.
    index_video: c_int,
    /// Base de temps de ce flux, pour convertir les PTS en microsecondes.
    base_temps: ff::AVRational,
    info: Option<StreamInfo>,
    /// Jeux de paramètres annoncés par l'entrée (`codecpar->extradata`), en Annex B.
    jeux_de_parametres: Vec<u8>,
    url: String,
    /// Drapeau lu par le rappel d'interruption de FFmpeg. Il doit rester à une adresse
    /// stable aussi longtemps que le contexte vit, d'où l'`Arc`.
    arret: Arc<AtomicBool>,
}

// SAFETY: `LavfSource` possède ses pointeurs FFmpeg (contexte et paquet) et ne les partage
// avec personne : seules ses propres méthodes y touchent, et elles prennent `&mut self`. Le
// transfert vers le thread vidéo est donc sûr. Le seul accès concurrent est celui du rappel
// d'interruption, qui ne lit que l'`AtomicBool`.
unsafe impl Send for LavfSource {}

impl LavfSource {
    /// Ouvre l'entrée décrite par `url` (déjà porteuse de ses options, voir
    /// [`crate::config::Config::url_video`]).
    ///
    /// `arret` est partagé avec l'appelant : le passer à `true` débloque immédiatement une
    /// lecture en cours.
    pub fn ouvrir(url: &str, arret: Arc<AtomicBool>) -> Result<Self, SourceError> {
        RESEAU.call_once(|| {
            // SAFETY: appel sans argument, documenté comme devant précéder tout usage des
            // protocoles réseau ; `Once` garantit l'unicité.
            unsafe {
                ff::avformat_network_init();
            }
        });

        // FFmpeg écrit ses propres diagnostics directement sur stderr, sans passer par notre
        // journal. Tant qu'une image clé n'est pas arrivée, le décodeur en émet une paire par
        // image (« non-existing PPS », « Error constructing the frame RPS ») : c'est normal
        // quand on rejoint un flux en cours, et notre compteur d'unités refusées le rapporte
        // déjà. On ne garde donc que le fatal, sauf en mode debug.
        // SAFETY: simple réglage d'un entier global, sans précondition.
        unsafe {
            ff::av_log_set_level(if log::log_enabled!(log::Level::Debug) {
                ff::AV_LOG_VERBOSE
            } else {
                ff::AV_LOG_FATAL
            });
        }

        // SAFETY: av_packet_alloc renvoie soit un paquet valide, soit null (vérifié juste
        // après). Il est libéré dans `Drop`.
        let paquet = unsafe { ff::av_packet_alloc() };
        if paquet.is_null() {
            return Err(SourceError::Autre("allocation d'un AVPacket impossible".into()));
        }

        let mut source = Self {
            contexte: ptr::null_mut(),
            paquet,
            index_video: -1,
            base_temps: ff::AVRational { num: 1, den: 90_000 },
            info: None,
            jeux_de_parametres: Vec::new(),
            url: url.to_owned(),
            arret,
        };

        // En UDP comme en SRT, `avformat_open_input` ne rend la main qu'une fois assez de
        // données reçues pour identifier le flux (ou au délai de silence). Le thread vidéo
        // réessaie en boucle : le détail de chaque tentative ne sert qu'au débogage.
        log::debug!("Ouverture de l'entrée {} : attente du flux…", source.description());

        source.ouvrir_entree()?;
        Ok(source)
    }

    /// Ouvre (ou réouvre) l'entrée et repère le flux vidéo.
    fn ouvrir_entree(&mut self) -> Result<(), SourceError> {
        self.fermer_entree();

        // SAFETY: avformat_alloc_context renvoie un contexte valide ou null (vérifié).
        let contexte = unsafe { ff::avformat_alloc_context() };
        if contexte.is_null() {
            return Err(SourceError::Ouverture {
                url: self.url.clone(),
                cause: "allocation du contexte impossible".into(),
            });
        }

        // Le rappel d'interruption doit être posé AVANT l'ouverture : c'est lui qui rend la
        // fermeture immédiate si le flux ne vient jamais.
        // SAFETY: `contexte` vient d'être alloué et nous appartient. Le pointeur opaque
        // désigne l'`AtomicBool` de l'`Arc` que `self` conserve : il reste valide aussi
        // longtemps que ce contexte, puisque `Drop` ferme le contexte avant de lâcher l'Arc.
        unsafe {
            (*contexte).interrupt_callback = ff::AVIOInterruptCB {
                callback: Some(interruption_demandee),
                opaque: Arc::as_ptr(&self.arret) as *mut c_void,
            };
        }
        self.contexte = contexte;

        let mut options = Dictionnaire::nouveau();
        for (cle, valeur) in OPTIONS_COMMUNES {
            options.poser(cle, valeur)?;
        }

        let url_c = CString::new(self.url.as_str())
            .map_err(|_| SourceError::Autre("URL contenant un octet nul".into()))?;

        // SAFETY: `self.contexte` est un contexte alloué ; avformat_open_input prend la
        // propriété du contexte en cas de succès comme en cas d'échec (il le libère lui-même
        // et met le pointeur à null), ce que `fermer_entree` respecte.
        let code = unsafe {
            ff::avformat_open_input(
                &raw mut self.contexte,
                url_c.as_ptr(),
                ptr::null(),
                options.pointeur_mut(),
            )
        };
        if code < 0 {
            // avformat_open_input a déjà libéré le contexte et mis le pointeur à null.
            self.contexte = ptr::null_mut();
            return Err(SourceError::Ouverture {
                url: self.url.clone(),
                cause: message_erreur(code),
            });
        }

        // Les options non consommées signalent une faute de frappe de notre côté.
        options.journaliser_restes();

        // SAFETY: `self.contexte` est désormais un contexte ouvert et valide.
        let code = unsafe { ff::avformat_find_stream_info(self.contexte, ptr::null_mut()) };
        if code < 0 {
            let cause = message_erreur(code);
            self.fermer_entree();
            return Err(SourceError::Ouverture { url: self.url.clone(), cause });
        }

        self.reperer_flux_video()?;
        log::info!(
            "Entrée ouverte : {} ({} {}x{})",
            self.url,
            self.info.map_or_else(|| "?".to_owned(), |i| i.codec.to_string()),
            self.info.map_or(0, |i| i.width),
            self.info.map_or(0, |i| i.height)
        );
        Ok(())
    }

    /// Choisit le meilleur flux vidéo et en extrait codec et résolution.
    fn reperer_flux_video(&mut self) -> Result<(), SourceError> {
        // SAFETY: `self.contexte` est ouvert ; av_find_best_stream ne fait que lire ses
        // tableaux de flux et renvoie un indice valide ou un AVERROR négatif.
        let index = unsafe {
            ff::av_find_best_stream(
                self.contexte,
                ff::AVMediaType::AVMEDIA_TYPE_VIDEO,
                -1,
                -1,
                ptr::null_mut(),
                0,
            )
        };
        if index < 0 {
            return Err(SourceError::FluxVideoAbsent { url: self.url.clone() });
        }

        // SAFETY: `index` est un indice de flux validé par av_find_best_stream ; les
        // pointeurs `streams` et `codecpar` sont garantis non nuls par libavformat pour un
        // contexte ouvert. `extradata` peut être nul (taille nulle), cas traité.
        let (id_codec, largeur, hauteur, base_temps, jeux) = unsafe {
            let flux = *(*self.contexte).streams.add(index as usize);
            let parametres = (*flux).codecpar;

            let extradata = (*parametres).extradata;
            let taille = usize::try_from((*parametres).extradata_size).unwrap_or(0);
            let jeux = if extradata.is_null() || taille == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(extradata, taille).to_vec()
            };

            (
                (*parametres).codec_id,
                (*parametres).width,
                (*parametres).height,
                (*flux).time_base,
                jeux,
            )
        };

        let codec = match id_codec {
            ff::AVCodecID::AV_CODEC_ID_H264 => VideoCodec::H264,
            ff::AVCodecID::AV_CODEC_ID_HEVC => VideoCodec::Hevc,
            autre => {
                return Err(SourceError::CodecNonSupporte(nom_codec(autre)));
            }
        };

        if jeux.is_empty() {
            log::warn!(
                "L'entrée n'annonce aucun jeu de paramètres : le décodage ne démarrera qu'à \
                 la première unité SPS/PPS reçue dans le flux."
            );
        } else {
            log::debug!("Jeux de paramètres annoncés : {} octets.", jeux.len());
        }

        self.index_video = index;
        self.base_temps = base_temps;
        self.jeux_de_parametres = jeux;
        self.info =
            Some(StreamInfo { codec, width: largeur.max(0) as u32, height: hauteur.max(0) as u32 });
        Ok(())
    }

    /// Ferme l'entrée si elle est ouverte. Idempotent.
    fn fermer_entree(&mut self) {
        if self.contexte.is_null() {
            return;
        }
        // SAFETY: `self.contexte` est un contexte ouvert par avformat_open_input ;
        // avformat_close_input le libère et remet le pointeur à null.
        unsafe {
            ff::avformat_close_input(&raw mut self.contexte);
        }
        self.contexte = ptr::null_mut();
        self.index_video = -1;
        self.info = None;
        self.jeux_de_parametres.clear();
    }

    /// Vrai si l'arrêt a été demandé par l'appelant.
    fn arret_demande(&self) -> bool {
        self.arret.load(Ordering::Relaxed)
    }

    /// Convertit un PTS exprimé dans la base de temps du flux en microsecondes.
    fn pts_en_microsecondes(&self, pts: i64) -> Option<i64> {
        if pts == ff::AV_NOPTS_VALUE {
            return None;
        }
        let microseconde = ff::AVRational { num: 1, den: 1_000_000 };
        // SAFETY: av_rescale_q est un calcul pur sur des entiers et des rationnels non nuls
        // (la base de temps d'un flux ouvert a toujours un dénominateur non nul).
        Some(unsafe { ff::av_rescale_q(pts, self.base_temps, microseconde) })
    }
}

impl VideoSource for LavfSource {
    fn stream_info(&self) -> Option<StreamInfo> {
        self.info
    }

    fn parameter_sets(&self) -> &[u8] {
        &self.jeux_de_parametres
    }

    fn next_access_unit(&mut self, unite: &mut AccessUnit) -> Result<(), SourceError> {
        if self.contexte.is_null() {
            return Err(SourceError::FluxInterrompu("entrée fermée".into()));
        }

        loop {
            // SAFETY: `self.contexte` est ouvert et `self.paquet` alloué ; av_read_frame
            // remplit le paquet, qu'on déréférence ensuite sans le conserver.
            let code = unsafe { ff::av_read_frame(self.contexte, self.paquet) };

            if code < 0 {
                // Le rappel d'interruption fait échouer la lecture : ce n'est pas une panne.
                if self.arret_demande() {
                    return Err(SourceError::PasDeDonnees);
                }
                return Err(match code {
                    ff::AVERROR_EOF => SourceError::FluxInterrompu("fin de flux".into()),
                    code if code == ff::AVERROR(ff::EAGAIN) => SourceError::PasDeDonnees,
                    code => SourceError::FluxInterrompu(message_erreur(code)),
                });
            }

            // SAFETY: le paquet est valide après un av_read_frame réussi. `data` peut être
            // nul pour un paquet vide, cas traité par `taille == 0`.
            let (index, donnees, taille, pts, dts, drapeaux) = unsafe {
                let p = &*self.paquet;
                (p.stream_index, p.data, p.size, p.pts, p.dts, p.flags)
            };

            if index != self.index_video || taille <= 0 || donnees.is_null() {
                // Flux audio/données, ou paquet vide : on le relâche et on continue.
                // SAFETY: le paquet vient d'av_read_frame et doit être déréférencé avant
                // la lecture suivante.
                unsafe { ff::av_packet_unref(self.paquet) };
                continue;
            }

            // SAFETY: `donnees` est non nul et `taille` octets sont lisibles, garanti par
            // libavformat pour un paquet valide ; la tranche ne survit pas à l'appel.
            let octets = unsafe { std::slice::from_raw_parts(donnees, taille as usize) };

            // À défaut de PTS (certains flux MPEG-TS n'en posent pas sur chaque image),
            // le DTS fait un repère acceptable pour les mesures de latence.
            let horodatage =
                self.pts_en_microsecondes(pts).or_else(|| self.pts_en_microsecondes(dts));
            let image_cle = drapeaux & ff::AV_PKT_FLAG_KEY != 0;

            unite.remplir(octets, horodatage, image_cle);

            // SAFETY: on a fini de lire le paquet ; av_packet_unref libère ses données.
            unsafe { ff::av_packet_unref(self.paquet) };
            return Ok(());
        }
    }

    fn reconnect(&mut self) -> Result<(), SourceError> {
        log::debug!("Réouverture de l'entrée {}", self.url);
        self.ouvrir_entree()
    }

    fn description(&self) -> String {
        // On retire la chaîne d'options, illisible dans un overlay.
        match self.url.split_once('?') {
            Some((base, _)) => base.to_owned(),
            None => self.url.clone(),
        }
    }
}

impl Drop for LavfSource {
    fn drop(&mut self) {
        self.fermer_entree();
        if !self.paquet.is_null() {
            // SAFETY: `self.paquet` vient d'av_packet_alloc et n'est libéré qu'ici.
            unsafe { ff::av_packet_free(&raw mut self.paquet) };
        }
    }
}

/// Rappel interrogé par FFmpeg pendant ses attentes réseau : renvoyer non nul abandonne
/// l'opération en cours.
///
/// Appelé depuis un thread FFmpeg : il ne doit rien faire d'autre que lire un atomique, et
/// ne doit jamais paniquer (une panique à travers une frontière `extern "C"` avorte le
/// processus).
unsafe extern "C" fn interruption_demandee(opaque: *mut c_void) -> c_int {
    if opaque.is_null() {
        return 0;
    }
    // SAFETY: `opaque` est le pointeur que `ouvrir_entree` a tiré de son `Arc<AtomicBool>`,
    // maintenu en vie par la `LavfSource` propriétaire du contexte qui déclenche ce rappel.
    let arret = unsafe { &*(opaque as *const AtomicBool) };
    c_int::from(arret.load(Ordering::Relaxed))
}

/// `AVDictionary` possédé, libéré automatiquement.
struct Dictionnaire(*mut ff::AVDictionary);

impl Dictionnaire {
    fn nouveau() -> Self {
        Self(ptr::null_mut())
    }

    fn poser(&mut self, cle: &str, valeur: &str) -> Result<(), SourceError> {
        let cle_c = CString::new(cle)
            .map_err(|_| SourceError::Autre(format!("clé d'option invalide : {cle}")))?;
        let valeur_c = CString::new(valeur)
            .map_err(|_| SourceError::Autre(format!("valeur d'option invalide : {valeur}")))?;

        // SAFETY: `self.0` est soit null (le dictionnaire est alors alloué par l'appel),
        // soit un dictionnaire valide ; les deux chaînes sont terminées par un nul et
        // vivent pendant l'appel (av_dict_set copie leur contenu).
        let code =
            unsafe { ff::av_dict_set(&raw mut self.0, cle_c.as_ptr(), valeur_c.as_ptr(), 0) };
        if code < 0 {
            return Err(SourceError::Autre(format!(
                "option {cle}={valeur} refusée : {}",
                message_erreur(code)
            )));
        }
        Ok(())
    }

    fn pointeur_mut(&mut self) -> *mut *mut ff::AVDictionary {
        &raw mut self.0
    }

    /// Journalise les options que libavformat n'a pas reconnues : c'est le seul moyen de
    /// repérer une faute de frappe dans `OPTIONS_COMMUNES` ou dans l'URL.
    fn journaliser_restes(&self) {
        if self.0.is_null() {
            return;
        }
        let mut entree: *const ff::AVDictionaryEntry = ptr::null();
        loop {
            // SAFETY: `self.0` est un dictionnaire valide ; av_dict_get avec AV_DICT_IGNORE_SUFFIX
            // et une clé vide itère sur toutes les entrées, et renvoie null à la fin.
            entree =
                unsafe { ff::av_dict_get(self.0, c"".as_ptr(), entree, ff::AV_DICT_IGNORE_SUFFIX) };
            if entree.is_null() {
                return;
            }
            // SAFETY: une entrée non nulle a toujours ses deux chaînes renseignées.
            let (cle, valeur) = unsafe {
                (
                    CStr::from_ptr((*entree).key).to_string_lossy().into_owned(),
                    CStr::from_ptr((*entree).value).to_string_lossy().into_owned(),
                )
            };
            log::warn!("Option ignorée par libavformat : {cle}={valeur}");
        }
    }
}

impl Drop for Dictionnaire {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` est un dictionnaire valide que nous possédons.
            unsafe { ff::av_dict_free(&raw mut self.0) };
        }
    }
}

/// Traduit un code `AVERROR` en message lisible.
fn message_erreur(code: c_int) -> String {
    let mut tampon = [0_i8; ff::AV_ERROR_MAX_STRING_SIZE];
    // SAFETY: `tampon` fait exactement AV_ERROR_MAX_STRING_SIZE octets, taille annoncée à
    // av_strerror, qui y écrit une chaîne terminée par un nul.
    let ecrit = unsafe { ff::av_strerror(code, tampon.as_mut_ptr(), tampon.len()) };
    if ecrit < 0 {
        return format!("erreur FFmpeg {code}");
    }
    // SAFETY: av_strerror a réussi, donc `tampon` contient une chaîne C valide.
    let texte = unsafe { CStr::from_ptr(tampon.as_ptr()) };
    format!("{} ({code})", texte.to_string_lossy())
}

/// Nom lisible d'un codec, pour les messages d'erreur.
fn nom_codec(id: ff::AVCodecID) -> String {
    // SAFETY: avcodec_get_name accepte n'importe quel identifiant et renvoie toujours une
    // chaîne C statique valide (« unknown_codec » au pire).
    let nom = unsafe { ff::avcodec_get_name(id) };
    if nom.is_null() {
        return format!("{id:?}");
    }
    // SAFETY: `nom` est une chaîne C statique non nulle.
    unsafe { CStr::from_ptr(nom) }.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_message_d_erreur_est_lisible() {
        let message = message_erreur(ff::AVERROR_EOF);
        assert!(message.contains("End of file") || message.contains("of file"), "{message}");
        assert!(message.contains(&ff::AVERROR_EOF.to_string()), "{message}");
    }

    #[test]
    fn le_nom_de_codec_est_rempli() {
        assert_eq!(nom_codec(ff::AVCodecID::AV_CODEC_ID_H264), "h264");
        assert_eq!(nom_codec(ff::AVCodecID::AV_CODEC_ID_HEVC), "hevc");
        assert_eq!(nom_codec(ff::AVCodecID::AV_CODEC_ID_AV1), "av1");
    }

    #[test]
    fn les_options_communes_sont_bien_formees() {
        let mut dictionnaire = Dictionnaire::nouveau();
        for (cle, valeur) in OPTIONS_COMMUNES {
            dictionnaire.poser(cle, valeur).expect("option valide");
        }
        assert!(!dictionnaire.0.is_null());
    }

    #[test]
    fn une_url_inexistante_echoue_sans_paniquer() {
        let arret = Arc::new(AtomicBool::new(false));
        // Protocole inconnu : l'ouverture doit échouer proprement, en erreur de domaine.
        let resultat = LavfSource::ouvrir("protocole-bidon://rien", arret);
        match resultat {
            Err(SourceError::Ouverture { url, .. }) => assert_eq!(url, "protocole-bidon://rien"),
            Err(autre) => panic!("erreur inattendue : {autre}"),
            Ok(_) => panic!("l'ouverture n'aurait pas dû réussir"),
        }
    }

    #[test]
    fn l_arret_interrompt_immediatement_l_ouverture() {
        // Drapeau déjà levé : le rappel d'interruption doit faire échouer l'ouverture d'un
        // socket UDP qui, sans lui, bloquerait jusqu'à l'arrivée de données.
        let arret = Arc::new(AtomicBool::new(true));
        let debut = std::time::Instant::now();
        let resultat = LavfSource::ouvrir("udp://127.0.0.1:59999?timeout=5000000", arret);
        assert!(resultat.is_err(), "l'ouverture devait être interrompue");
        assert!(debut.elapsed() < std::time::Duration::from_secs(3), "interruption trop lente");
    }

    // ------------------------------------------------------------------------------------
    // Délai de silence et coupures, sur de vrais sockets.
    //
    // Ces tests vérifient ce que la doc de `config::DELAI_SILENCE_US` affirme : sans l'option
    // `timeout` de l'URL, une entrée muette bloquerait indéfiniment et la reconnexion
    // n'aurait jamais lieu. Les deux tests de coupure émettent une vraie mire avec le
    // `ffmpeg.exe` LGPL de `third_party/ffmpeg` (libopenh264) ; ils sont sautés s'il manque.
    // ------------------------------------------------------------------------------------

    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// `ffmpeg.exe` du build LGPL téléchargé par `scripts/setup.ps1`, s'il est là.
    fn ffmpeg_exe() -> Option<std::path::PathBuf> {
        let chemin = std::path::Path::new(option_env!("FFMPEG_DIR")?).join("bin/ffmpeg.exe");
        chemin.exists().then_some(chemin)
    }

    /// Lance une petite mire H.264 vers `destination` (MPEG-TS), pendant `duree_s` secondes.
    fn lancer_mire(ffmpeg: &std::path::Path, destination: &str, duree_s: u32) -> Child {
        Command::new(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=320x240:rate=30")
            .args(["-t", &duree_s.to_string(), "-c:v", "libopenh264", "-g", "15"])
            .args(["-f", "mpegts", destination])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("lancement de ffmpeg.exe")
    }

    /// Ouvre `url` en réessayant (ffmpeg met un moment à démarrer), comme le thread vidéo.
    fn ouvrir_avec_patience(url: &str) -> LavfSource {
        let debut = Instant::now();
        loop {
            match LavfSource::ouvrir(url, Arc::new(AtomicBool::new(false))) {
                Ok(source) => return source,
                Err(erreur) if debut.elapsed() < Duration::from_secs(15) => {
                    eprintln!("ouverture en attente : {erreur}");
                }
                Err(erreur) => panic!("le flux de test n'est jamais arrivé : {erreur}"),
            }
        }
    }

    /// Lit jusqu'à la première erreur ; renvoie (unités lues, erreur, instant de la dernière
    /// unité, instant de l'erreur). `apres_n` est appelé une fois, après `n` unités.
    fn lire_jusqu_a_l_erreur(
        source: &mut LavfSource,
        n: u32,
        mut apres_n: impl FnMut(),
    ) -> (u32, SourceError, Instant, Instant) {
        let mut unite = AccessUnit::vide();
        let mut lues = 0;
        let mut derniere = Instant::now();
        loop {
            match source.next_access_unit(&mut unite) {
                Ok(()) => {
                    lues += 1;
                    derniere = Instant::now();
                    if lues == n {
                        apres_n();
                    }
                }
                Err(erreur) => return (lues, erreur, derniere, Instant::now()),
            }
            assert!(lues < 10_000, "la lecture ne s'arrête jamais");
        }
    }

    #[test]
    fn une_entree_udp_muette_echoue_dans_le_delai_de_silence() {
        // Sans `timeout`, cette ouverture bloquerait jusqu'à ce qu'on lève le drapeau.
        let debut = Instant::now();
        let resultat = LavfSource::ouvrir(
            "udp://127.0.0.1:59981?fifo_size=5000000&overrun_nonfatal=1&timeout=500000",
            Arc::new(AtomicBool::new(false)),
        );
        let ecoule = debut.elapsed();
        assert!(
            matches!(resultat, Err(SourceError::Ouverture { .. })),
            "attendu une erreur d'ouverture"
        );
        assert!(ecoule < Duration::from_secs(3), "délai de silence ignoré ({ecoule:?})");
    }

    #[test]
    fn une_coupure_udp_remonte_flux_interrompu_dans_le_delai() {
        let Some(ffmpeg) = ffmpeg_exe() else {
            eprintln!("ffmpeg.exe absent de third_party : test sauté");
            return;
        };
        let mut mire = lancer_mire(&ffmpeg, "udp://127.0.0.1:59982?pkt_size=1316", 3);
        // On date la fin de l'émission : la dernière unité, elle, n'est livrée par le
        // démultiplexeur MPEG-TS qu'au moment de l'erreur (un paquet n'est complet qu'au
        // début du suivant), elle ne peut donc pas servir de repère.
        let fin_mire = std::thread::spawn(move || {
            let _ = mire.wait();
            Instant::now()
        });
        let mut source = ouvrir_avec_patience(
            "udp://127.0.0.1:59982?fifo_size=5000000&overrun_nonfatal=1&timeout=1000000",
        );

        // La mire s'arrête seule après 3 s : on doit ensuite obtenir FluxInterrompu (et non
        // un blocage) environ une seconde plus tard.
        let (lues, erreur, _, fin) = lire_jusqu_a_l_erreur(&mut source, 0, || {});
        let fin_mire = fin_mire.join().expect("attente de la mire");

        assert!(lues >= 10, "trop peu d'unités reçues ({lues})");
        assert!(matches!(erreur, SourceError::FluxInterrompu(_)), "erreur inattendue : {erreur}");
        assert!(fin > fin_mire, "erreur avant la fin de l'émission : {erreur}");
        let silence = fin - fin_mire;
        assert!(
            silence >= Duration::from_millis(500) && silence < Duration::from_secs(4),
            "coupure détectée après {silence:?} au lieu d'environ 1 s"
        );
        eprintln!("coupure UDP détectée {silence:?} après la fin de l'émission ({erreur})");
    }

    #[test]
    fn une_coupure_srt_brutale_remonte_flux_interrompu_dans_le_delai() {
        let Some(ffmpeg) = ffmpeg_exe() else {
            eprintln!("ffmpeg.exe absent de third_party : test sauté");
            return;
        };
        // Le portable écoute, la mire appelle, comme en production.
        let mut mire = lancer_mire(
            &ffmpeg,
            "srt://127.0.0.1:59983?mode=caller&transtype=live&latency=40000",
            20,
        );
        let mut source = ouvrir_avec_patience(
            "srt://127.0.0.1:59983?mode=listener&transtype=live&latency=40000&timeout=1000000",
        );

        // On tue l'émetteur sans lui laisser fermer la connexion : c'est le cas d'un PC fixe
        // qui plante ou d'un câble débranché. SRT seul ne s'en apercevrait qu'au bout de
        // son propre délai (5 s) ; avec `timeout`, la lecture doit échouer vers 1 s.
        let mut tue_a = None;
        let (lues, erreur, _, fin) = lire_jusqu_a_l_erreur(&mut source, 20, || {
            let _ = mire.kill();
            tue_a = Some(Instant::now());
        });
        let _ = mire.wait();
        let tue_a = tue_a.expect("la mire a bien été arrêtée");

        assert!(lues >= 20, "trop peu d'unités reçues ({lues})");
        assert!(matches!(erreur, SourceError::FluxInterrompu(_)), "erreur inattendue : {erreur}");
        let delai = fin - tue_a;
        assert!(delai < Duration::from_secs(4), "coupure SRT détectée après {delai:?}");
        eprintln!("coupure SRT détectée {delai:?} après l'arrêt de l'émetteur");
    }

    #[test]
    fn l_arret_interrompt_l_ecoute_srt() {
        // En mode listener, l'attente d'un appelant n'a pas de délai : seul le drapeau
        // d'arrêt doit pouvoir y mettre fin (fermeture de la fenêtre avant tout flux).
        let arret = Arc::new(AtomicBool::new(false));
        let copie = Arc::clone(&arret);
        let fil = std::thread::spawn(move || {
            LavfSource::ouvrir(
                "srt://127.0.0.1:59984?mode=listener&transtype=live&timeout=1000000",
                copie,
            )
            .is_err()
        });
        std::thread::sleep(Duration::from_millis(300));
        let debut = Instant::now();
        arret.store(true, Ordering::Relaxed);
        assert!(fil.join().expect("le fil d'écoute doit se terminer"), "ouverture interrompue");
        assert!(debut.elapsed() < Duration::from_secs(2), "interruption trop lente");
    }

    #[test]
    fn le_rappel_d_interruption_lit_le_drapeau() {
        let arret = Arc::new(AtomicBool::new(false));
        let pointeur = Arc::as_ptr(&arret) as *mut c_void;

        // SAFETY: `pointeur` désigne l'AtomicBool vivant de `arret`, exactement comme le
        // fait `ouvrir_entree`.
        assert_eq!(unsafe { interruption_demandee(pointeur) }, 0);
        arret.store(true, Ordering::Relaxed);
        assert_eq!(unsafe { interruption_demandee(pointeur) }, 1);
        // Un pointeur nul ne doit pas déréférencer quoi que ce soit.
        assert_eq!(unsafe { interruption_demandee(ptr::null_mut()) }, 0);
    }
}
