//! Proof jobs: who waits, what an install reads on its retry, and what is
//! kept under which key.

use super::*;

/// The verdict of a walk that did not fit in its memory.
fn no_room() -> Walked {
    Walked {
        verdict: Verdict {
            held: vec![],
            unproved: vec!["no room".into()],
            refused: None,
        },
        fit: false,
    }
}

#[tokio::test]
async fn every_waiter_reads_a_walk_that_did_not_fit_and_a_later_install_walks_again() {
    let state = PlatformState::new(None);
    let (release, blocked) = std::sync::mpsc::channel();
    let keys = || Keys {
        model: "memory-refusal-all-waiters".into(),
        source: "memory-refusal-all-waiters-source".into(),
    };
    let (mut one, mut two) = (Attempt::default(), Attempt::default());
    let first = proof(
        &state,
        "default",
        "test",
        "test".into(),
        keys(),
        &one,
        move |_| {
            blocked.recv().unwrap();
            no_room()
        },
    )
    .await;
    let second = proof(
        &state,
        "default",
        "test",
        "test".into(),
        keys(),
        &two,
        |_| panic!("an attached install must not start another walk"),
    )
    .await;
    let Proof::Running(first) = first else {
        panic!("expected a job")
    };
    let Proof::Running(second) = second else {
        panic!("expected shared job")
    };
    release.send(()).unwrap();
    assert_eq!(first.ended(&mut one).await, Ok(()));
    assert_eq!(second.ended(&mut two).await, Ok(()));
    // Each install's retry reads the verdict it waited for: it goes ahead
    // with the rule unproved instead of walking again, and again.
    for attempt in [&one, &two] {
        let retry = proof(
            &state,
            "default",
            "test",
            "test".into(),
            keys(),
            attempt,
            |_| panic!("a retry must read the verdict it waited for"),
        )
        .await;
        let Proof::Done(verdict) = retry else {
            panic!("expected the verdict the install waited for")
        };
        assert_eq!(verdict.unproved, vec!["no room".to_string()]);
        assert_eq!(verdict.refused, None);
    }
    // A walk that did not fit says nothing of the specs: a later install
    // walks again.
    let mut three = Attempt::default();
    let later = proof(
        &state,
        "default",
        "test",
        "test".into(),
        keys(),
        &three,
        |_| Walked {
            verdict: Verdict {
                held: vec![],
                unproved: vec![],
                refused: None,
            },
            fit: true,
        },
    )
    .await;
    let Proof::Running(later) = later else {
        panic!("later install must retry")
    };
    assert_eq!(later.ended(&mut three).await, Ok(()));
}

/// A platform state with its own file database, where verdicts are kept.
async fn with_store() -> PlatformState {
    let db = std::env::temp_dir().join(format!("temper-proofs-{}.db", uuid::Uuid::new_v4()));
    let turso = temper_store_turso::TursoEventStore::new(&format!("file:{}", db.display()), None)
        .await
        .expect("the test database opens");
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    state
}

/// Keys no other test uses: walks and verdicts are shared by the process.
fn unique_keys() -> Keys {
    Keys {
        model: format!("model-{}", uuid::Uuid::new_v4()),
        source: format!("source-{}", uuid::Uuid::new_v4()),
    }
}

/// The verdict kept in the blob table under `key`.
async fn kept(state: &PlatformState, key: &str) -> Option<Vec<String>> {
    let store = state.server.platform_metadata_store().expect("a store");
    let bytes = store
        .get_blob(&blob_key(key))
        .await
        .expect("the blob reads")?;
    let verdict: Verdict = serde_json::from_slice(&bytes).expect("a kept verdict parses");
    Some(verdict.held)
}

fn holding(rule: &str) -> Verdict {
    Verdict {
        held: vec![rule.into()],
        unproved: vec![],
        refused: None,
    }
}

