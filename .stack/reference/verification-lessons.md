# Verification lessons

Reference examples for designing or debugging verification. Load the relevant example when needed; these histories do not add per-task ceremonies. Current scope and completion rules remain in AGENTS.md and implementation.md.

## Testing

- Write tests for new code - features and fixes alike. Red-green TDD.
- Run locally before pushing; failing tests get fixed before push, not leaned on CI.
- Tests alone never satisfy the Definition of Done - live e2e and QA is also required.
- **Run the artifact, do not read the description of it.** That is the root of
  everything below, and it does not stop being true when the description is a
  commit hash, a table someone else produced, a comment, a correction, or
  another agent's account of what their own commit contains. One agent read
  hashes in a message as pushed commits and raised a process concern about
  someone else's discipline on evidence it had not checked. Another read a
  commit as carrying an invariant it did not carry, from its author's
  description, and repeated it as fact. A third nearly published a correction
  that was itself wrong, having tested a superseded copy of the thing it was
  correcting - and a false correction is worse than the original error, because
  corrections are trusted more, not less. This rule was itself broken while
  being written: the author of this section told an agent both of two clauses
  were committed when one was not, and the agent found it only because it was
  asked to check rather than to confirm.
- **A path is not an identity, and building the comparison destroys one side of
  it.** Proving a fix works by running the fixed build and the broken build
  against the same input is the right shape, and both builds write to the same
  target path, so the second overwrites the first. An agent did exactly this,
  copied the pre-fix binary aside, and handed a verifier two paths that were by
  then byte-identical pre-fix builds - anyone re-running from the handoff would
  have watched the fix panic and concluded it did not work. Copy aside the build
  you intend to keep BEFORE building its variant, and identify each by something
  inside it - a string only that version contains, a hash you recorded at build
  time - never by where it sits. The same check rescues the evidence after the
  fact: logs naming the two mutually exclusive messages each identify the binary
  that wrote them, which is why the recorded runs survived a handoff that did
  not.
- **Running the right artifact is not enough if you ask it a question that cannot
  fail.** A cache fix was checked by response time: after the drop the page came
  back in 0.08s, which reads as a hit and was a cheap rebuild over warm caches
  underneath. Fast and stale are indistinguishable from outside, so the
  measurement passed while the thing it stood for failed, and the fix - which
  never reached the render at all - was pushed and would have merged. What
  separated them was a probe printing when the backend was actually read. Before
  trusting a number, ask what it would look like if the change had done nothing.
  This is the variant that ships, because every instinct says you verified
  properly: you ran the real thing, in the real environment, and read a real
  number. It is also why an independent verifier is worth its cost even when you
  did drive the page: **driving finds what you thought to do, and a verifier
  finds what the change made possible.** The agent above drove that map at two
  viewports and saw nothing, because it looked at the view it knew - and the
  regression existed only at a zoom its own fix had introduced. The author's own summary is the one to keep: *I did not have an
  instrument, and I reported as though I did.* So the rule is not run it, it is
  know what your instrument can see - a stopwatch is a real instrument honestly
  read, and it cannot see the difference between a fast rebuild and a hit.
- **Changing a constant silently invalidates everything calibrated against it,
  and this one has no green check to distrust.** Raising a zoom ceiling from 3.2
  to a value derived from the data made five absolute sizes wrong at once - two
  literal font sizes inside a scaled layer, and three clamps whose floors had
  been *correct* when written, calibrated against the ceiling that moved. Four
  were found by looking at the screen; the fifth, a halo stroke drawing at 48px
  behind a 104px word, could not be, because it did not read as a separate
  defect. So when you change a constant, grep for what was tuned against it
  rather than waiting for someone to notice - and note that reading the tuned
  code will not save you: an agent looked straight at two of those clamps, judged
  them deliberate, and left them, because **reading a clamp in isolation tells
  you what its author intended and not what it now does.** Both were deliberate,
  against the old ceiling. This is not an instrument reporting
  on what it cannot see - nothing here was ever wrong until the constant moved -
  which is why nothing was suspicious and why it is worth naming separately.
- **Fix a data bug in every instrument that reads that field, not where you
  noticed it.** A status field lived on the row and also inside a projection of
  it; one script read the wrong one, counting 17 archived rows as live. It was
  fixed in the script that *counts* and left in the script that *writes* - so a
  link into an archived cell passed silently on the write path, while the read
  path looked correct. Worse, the guard that appeared to hold there held by luck:
  it refused the archived parent for being unattested rather than archived, which
  is true only while no archived row happens to be attested. Grep for every
  reader of the field before you call it fixed.
- **Do not measure an effort with an instrument the effort changed.** A before
  and an after taken with different versions of the same script are two
  instruments, not one measurement, and the direction of the difference does not
  save you: a repair pass improved its own checker six times and its headline
  pair had the before scored strictly and the after leniently. The fix is to
  score **both states with both scripts** and read down a column - which also
  tells you which figures are instrument-independent and which are not. Where the
  states are recoverable, reconstruct the old one and prove the reconstruction
  (applying the forward change to it must reproduce the current bytes exactly)
  rather than asserting it. A figure whose two columns agree is measuring the
  thing; one whose columns diverge is partly measuring the tool.
