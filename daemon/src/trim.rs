//! Pure planner for the opt-in Auto-mode service trimmer.
//!
//! The planner has no side effects and no clock: the caller supplies the
//! launchd snapshot, the tracked set and whether the grace period has elapsed.
//! That keeps every safety rule testable without touching a real service.
//!
//! Invariants enforced here:
//!   * only ids in the compiled-in `auto_safe` catalog are ever acted on;
//!   * a tracked id that the user re-enabled is forgotten, never fought over;
//!   * leaving Auto / switching the feature off only ever *enables* services we
//!     disabled ourselves (the tracked set), never arbitrary ones;
//!   * at most one action per call, so a daemon tick can never disable a whole
//!     list of services in one burst.

/// One decision for the current tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do.
    None,
    /// Disable this auto-safe service (it is currently enabled).
    Disable(String),
    /// Re-enable this tracked service (it is currently disabled).
    Enable(String),
    /// Stop tracking an id that is enabled again.
    Forget(String),
}

fn enabled(states: &[(String, bool)], id: &str) -> Option<bool> {
    states
        .iter()
        .find(|(sid, _)| sid == id)
        .map(|(_, on)| *on)
}

fn is_auto_safe(auto_safe: &[&str], id: &str) -> bool {
    auto_safe.iter().any(|s| *s == id)
}

