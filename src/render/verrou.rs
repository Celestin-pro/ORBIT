//! Verrou partagé entre FFmpeg et le rendu, sur l'`ID3D11DeviceContext` commun.
//!
//! Pourquoi un verrou alors que `ID3D10Multithread::SetMultithreadProtected(TRUE)` est déjà
//! activé ? Parce que les deux protègent des choses différentes :
//!
//! - la protection multithread rend **chaque appel** au device et au contexte atomique ;
//! - le verrou rend atomique une **séquence** d'appels (fixer les vues, les ressources,
//!   dessiner), et surtout la sérialise avec l'usage que FFmpeg fait du même contexte.
//!
//! `libavutil/hwcontext_d3d11va.h` prévoit explicitement ce cas : les champs `lock`,
//! `unlock` et `lock_ctx` d'`AVD3D11VADeviceContext` permettent de fournir notre propre
//! verrou, que FFmpeg prendra autour de ses accès à `device_context` et `video_context`.
//! Si on ne les fournit pas, FFmpeg utilise un mutex interne auquel nous n'avons pas accès,
//! et nos séquences de rendu ne seraient pas sérialisées avec les siennes.
//!
//! L'en-tête impose que **le verrou soit récursif**. On utilise donc une
//! `CRITICAL_SECTION` Win32, qui l'est, exactement comme l'implémentation par défaut de
//! FFmpeg.

use std::cell::UnsafeCell;
use std::ffi::c_void;

use windows::Win32::System::Threading::{
    CRITICAL_SECTION, DeleteCriticalSection, EnterCriticalSection, InitializeCriticalSection,
    LeaveCriticalSection,
};

/// Section critique récursive, partagée avec FFmpeg.
///
/// Elle est toujours manipulée derrière un `Arc` : son adresse doit rester stable, puisque
/// FFmpeg en conserve un pointeur brut (`lock_ctx`) pour toute la durée du décodage.
pub struct VerrouD3d11 {
    /// `UnsafeCell` parce que les fonctions Win32 prennent un pointeur mutable, alors que
    /// nous n'avons qu'un `&self` : c'est précisément le rôle d'une section critique que
    /// d'être mutée depuis plusieurs threads.
    section: UnsafeCell<CRITICAL_SECTION>,
}

// SAFETY: une CRITICAL_SECTION Win32 est conçue pour être utilisée depuis plusieurs threads ;
// c'est sa raison d'être. Aucune donnée n'est stockée à côté.
unsafe impl Send for VerrouD3d11 {}
unsafe impl Sync for VerrouD3d11 {}

impl VerrouD3d11 {
    pub fn nouveau() -> Self {
        let verrou = Self { section: UnsafeCell::new(CRITICAL_SECTION::default()) };
        // SAFETY: `section` vient d'être créée, n'est pas encore initialisée, et n'est
        // partagée avec personne (on détient encore `verrou` en exclusivité).
        unsafe { InitializeCriticalSection(verrou.section.get()) };
        verrou
    }

    /// Prend le verrou et le rend à la fin de la portée.
    pub fn prendre(&self) -> GardeVerrou<'_> {
        // SAFETY: `section` a été initialisée par `nouveau` et n'est détruite que dans
        // `Drop`, après que toutes les gardes ont disparu (elles emprunntent `self`).
        unsafe { EnterCriticalSection(self.section.get()) };
        GardeVerrou { verrou: self }
    }

    /// Pointeur à passer à FFmpeg comme `lock_ctx`.
    ///
    /// L'appelant doit garantir que l'`Arc` qui détient ce verrou survit au contexte FFmpeg.
    pub fn contexte_pour_ffmpeg(&self) -> *mut c_void {
        self.section.get().cast()
    }
}

impl Drop for VerrouD3d11 {
    fn drop(&mut self) {
        // SAFETY: `&mut self` prouve qu'aucune garde ne subsiste ; la section a bien été
        // initialisée. Le contrat de `contexte_pour_ffmpeg` garantit que FFmpeg n'y touche
        // plus (le contexte matériel est libéré avant).
        unsafe { DeleteCriticalSection(self.section.get()) };
    }
}

/// Garde RAII : le verrou est rendu même si le code qui suit sort par une erreur.
pub struct GardeVerrou<'a> {
    verrou: &'a VerrouD3d11,
}

