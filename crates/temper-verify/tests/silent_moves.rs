//! A move that stays in place, has no guard and writes nothing a rule reads
//! is left out of the digest a verdict is kept by, and the kept verdict
//! stands for a spec with it only where its places agree with the walk the
//! verdict kept.

use temper_verify::{CascadeLevel, CascadeResult, VerificationCascade};

/// A ticket noted at most twice, then closed. `Archived` is never reached.
const TICKET: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "Closed", "Archived"]
initial = "Open"

[[state]]
name = "notes"
type = "counter"
initial = 0

[[state]]
name = "memo"
type = "string"
initial = ""

[[action]]
name = "Note"
kind = "input"
from = ["Open"]
guard = "notes < 2"
effect = ["notes += 1"]

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[invariant]]
name = "few"
assert = "notes <= 2"
"#;

/// A move from `from` that writes `memo`, which no rule reads.
fn remark(from: &str) -> String {
    format!(
        "\n[[action]]\nname = \"Remark\"\nkind = \"input\"\nfrom = [\"{from}\"]\nparams = [{{ name = \"memo\", type = \"string\" }}]\n"
    )
}

/// The check `validate-ioa` runs when its caller names no settings.
fn check(ioa: &str) -> VerificationCascade {
    VerificationCascade::from_ioa(ioa)
        .with_sim_seeds(5)
        .with_prop_test_cases(100)
}

/// Whether `spec`'s key finds `kept`'s verdict and it stands for `spec`.
fn keeps(kept: &CascadeResult, kept_spec: &str, spec: &str) -> bool {
    let key = check(spec).kept_key().expect("the fixture parses");
    assert_eq!(
        key.digest,
        check(kept_spec).kept_key().unwrap().digest,
        "the two differ only in a silent move"
    );
    key.fits(kept)
}

#[test]
fn a_silent_move_keeps_the_verdict() {
    let kept = check(TICKET).run();
    assert!(kept.all_passed, "{kept:?}");
    let remarked = TICKET.to_string() + &remark("Open");
    assert!(keeps(&kept, TICKET, &remarked));
    assert!(check(&remarked).run().all_passed);
}

/// Each of these keeps the digest, and its own check refuses it: the kept
/// verdict must not stand for it.
#[test]
fn a_silent_move_its_places_break_does_not_keep_the_verdict() {
    let kept = check(TICKET).run();
    let dead = TICKET.to_string() + &remark("Archived");
    let terminal = TICKET.replace(
        "initial = \"Open\"",
        "initial = \"Open\"\nterminal = [\"Closed\"]",
    );
    let kept_terminal = check(&terminal).run();
    let closed_remark = terminal.clone() + &remark("Closed");
    for (name, kept, kept_spec, spec) in [
        ("from a place never reached", &kept, TICKET, dead.as_str()),
        (
            "from a terminal place",
            &kept_terminal,
            terminal.as_str(),
            closed_remark.as_str(),
        ),
    ] {
        assert!(kept.all_passed, "{name}: {kept:?}");
        assert!(
            !check(spec).run().all_passed,
            "{name}: its check refuses it"
        );
        assert!(!keeps(kept, kept_spec, spec), "{name}");
    }
}

/// `Closed` must offer a move. The kept spec's only one there is silent;
/// without it the record is stuck in `Closed`.
#[test]
fn removing_the_only_move_out_of_a_place_does_not_keep_the_verdict() {
    let live = "\n[[liveness]]\nname = \"moves\"\nfrom = [\"Closed\"]\nhas_actions = true\n";
    let kept_spec = TICKET.to_string() + live + &remark("Closed");
    let kept = check(&kept_spec).run();
    assert!(kept.all_passed, "{kept:?}");
    let stuck = TICKET.to_string() + live;
    assert!(!check(&stuck).run().all_passed);
    assert!(!keeps(&kept, &kept_spec, &stuck));
}

/// A simulation counts walks that end short of a `reaches` target, and a
/// move that stays in place makes more of them, so such a spec's moves are
/// all in its digest.
#[test]
fn a_spec_with_a_reaches_liveness_keeps_every_move_in_its_digest() {
    let reaches = TICKET.to_string()
        + "\n[[liveness]]\nname = \"ends\"\nfrom = [\"Open\"]\nreaches = [\"Closed\"]\n";
    let remarked = reaches.clone() + &remark("Open");
    assert_ne!(
        check(&reaches).kept_key().unwrap().digest,
        check(&remarked).kept_key().unwrap().digest
    );
}