/// Decide the next trim/restore action.
///
/// `active` is true only when the user opted in *and* the cooling mode is Auto.
/// `states` is launchd's own answer; `None` means it could not be read and the
/// planner refuses to guess. `grace_elapsed` gates the first disable after the
/// feature is armed. `blocked` holds ids that recently refused (SIP), so they
/// are not retried on every tick.
pub fn plan(
    active: bool,
    states: Option<&[(String, bool)]>,
    tracked: &[String],
    auto_safe: &[&str],
    grace_elapsed: bool,
    blocked: &[String],
) -> Action {
    let states = match states {
        Some(s) => s,
        None => return Action::None,
    };

    // Validate the persisted set against the compiled-in catalog before it is
    // used for anything. A tampered state file can therefore never cause a
    // restore or a disable outside the auto-safe subset.
    let mut valid: Vec<&String> = Vec::new();
    for id in tracked {
        if is_auto_safe(auto_safe, id) && !valid.iter().any(|v| *v == id) {
            valid.push(id);
        }
    }

    if !active {
        // Leaving Auto or switching the feature off: hand back only what we
        // disabled, one service per tick. A tracked service the user already
        // re-enabled is simply forgotten.
        for id in valid {
            match enabled(states, id) {
                Some(false) => return Action::Enable(id.clone()),
                Some(true) => return Action::Forget(id.clone()),
                None => {}
            }
        }
        return Action::None;
    }

    if !grace_elapsed {
        return Action::None;
    }

    // The user manually re-enabled a trimmed service: stop tracking it instead
    // of disabling it again. This is what makes the switch non-hostile.
    for id in &valid {
        if enabled(states, id) == Some(true) {
            return Action::Forget((*id).clone());
        }
    }

    // Disable the first auto-safe service that is still on, is not already
    // tracked and has not recently refused. One per tick.
    for id in auto_safe {
        if valid.iter().any(|v| v.as_str() == *id) {
            continue;
        }
        if blocked.iter().any(|b| b == id) {
            continue;
        }
        if enabled(states, id) == Some(true) {
            return Action::Disable((*id).to_string());
        }
    }

    Action::None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAFE: &[&str] = &["com.test.a", "com.test.b", "com.test.c"];

    fn states(pairs: &[(&str, bool)]) -> Vec<(String, bool)> {
        pairs.iter().map(|(id, on)| (id.to_string(), *on)).collect()
    }

    #[test]
    fn inactive_restores_only_tracked_disabled_services() {
        let s = states(&[("com.test.a", false), ("com.test.b", true)]);
        assert_eq!(
            plan(false, Some(&s), &["com.test.a".into()], SAFE, false, &[]),
            Action::Enable("com.test.a".into())
        );
    }

    #[test]
    fn inactive_never_touches_an_untracked_service() {
        let s = states(&[("com.test.a", false), ("com.test.b", false)]);
        assert_eq!(plan(false, Some(&s), &[], SAFE, false, &[]), Action::None);
    }

    #[test]
    fn inactive_forgets_a_tracked_service_the_user_restored() {
        let s = states(&[("com.test.a", true)]);
        assert_eq!(
            plan(false, Some(&s), &["com.test.a".into()], SAFE, false, &[]),
            Action::Forget("com.test.a".into())
        );
    }

    #[test]
    fn active_waits_for_the_grace_period() {
        let s = states(&[("com.test.a", true)]);
        assert_eq!(plan(true, Some(&s), &[], SAFE, false, &[]), Action::None);
        assert_eq!(
            plan(true, Some(&s), &[], SAFE, true, &[]),
            Action::Disable("com.test.a".into())
        );
    }

    #[test]
    fn active_disables_only_auto_safe_services() {
        let s = states(&[
            ("com.test.a", true),
            ("com.test.b", true),
            ("com.manual.only", true),
        ]);
        // Even if a manual-only id is passed in the catalog, the planner only
        // sees SAFE; a caller cannot widen the set.
        assert_eq!(
            plan(true, Some(&s), &[], SAFE, true, &[]),
            Action::Disable("com.test.a".into())
        );
    }

    #[test]
    fn active_skips_services_the_user_already_disabled() {
        let s = states(&[("com.test.a", false), ("com.test.b", true)]);
        assert_eq!(
            plan(true, Some(&s), &[], SAFE, true, &[]),
            Action::Disable("com.test.b".into())
        );
    }

    #[test]
    fn active_skips_already_tracked_and_blocked_ids() {
        let s = states(&[
            ("com.test.a", false),
            ("com.test.b", true),
            ("com.test.c", true),
        ]);
        let tracked = vec!["com.test.a".to_string()];
        let blocked = vec!["com.test.b".to_string()];
        assert_eq!(
            plan(true, Some(&s), &tracked, SAFE, true, &blocked),
            Action::Disable("com.test.c".into())
        );
    }

    #[test]
    fn active_forgets_a_tracked_service_the_user_restored() {
        let s = states(&[("com.test.a", true), ("com.test.b", true)]);
        assert_eq!(
            plan(true, Some(&s), &["com.test.a".into()], SAFE, true, &[]),
            Action::Forget("com.test.a".into())
        );
    }

    #[test]
    fn tampered_tracked_ids_are_ignored() {
        let s = states(&[("com.apple.WindowServer", false), ("com.test.a", true)]);
        let tracked = vec!["com.apple.WindowServer".to_string()];
        // The tampered id is not auto-safe: it is neither restored nor used to
        // block a real trim.
        assert_eq!(
            plan(false, Some(&s), &tracked, SAFE, false, &[]),
            Action::None
        );
        assert_eq!(
            plan(true, Some(&s), &tracked, SAFE, true, &[]),
            Action::Disable("com.test.a".into())
        );
    }

    #[test]
    fn unreadable_launchd_state_means_no_action() {
        assert_eq!(plan(false, None, &["com.test.a".into()], SAFE, true, &[]), Action::None);
        assert_eq!(plan(true, None, &[], SAFE, true, &[]), Action::None);
    }

    #[test]
    fn all_trimmed_is_a_no_op() {
        let s = states(&[
            ("com.test.a", false),
            ("com.test.b", false),
            ("com.test.c", false),
        ]);
        let tracked = vec![
            "com.test.a".to_string(),
            "com.test.b".to_string(),
            "com.test.c".to_string(),
        ];
        assert_eq!(plan(true, Some(&s), &tracked, SAFE, true, &[]), Action::None);
    }
}
