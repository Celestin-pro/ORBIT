//! Attente précise pour le pacing, et priorité du thread d'envoi.
//!
//! **Seul module `unsafe` d'orbit-sender** (appels Win32), chaque bloc porte un `// SAFETY:`.
//!
//! Stratégie de [`PreciseSleeper::sleep_until`] :
//! 1. tant qu'il reste plus de 200 µs : timer Windows haute résolution
//!    (`CreateWaitableTimerExW` + `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`, Windows 10 1803+),
//!    armé pour se réveiller 200 µs avant l'échéance ;
//! 2. puis attente active (`spin_loop`) jusqu'à l'échéance.
//!
//! Si le timer haute résolution n'existe pas, repli sur un timer classique avec
//! `timeBeginPeriod(1)` (résolution 1 ms), signalé dans le journal.

use std::time::{Duration, Instant};

/// Marge laissée à l'attente active.
const SPIN_MARGIN: Duration = Duration::from_micros(200);

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
    use windows::Win32::System::Threading::{
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, GetCurrentThread, INFINITE,
        SetThreadPriority, SetWaitableTimer, THREAD_PRIORITY_HIGHEST, TIMER_ALL_ACCESS,
        WaitForSingleObject,
    };
    use windows::core::PCWSTR;

    pub struct PreciseSleeper {
        timer: Option<HANDLE>,
        high_resolution: bool,
        time_period_set: bool,
    }

    impl PreciseSleeper {
        pub fn new() -> Self {
            // SAFETY: aucun attribut de sécurité ni nom (pointeurs nuls autorisés par l'API) ;
            // le HANDLE renvoyé est possédé par cette structure et fermé dans Drop.
            let haute = unsafe {
                CreateWaitableTimerExW(
                    None,
                    PCWSTR::null(),
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ALL_ACCESS.0,
                )
            };
            if let Ok(h) = haute {
                log::info!("pacing : timer haute résolution disponible");
                return PreciseSleeper {
                    timer: Some(h),
                    high_resolution: true,
                    time_period_set: false,
                };
            }
            log::warn!(
                "pacing : timer haute résolution indisponible, repli sur timeBeginPeriod(1) \
                 (attentes moins précises)"
            );
            // SAFETY: timeBeginPeriod n'a pas de précondition ; il est compensé par
            // timeEndPeriod dans Drop si et seulement s'il a réussi (code 0 = TIMERR_NOERROR).
            let periode = unsafe { timeBeginPeriod(1) } == 0;
            // SAFETY: mêmes arguments que ci-dessus, sans le drapeau haute résolution.
            let classique =
                unsafe { CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0) };
            PreciseSleeper {
                timer: classique.ok(),
                high_resolution: false,
                time_period_set: periode,
            }
        }

        pub fn is_high_resolution(&self) -> bool {
            self.high_resolution
        }

        /// Attend jusqu'à `deadline` ; renvoie l'heure de réveil effective.
        pub fn sleep_until(&self, deadline: Instant) -> Instant {
            let reste = deadline.saturating_duration_since(Instant::now());
            if reste > SPIN_MARGIN {
                let attente = reste - SPIN_MARGIN;
                match self.timer {
                    Some(h) => {
                        // Échéance relative en unités de 100 ns, négative (convention Win32).
                        let due: i64 = -((attente.as_nanos() / 100).min(i64::MAX as u128) as i64);
                        // SAFETY: `h` est un timer valide possédé par `self` ; `due` vit
                        // pendant tout l'appel ; aucune routine de complétion.
                        let arme = unsafe { SetWaitableTimer(h, &due, 0, None, None, false) };
                        if arme.is_ok() {
                            // SAFETY: `h` reste valide pendant l'attente (fermé seulement dans
                            // Drop, qui ne peut pas s'exécuter en même temps).
                            let r = unsafe { WaitForSingleObject(h, INFINITE) };
                            if r != WAIT_OBJECT_0 {
                                std::thread::sleep(attente);
                            }
                        } else {
                            std::thread::sleep(attente);
                        }
                    }
                    None => std::thread::sleep(attente),
                }
            }
            while Instant::now() < deadline {
                std::hint::spin_loop();
            }
            Instant::now()
        }
    }

    impl Drop for PreciseSleeper {
        fn drop(&mut self) {
            if let Some(h) = self.timer.take() {
                // SAFETY: HANDLE créé par CreateWaitableTimerExW et fermé une seule fois.
                let _ = unsafe { CloseHandle(h) };
            }
            if self.time_period_set {
                // SAFETY: compense le timeBeginPeriod(1) réussi de `new`.
                let _ = unsafe { timeEndPeriod(1) };
            }
        }
    }

    /// Passe le thread courant en priorité haute (THREAD_PRIORITY_HIGHEST).
    pub fn raise_current_thread_priority() -> bool {
        // SAFETY: GetCurrentThread renvoie un pseudo-handle toujours valide pour le thread
        // appelant, qu'il ne faut pas fermer.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) }.is_ok()
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    /// Repli portable (développement hors Windows) : sommeil + attente active.
    pub struct PreciseSleeper;

    impl PreciseSleeper {
        pub fn new() -> Self {
            PreciseSleeper
        }
        pub fn is_high_resolution(&self) -> bool {
            false
        }
        pub fn sleep_until(&self, deadline: Instant) -> Instant {
            let reste = deadline.saturating_duration_since(Instant::now());
            if reste > SPIN_MARGIN {
                std::thread::sleep(reste - SPIN_MARGIN);
            }
            while Instant::now() < deadline {
                std::hint::spin_loop();
            }
            Instant::now()
        }
    }

    pub fn raise_current_thread_priority() -> bool {
        false
    }
}

pub use imp::{PreciseSleeper, raise_current_thread_priority};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveil_jamais_en_avance_et_peu_en_retard() {
        let s = PreciseSleeper::new();
        let mut pire = Duration::ZERO;
        for k in 0..50u64 {
            let echeance = Instant::now() + Duration::from_micros(300 + k * 37);
            let reveil = s.sleep_until(echeance);
            assert!(reveil >= echeance);
            pire = pire.max(reveil - echeance);
        }
        // Large marge : machine de développement chargée (compilation en parallèle).
        assert!(pire < Duration::from_millis(5), "retard maximal {pire:?}");
    }
}
