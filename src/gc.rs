//! Cross-authority garbage-collection ordering.
//!
//! Lifecycle owns managed-worker collection and protection policy; store owns
//! legacy filesystem safety. This module is the only place that composes them.

use crate::lifecycle::{GcControl, GcProtectionSnapshot, LifecycleStore};
use crate::store::LegacyGc;
use std::path::Path;
use std::time::SystemTime;

pub(crate) fn run(now: SystemTime, self_id: Option<&str>) -> Result<usize, String> {
    run_prepared(
        LegacyGc::prepare()?,
        now,
        self_id,
        || {
            LifecycleStore::default()
                .gc_unmanaged_excluding(now, GcControl::RunToCompletion, self_id)
                .map(|result| result.removed_candidates)
        },
        |root| {
            let snapshot = GcProtectionSnapshot::load(root)?;
            Ok(move |id: &str| snapshot.protects_session(id))
        },
    )
}

fn run_prepared<M, L, P>(
    legacy: Option<LegacyGc>,
    now: SystemTime,
    self_id: Option<&str>,
    collect_managed: M,
    load_protection: L,
) -> Result<usize, String>
where
    M: FnOnce() -> Result<usize, String>,
    L: FnOnce(&Path) -> Result<P, String>,
    P: FnMut(&str) -> Result<bool, String>,
{
    let Some(legacy) = legacy else {
        return Ok(0);
    };
    if legacy.preflight_throttled(now)? {
        return Ok(0);
    }
    let managed_removed = collect_managed()?;
    let legacy_removed = legacy.collect(now, self_id, load_protection)?;
    Ok(managed_removed + legacy_removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;
    use std::collections::HashMap;
    use std::fs::{self, FileTimes};
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex, mpsc};
    use std::thread;
    use std::time::Duration;
    use tinyjson::JsonValue;

    const GC_DAYS: u64 = 14;
    type ProtectionFn = fn(&str) -> Result<bool, String>;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!("relay-gc-{label}-{}", store::uuid_v4()));
            fs::create_dir_all(root.join("mailbox")).expect("create GC fixture");
            Self(root)
        }

        fn root(&self) -> &Path {
            &self.0
        }

        fn old_mailbox(&self, id: &str) -> PathBuf {
            let path = self.0.join("mailbox").join(format!("{id}.jsonl"));
            fs::write(&path, b"{\"old\":true}\n").expect("seed old mailbox");
            let old = SystemTime::now() - Duration::from_secs((GC_DAYS + 1) * 24 * 60 * 60);
            let times = FileTimes::new().set_modified(old).set_accessed(old);
            fs::File::options()
                .read(true)
                .write(true)
                .open(&path)
                .expect("open old mailbox")
                .set_times(times)
                .expect("age old mailbox");
            path
        }

        fn legacy(&self) -> LegacyGc {
            LegacyGc::for_test(&self.0, GC_DAYS).expect("prepare fixture GC")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn coordinator_runs_managed_before_legacy_and_stamps_after_both_succeed() {
        let fixture = Fixture::new("order");
        let id = "11111111-1111-4111-8111-111111111111";
        let mailbox = fixture.old_mailbox(id);
        let events = Mutex::new(Vec::new());
        let now = SystemTime::now();

        let removed = run_prepared(
            Some(fixture.legacy()),
            now,
            None,
            || {
                events.lock().unwrap().push("managed");
                assert!(!fixture.root().join("gc-stamp").exists());
                Ok(2)
            },
            |_| {
                events.lock().unwrap().push("protection");
                assert!(!fixture.root().join("gc-stamp").exists());
                Ok(|_: &str| Ok(false))
            },
        )
        .expect("coordinated GC succeeds");

        assert_eq!(removed, 3);
        assert_eq!(*events.lock().unwrap(), ["managed", "protection"]);
        assert!(!mailbox.exists());
        assert!(fixture.root().join("gc-stamp").is_file());
    }

    #[test]
    fn fresh_preflight_stamp_skips_both_collectors() {
        let fixture = Fixture::new("preflight");
        fs::write(fixture.root().join("gc-stamp"), "fresh\n").unwrap();

        let removed = run_prepared(
            Some(fixture.legacy()),
            SystemTime::now(),
            None,
            || -> Result<usize, String> { panic!("managed GC must remain throttled") },
            |_| -> Result<ProtectionFn, String> {
                panic!("legacy protection must remain throttled")
            },
        )
        .expect("fresh stamp is a successful no-op");

        assert_eq!(removed, 0);
    }

    #[test]
    fn managed_error_aborts_before_legacy_and_omits_stamp() {
        let fixture = Fixture::new("managed-error");
        let id = "22222222-2222-4222-8222-222222222222";
        let mailbox = fixture.old_mailbox(id);
        let protection_loads = AtomicUsize::new(0);

        let error = run_prepared(
            Some(fixture.legacy()),
            SystemTime::now(),
            None,
            || Err("managed GC failed".to_string()),
            |_| {
                protection_loads.fetch_add(1, Ordering::SeqCst);
                Ok(|_: &str| Ok(false))
            },
        )
        .unwrap_err();

        assert_eq!(error, "managed GC failed");
        assert_eq!(protection_loads.load(Ordering::SeqCst), 0);
        assert!(mailbox.exists());
        assert!(!fixture.root().join("gc-stamp").exists());
    }

    #[test]
    fn managed_phase_protection_is_loaded_fresh_under_the_legacy_lock() {
        let fixture = Fixture::new("fresh-protection");
        let id = "33333333-3333-4333-8333-333333333333";
        let mailbox = fixture.old_mailbox(id);
        let root = fixture.root().to_path_buf();
        let protection_loads = AtomicUsize::new(0);

        let removed = run_prepared(
            Some(fixture.legacy()),
            SystemTime::now(),
            None,
            || {
                let mut bindings = HashMap::new();
                bindings.insert(id.to_string(), JsonValue::from(HashMap::new()));
                let mut authority = HashMap::new();
                authority.insert("session_bindings".to_string(), JsonValue::from(bindings));
                store::write_lifecycle_authority_at(&root, authority)?;
                Ok(0)
            },
            |locked_root| {
                protection_loads.fetch_add(1, Ordering::SeqCst);
                let snapshot = GcProtectionSnapshot::load(locked_root)?;
                Ok(move |session_id: &str| snapshot.protects_session(session_id))
            },
        )
        .expect("fresh lifecycle protection preserves legacy data");

        assert_eq!(removed, 0);
        assert_eq!(protection_loads.load(Ordering::SeqCst), 1);
        assert!(mailbox.exists());
        assert!(fixture.root().join("gc-stamp").is_file());
    }

    #[test]
    fn unknown_lifecycle_protection_fails_closed_without_stamp() {
        let fixture = Fixture::new("unknown-protection");
        let id = "44444444-4444-4444-8444-444444444444";
        let mailbox = fixture.old_mailbox(id);
        fs::write(
            fixture.root().join("lifecycle-v1.json"),
            format!(
                "{{\"schema_version\":\"1\",\"state\":{{\"future_session_protection\":{{\"id\":\"{id}\"}}}}}}"
            ),
        )
        .unwrap();

        let error = run_prepared(
            Some(fixture.legacy()),
            SystemTime::now(),
            None,
            || Ok(0),
            |locked_root| {
                let snapshot = GcProtectionSnapshot::load(locked_root)?;
                Ok(move |session_id: &str| snapshot.protects_session(session_id))
            },
        )
        .unwrap_err();

        assert!(error.contains("unknown state key future_session_protection"));
        assert!(mailbox.exists());
        assert!(!fixture.root().join("gc-stamp").exists());
    }

    #[test]
    fn losing_runner_rechecks_winner_stamp_before_legacy_work() {
        let fixture = Fixture::new("race");
        let removed_id = "55555555-5555-4555-8555-555555555555";
        let preserved_id = "66666666-6666-4666-8666-666666666666";
        let removed_mailbox = fixture.old_mailbox(removed_id);
        let preserved_mailbox = fixture.old_mailbox(preserved_id);
        let winner_gc = fixture.legacy();
        let loser_gc = fixture.legacy();
        let now = SystemTime::now();
        let both_missed_preflight = Arc::new(Barrier::new(2));
        let (winner_done_tx, winner_done_rx) = mpsc::channel();
        let (release_loser_tx, release_loser_rx) = mpsc::channel();
        let loser_protection_loads = Arc::new(AtomicUsize::new(0));

        let winner_barrier = Arc::clone(&both_missed_preflight);
        let winner = thread::spawn(move || {
            let result = run_prepared(
                Some(winner_gc),
                now,
                None,
                || {
                    winner_barrier.wait();
                    Ok(0)
                },
                |_| Ok(move |id: &str| Ok(id == preserved_id)),
            );
            winner_done_tx.send(result).unwrap();
        });

        let loser_barrier = Arc::clone(&both_missed_preflight);
        let loser_loads = Arc::clone(&loser_protection_loads);
        let loser = thread::spawn(move || {
            run_prepared(
                Some(loser_gc),
                now,
                None,
                || {
                    loser_barrier.wait();
                    release_loser_rx.recv().unwrap();
                    Ok(0)
                },
                |_| {
                    loser_loads.fetch_add(1, Ordering::SeqCst);
                    Ok(|_: &str| Ok(false))
                },
            )
        });

        assert_eq!(winner_done_rx.recv().unwrap().unwrap(), 1);
        let stamp = fixture.root().join("gc-stamp");
        let winner_stamp = fs::read(&stamp).unwrap();
        let winner_stamp_ino = fs::metadata(&stamp).unwrap().ino();
        release_loser_tx.send(()).unwrap();
        let loser_removed = loser.join().unwrap().unwrap();
        winner.join().unwrap();

        assert_eq!(loser_removed, 0);
        assert_eq!(loser_protection_loads.load(Ordering::SeqCst), 0);
        assert!(!removed_mailbox.exists());
        assert!(preserved_mailbox.exists());
        assert_eq!(fs::read(&stamp).unwrap(), winner_stamp);
        assert_eq!(fs::metadata(&stamp).unwrap().ino(), winner_stamp_ino);
    }
}