impl Drop for GardeVerrou<'_> {
    fn drop(&mut self) {
        // SAFETY: cette garde a été créée par `prendre`, qui a pris la section ; on la rend
        // exactement une fois.
        unsafe { LeaveCriticalSection(self.verrou.section.get()) };
    }
}

/// Fonction `lock` passée à FFmpeg.
///
/// Appelée depuis les threads de FFmpeg ; elle ne doit jamais paniquer (une panique à
/// travers une frontière `extern "C"` avorte le processus).
pub unsafe extern "C" fn verrouiller(contexte: *mut c_void) {
    if contexte.is_null() {
        return;
    }
    // SAFETY: `contexte` est le pointeur renvoyé par `contexte_pour_ffmpeg`, c'est-à-dire
    // l'adresse d'une CRITICAL_SECTION initialisée, maintenue en vie par l'`Arc<VerrouD3d11>`
    // que le décodeur conserve aussi longtemps que le contexte matériel.
    unsafe { EnterCriticalSection(contexte.cast::<CRITICAL_SECTION>()) };
}

/// Fonction `unlock` passée à FFmpeg.
pub unsafe extern "C" fn deverrouiller(contexte: *mut c_void) {
    if contexte.is_null() {
        return;
    }
    // SAFETY: même raisonnement que `verrouiller` ; FFmpeg appelle toujours `unlock` après
    // un `lock` réussi sur le même contexte.
    unsafe { LeaveCriticalSection(contexte.cast::<CRITICAL_SECTION>()) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn le_verrou_est_recursif() {
        // Exigence explicite de hwcontext_d3d11va.h : sans cela, un appel FFmpeg imbriqué
        // bloquerait définitivement. Si cette prise double gelait, le test expirerait.
        let verrou = VerrouD3d11::nouveau();
        let externe = verrou.prendre();
        let interne = verrou.prendre();
        drop(interne);
        drop(externe);
    }

    #[test]
    fn le_verrou_exclut_bien_deux_threads() {
        let verrou = Arc::new(VerrouD3d11::nouveau());
        let compteur = Arc::new(AtomicU32::new(0));
        let simultanes = Arc::new(AtomicU32::new(0));

        let mut fils = Vec::new();
        for _ in 0..4 {
            let verrou = Arc::clone(&verrou);
            let compteur = Arc::clone(&compteur);
            let simultanes = Arc::clone(&simultanes);
            fils.push(thread::spawn(move || {
                for _ in 0..50 {
                    let _garde = verrou.prendre();
                    // Si deux threads entraient ensemble, ce compteur dépasserait 1.
                    let dedans = simultanes.fetch_add(1, Ordering::SeqCst) + 1;
                    assert_eq!(dedans, 1, "deux threads dans la section critique");
                    compteur.fetch_add(1, Ordering::SeqCst);
                    simultanes.fetch_sub(1, Ordering::SeqCst);
                }
            }));
        }
        for fil in fils {
            fil.join().expect("aucun fil ne doit paniquer");
        }
        assert_eq!(compteur.load(Ordering::SeqCst), 200);
    }

    #[test]
    fn les_rappels_ffmpeg_utilisent_la_meme_section() {
        let verrou = Arc::new(VerrouD3d11::nouveau());
        let contexte = verrou.contexte_pour_ffmpeg();

        // SAFETY: `contexte` désigne la section critique de `verrou`, encore vivant.
        unsafe { verrouiller(contexte) };

        // Un autre thread doit rester bloqué tant que le verrou est pris.
        let copie = Arc::clone(&verrou);
        let entre = Arc::new(AtomicU32::new(0));
        let entre_fil = Arc::clone(&entre);
        let fil = thread::spawn(move || {
            let _garde = copie.prendre();
            entre_fil.store(1, Ordering::SeqCst);
        });

        thread::sleep(Duration::from_millis(50));
        assert_eq!(entre.load(Ordering::SeqCst), 0, "le verrou FFmpeg n'a pas exclu le thread");

        // SAFETY: on rend la section prise juste au-dessus.
        unsafe { deverrouiller(contexte) };
        fil.join().expect("le fil doit obtenir le verrou");
        assert_eq!(entre.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn un_contexte_nul_ne_fait_rien() {
        // SAFETY: le cas nul est explicitement traité par les deux rappels.
        unsafe {
            verrouiller(std::ptr::null_mut());
            deverrouiller(std::ptr::null_mut());
        }
    }
}