#[tokio::test]
async fn a_verdict_kept_under_either_key_is_read_without_a_walk_and_kept_under_both() {
    let state = with_store().await;
    let store = state.server.platform_metadata_store().expect("a store");
    for under_model in [true, false] {
        let keys = unique_keys();
        let (found, missing) = if under_model {
            (keys.model.clone(), keys.source.clone())
        } else {
            (keys.source.clone(), keys.model.clone())
        };
        let bytes = serde_json::to_vec(&holding("kept")).expect("a verdict serializes");
        store
            .put_blob(&blob_key(&found), &bytes)
            .await
            .expect("the blob is written");
        let read = proof(
            &state,
            "default",
            "test",
            "test".into(),
            keys,
            &Attempt::default(),
            |_| panic!("a kept verdict is read, not walked"),
        )
        .await;
        let Proof::Done(verdict) = read else {
            panic!("expected the kept verdict")
        };
        assert_eq!(verdict.held, vec!["kept".to_string()]);
        assert_eq!(
            kept(&state, &missing).await,
            Some(vec!["kept".to_string()]),
            "the key that missed keeps it too (kept under the model key: {under_model})"
        );
    }
}

#[tokio::test]
async fn new_keys_walk_and_only_a_verdict_with_nothing_broken_is_kept() {
    let state = with_store().await;
    let broken = Verdict {
        held: vec![],
        unproved: vec![],
        refused: Some("broken".into()),
    };
    for (verdict, stored) in [(holding("holds"), true), (broken, false)] {
        let keys = unique_keys();
        let (model, source) = (keys.model.clone(), keys.source.clone());
        let mut attempt = Attempt::default();
        let walked = verdict.clone();
        let started = proof(
            &state,
            "default",
            "test",
            "test".into(),
            keys,
            &attempt,
            move |_| Walked {
                verdict: walked,
                fit: true,
            },
        )
        .await;
        let Proof::Running(wait) = started else {
            panic!("keys nothing is kept under must walk")
        };
        assert_eq!(wait.ended(&mut attempt).await, Ok(()));
        for key in [&model, &source] {
            assert_eq!(kept(&state, key).await.is_some(), stored, "{verdict:?}");
        }
    }
}

/// Two installs whose specs differ only where the proof does not read share
/// one walk. The second's retry reads the verdict and keeps it under its own
/// source key too, so it is found after a new kernel changes the model key.
#[tokio::test]
async fn an_install_that_waited_on_another_s_walk_keeps_the_verdict_under_its_own_source_key() {
    let state = with_store().await;
    let first_keys = unique_keys();
    let second_keys = Keys {
        model: first_keys.model.clone(),
        source: format!("source-{}", uuid::Uuid::new_v4()),
    };
    let second_source = second_keys.source.clone();
    let (release, blocked) = std::sync::mpsc::channel();
    let (mut one, mut two) = (Attempt::default(), Attempt::default());
    let first = proof(
        &state,
        "default",
        "test",
        "test".into(),
        first_keys,
        &one,
        move |_| {
            blocked.recv().unwrap();
            Walked {
                verdict: holding("holds"),
                fit: true,
            }
        },
    )
    .await;
    let second = proof(
        &state,
        "default",
        "test",
        "test".into(),
        Keys {
            model: second_keys.model.clone(),
            source: second_keys.source.clone(),
        },
        &two,
        |_| panic!("an install with the same model waits on the running walk"),
    )
    .await;
    let (Proof::Running(first), Proof::Running(second)) = (first, second) else {
        panic!("expected one shared walk")
    };
    release.send(()).unwrap();
    assert_eq!(first.ended(&mut one).await, Ok(()));
    assert_eq!(second.ended(&mut two).await, Ok(()));
    assert_eq!(kept(&state, &second_source).await, None);

    let retry = proof(
        &state,
        "default",
        "test",
        "test".into(),
        second_keys,
        &two,
        |_| panic!("a retry reads the verdict it waited for"),
    )
    .await;
    assert!(matches!(retry, Proof::Done(_)));
    assert_eq!(
        kept(&state, &second_source).await,
        Some(vec!["holds".to_string()])
    );
}