- **A check that pins exact text asserts what its author last saw, not what the
  rule says.** One pinned the wording of a function's return statement and went
  red when a third guard was added to it - it noticed the edit, which is what it
  is for, and it was wrong about it. Assert the property: read the condition and
  require each guard to still gate it, so the check survives a guard being added
  and still fails when one is removed. Test both directions before you believe
  it.
- **Read a row across, never a diagonal.** When a change reports a before and an
  after, both numbers must come from the same measurement, and the way to be sure
  is to compute every measure at both reads and print the table. An effort
  reported an improvement as "168 to 131" where 168 was one definition of the
  before and 131 a different definition of the after - one number from each
  column of a two-by-two. Nothing reconciled, because the two figures never
  described the same thing. It survived a report, a pull request body and two
  separate corrections, and was twice explained away as data drift by people
  hunting for a cause that did not exist. The honest pair was 165 to 131.
- **A check is worth what it fails on. Before you push one, write the mutation
  that should break it and run it.** Red-green covers a test written before the
  code. This covers everything written after: an assertion, a lint, a guard, a
  watch, a comment claiming a loop terminates. Those are never observed failing
  unless someone makes them fail, and an unobserved check reports green whether
  or not it is still looking at anything. Nine surfaced in one Katagami effort
  (ARN-118) on 2026-09-09, in nine unrelated pieces of code: a security test that
  matched read names and went silent when one was renamed, so a backend read
  moved above the owner gate passed 8 of 8; an overlap assertion true of its
  160-cell fixture and false of the 744-cell library, which then vouched for a
  false comment about the loop it tested; a fixture of one row, which cannot be
  read short, backing two tests written to detect a short read; a voice checker
  skipping every exemplar because all of them fell under its 150-word floor; a
  viewport measured in an effect that ran once; and `git push` reporting
  "Everything up-to-date" while the branch ref had never moved. Every one was
  green while it was wrong. The mutation costs about four minutes per check.
- Corollaries, each earned the same day. A check with a threshold reports what it
  declined to evaluate; a run whose skipped count equals its input count is a
  finding, not a pass. A check fails when its subject disappears, not only when
  its subject is wrong - one that silently skips what it cannot find reports a
  pass for something it stopped watching. A band or threshold derived from an
  unrepresentative corpus measures the corpus rather than the thing, in both
  directions; the tell is a threshold no reasonable input can sit inside, and the
  fix is the corpus, not the number. And **a deleted check cannot fail** - a
  suite that is green because there is less of it looks exactly like one that is
  green because the code is right. Two suite-level tests were dropped in a
  refactor that day and the test run said nothing, because a test that no longer
  exists cannot report; the lint caught it, as an unused import.
- **Never give a check a phrase that silences it.** An excuse the machine
  verifies is a check; an excuse the machine reads is a comment. Three times in
  one effort a check shipped with a way to wave itself off: a flag that
  suppressed a short read of any collection including the one under test, and a
  prose excuse a verifier defeated by pasting the excusing sentence onto a row
  whose source answered fine. The third was caught before it shipped, by its own
  author: it had labelled each mismatch disclosed or silent by keyword, then
  deleted the label and printed the cell's own explanation instead, because the
  keyword test misread a real case and, in its words, a magic word that silences
  a check is a bad thing to teach. Where a check genuinely cannot judge, print
  the evidence and let a person read it; do not let the subject of a check write
  the sentence that exempts it. When an exemption is genuinely needed, make it
  **data keyed above the thing being checked** rather than prose on the thing
  itself: the same effort replaced a per-row prose excuse with a per-host list
  carrying a dated measurement, and the reason it closes the attack is not the
  measurement but the key - a per-row hatch can be aimed at the one row someone
  wants through, a per-host list cannot.
- **A machine record's named predicates are not text.** If the source is
  structured and the code does a substring search over the flattened bytes, the
  question has been asked in the wrong shape, whether or not it currently returns
  the right answer. One check searched a record's JSON for a parent id without
  asking which predicate it sat under, so an id appearing under a *narrower*
  relation, a related-term link, or a change note would have passed. Run both
  ways before you conclude anything: there it happened to agree on all 453 links,
  so it was right by luck rather than by construction. Flattening structure to
  text is natural enough that the author of that check made the move again
  minutes after diagnosing it in someone else's code.
- Two rules about fixing a check, both earned the same day. Do not narrow the
  aperture of a check that matched the wrong thing - three people in a row wrote
  a name-matching guard to fix a name-matching guard, each closing the previous
  one's blindness by matching more names, and the version that worked matched
  none. And when a check has a limit you cannot close, name the limit in the file
  rather than the workaround: a comment claiming a gap is inherent when it is a
  variant choice is worse than the gap, because it stops the next person fixing
  what is already fixable.

