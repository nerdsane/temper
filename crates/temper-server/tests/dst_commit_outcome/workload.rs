//! The seeded move workload, sharing the runtime clock with timer deadlines.

use super::*;

pub(super) fn run_seed(seed: u64) -> Result<usize, Vec<String>> {
    let clock = std::sync::Arc::new(process_clock::ProcessClock::new());
    let _guard = temper_runtime::scheduler::install_sim_context(
        clock.clone(),
        std::sync::Arc::new(temper_runtime::scheduler::DeterministicIdGen::new(seed)),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime builds");
    let store = SimEventStore::no_faults(seed);
    let mut rng = DeterministicRng::new(seed ^ 0xc0_33_17);
    let mut run = Run {
        seed,
        trace: Vec::new(),
        noted: BTreeMap::new(),
    };
    let ended = runtime.block_on(async {
        clock.begin();
        let state = server(&store, seed);
        let tenant = TenantId::new(TENANT);
        let mut status: BTreeMap<String, String> = BTreeMap::new();
        for i in 0..RECORDS {
            let id = format!("t-{i}");
            state
                .get_or_create_tenant_entity(
                    &tenant,
                    "Tally",
                    &id,
                    json!({ "Id": id, "Code": format!("code-{i}") }),
                )
                .await
                .map_err(|error| run.fail("records are created", error.to_string()))?;
            status.insert(id, "Open".to_string());
        }
        let mut faulted = 0;
        // Per record: the `Hold`s that landed, and those of them whose
        // outcome could not be learned.
        let mut holds: BTreeMap<String, (i64, i64)> = BTreeMap::new();
        for n in 0..MOVES {
            let id = format!("t-{}", rng.next_bound(RECORDS));
            let journal = journal_of(&id);
            let action = match (status[&id].as_str(), rng.next_bound(3)) {
                (_, 0) => "Bump",
                ("Open", _) => "Hold",
                _ => "Release",
            };
            let mark = format!("m-{n}");
            let fault = Fault::draw(&mut rng);
            let hang_up = match rng.next_bound(10) {
                0 | 1 => Some(HangUp::AfterTurns(rng.next_bound(40))),
                2 | 3 => Some(HangUp::AfterCommit),
                _ => None,
            };
            fault.apply(&store, &journal);
            faulted += usize::from(fault != Fault::None);

            let commits_before = store.commits(&journal);
            let answer = move_once(&state, &store, &id, action, &mark, hang_up).await;
            // A hung-up move's actor keeps going; let its late writes land.
            tokio::time::sleep(Duration::from_secs(1)).await;
            settle().await;
            Fault::clear(&store, &journal);
            run.trace.push(format!(
                "{mark}: {action} {id}, {fault:?}, hang up {hang_up:?}: {answer:?}"
            ));

            let landings = landed(&store, &id, &mark).await;
            if action == "Hold" && landings == 1 {
                // Its caller was answered "outcome unknown", or would have
                // been: the journal read after its commit failed.
                let unknown = fault == Fault::LandThenFailUnreadable
                    && !matches!(answer, Answer::Success);
                let entry = holds.entry(id.clone()).or_default();
                entry.0 += 1;
                entry.1 += i64::from(unknown);
            }
            if landings > 1 {
                return Err(run.fail(
                    "a move lands at most once",
                    format!("{mark} landed {landings} times"),
                ));
            }
            let commits = store.commits(&journal) - commits_before;
            if commits > 1 {
                run.note(
                    "a move takes one commit",
                    format!("{mark} took {commits} commits"),
                );
            }
            check_rows(&mut run, &store, &id).await;
            match &answer {
                Answer::Success if landings != 1 => {
                    return Err(run.fail(
                        "a successful answer is a move that landed",
                        format!("{mark} answered success, landed {landings} times"),
                    ));
                }
                Answer::Refused(error) | Answer::Failed(error) if landings != 0 => {
                    let unreadable = fault == Fault::LandThenFailUnreadable;
                    if !unreadable {
                        return Err(run.fail(
                            "a failed answer is a move that did not land",
                            format!("{mark} answered {error:?} and landed"),
                        ));
                    }
                }
                _ => {}
            }

            {
                let (sequence, bumps, journal_status) = journal_state(&store, &id).await;
                let memory = state
                    .get_tenant_entity_state(&tenant, "Tally", &id)
                    .await
                    .map_err(|error| run.fail("the record reads", error))?;
                let memory_bumps = memory.state.counters.get("bumps").copied().unwrap_or(0);
                if memory.state.sequence_nr != sequence
                    || memory_bumps as i64 != bumps
                    || memory.state.status != journal_status
                {
                    return Err(run.fail(
                        "the record's state in memory is the state its journal gives",
                        format!(
                            "{id} after {mark}: memory seq {} bumps {memory_bumps} {}, journal seq {sequence} bumps {bumps} {journal_status}",
                            memory.state.sequence_nr, memory.state.status
                        ),
                    ));
                }
                status.insert(id.clone(), journal_status);
            }
        }

        // A day later every timer has fired: no record waits in `Busy`.
        tokio::time::sleep(Duration::from_secs(86_400 + 60)).await;
        settle().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        settle().await;
        for id in (0..RECORDS).map(|i| format!("t-{i}")) {
            let (_, _, journal_status) = journal_state(&store, &id).await;
            // Each end check is noted and the others still run, so one broken
            // invariant does not hide another.
            if journal_status == "Busy" {
                run.note(
                    "every timer fires, whoever hung up",
                    format!("{id} is still Busy a day later"),
                );
            }
            check_rows(&mut run, &store, &id).await;
            let (landed_holds, unknown) = holds.get(&id).copied().unwrap_or_default();
            let notes = noted(&store, &id).await;
            if notes != landed_holds && notes != landed_holds - unknown {
                run.note(
                    "every landed hold has its scheduled note, whoever hung up",
                    format!("{id}: {landed_holds} holds landed ({unknown} unknown), {notes} notes"),
                );
            }
            let counted = ledger_count(&state, &id).await;
            if counted != landed_holds - unknown {
                run.note(
                    "every landed hold has its reaction, whoever hung up",
                    format!(
                        "{id}: {landed_holds} holds landed ({unknown} unknown), ledger counts {counted}"
                    ),
                );
            }
        }
        clock.end(Duration::ZERO);
        Ok(faulted)
    });
    let mut broken: Vec<String> = run.noted.into_values().collect();
    match ended {
        Ok(faulted) if broken.is_empty() => Ok(faulted),
        Ok(_) => Err(broken),
        Err(report) => {
            broken.insert(0, report);
            Err(broken)
        }
    }
}