#[test]
fn a_move_with_a_guard_a_read_write_or_a_new_place_is_in_the_digest() {
    let digest = |spec: &str| check(spec).kept_key().unwrap().digest;
    let base = digest(TICKET);
    let open = remark("Open");
    for (name, changed) in [
        (
            "a guard",
            open.replace("params =", "guard = \"notes < 1\"\nparams ="),
        ),
        (
            "a read write",
            open.replace(
                "params = [{ name = \"memo\", type = \"string\" }]",
                "effect = [\"notes += 1\"]",
            ),
        ),
        (
            "a new place",
            open.replace("from = [\"Open\"]", "from = [\"Open\"]\nto = \"Closed\""),
        ),
    ] {
        assert_ne!(digest(&(TICKET.to_string() + &changed)), base, "{name}");
    }
}

/// A verdict kept before its walk's places were kept was found by a digest
/// that held every move, so it stands only for a spec with no silent moves.
#[test]
fn a_verdict_kept_without_its_places_stands_only_without_silent_moves() {
    let mut kept = check(TICKET).run();
    for level in &mut kept.levels {
        if level.level == CascadeLevel::ModelCheck {
            level.verification.as_mut().unwrap().reached = None;
        }
    }
    assert!(check(TICKET).kept_key().unwrap().fits(&kept));
    let remarked = TICKET.to_string() + &remark("Open");
    assert!(!keeps(&kept, TICKET, &remarked));
}

/// A field declared that no rule reads, with a move that writes it, keeps
/// the verdict: no level looks its kind up.
#[test]
fn a_new_field_no_rule_reads_keeps_the_verdict() {
    let kept = check(TICKET).run();
    let tagged = TICKET.to_string()
        + "\n[[state]]\nname = \"tags\"\ntype = \"counter\"\ninitial = 0\n\n[[action]]\nname = \"Tag\"\nkind = \"input\"\nfrom = [\"Open\"]\neffect = [\"tags += 1\"]\n";
    assert!(keeps(&kept, TICKET, &tagged));
    assert!(check(&tagged).run().all_passed);
}

/// `Closed` has no move out and no liveness asks for one, so a silent move
/// out of it changes nothing a level reads.
#[test]
fn a_silent_move_out_of_a_place_no_liveness_watches_keeps_the_verdict() {
    let kept = check(TICKET).run();
    let remarked = TICKET.to_string() + &remark("Closed");
    assert!(check(&remarked).run().all_passed);
    assert!(keeps(&kept, TICKET, &remarked));
}

/// A silent move leaves every route open, so a passing `can_reach` verdict
/// can stand for a spec with that move too.
#[test]
fn a_silent_move_keeps_a_can_reach_verdict() {
    let route = TICKET.to_string()
        + "\n[[liveness]]\nname = \"can_close\"\nfrom = [\"Open\"]\ncan_reach = [\"Closed\"]\n";
    let kept = check(&route).run();
    assert!(kept.all_passed, "{kept:?}");
    let remarked = route.clone() + &remark("Open");
    assert!(keeps(&kept, &route, &remarked));
    assert!(check(&remarked).run().all_passed);
}

/// The same whole walk must retain both its reached-place evidence and
/// the refusal path when a non-silent move closes the last route.
#[test]
fn a_walk_with_silent_moves_keeps_its_can_reach_refusal() {
    let route = TICKET.replace(
        "name = \"Close\"",
        "name = \"Close\"\nguard = \"notes == 0\"",
    ) + "\n[[liveness]]\nname = \"can_close\"\nfrom = [\"Open\"]\ncan_reach = [\"Closed\"]\n"
        + &remark("Open");
    let result = check(&route).run();
    assert!(!result.all_passed, "{result:?}");
    let l1 = result
        .levels
        .iter()
        .find(|level| level.level == CascadeLevel::ModelCheck)
        .unwrap()
        .verification
        .as_ref()
        .unwrap();
    assert!(l1.is_complete);
    let reached = l1.reached.as_ref().expect("the whole walk kept its places");
    assert_eq!(reached.places, ["Closed", "Open"]);
    assert_eq!(reached.left_silently, ["Open"]);
    let refusal = l1
        .counterexamples
        .iter()
        .find(|example| example.property == "CanReach")
        .expect("a closed route remains a refusal");
    assert_eq!(refusal.violated, ["can_close"]);
    assert_eq!(refusal.never_reaches, ["Closed"]);
    assert_eq!(refusal.trace.len(), 2);
    assert_eq!(refusal.trace[0].1.as_ref().unwrap().name, "Note");
    let last = &refusal.trace[1];
    assert_eq!(last.0.status, "Open");
    assert_eq!(last.0.counters["notes"], 1);
    assert!(last.1.is_none());
}
