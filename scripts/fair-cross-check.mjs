#!/usr/bin/env node
// Check the fairness check against tla-ts, a TLC-compatible model checker, on
// the same small systems written by hand in both forms.
//
//   cargo run -p temper-verify --example fair_check -- \
//     --specs crates/temper-verify/tests/fair_models --models --json > verdicts.jsonl
//   npm i --prefix "$SCRATCH" tla-ts@0.0.5
//   NODE_PATH="$SCRATCH/node_modules" node scripts/fair-cross-check.mjs verdicts.jsonl \
//     crates/temper-verify/tests/fair_models
//
// `<model>.tla` has every move in Next, so it answers for `with_people`. A
// `<model>_left_alone.tla` answers for `left_alone`. Exits 1 on any difference.

import { createRequire } from "node:module";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const require = createRequire(`${process.env.NODE_PATH ?? process.cwd()}/`);
const { check } = await import(require.resolve("tla-ts"));

const [verdictsPath, dir] = process.argv.slice(2);
const config = readFileSync(join(dir, "temper.cfg"), "utf8");

const verdicts = new Map(
  readFileSync(verdictsPath, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line))
    .map((verdict) => [verdict.model, verdict]),
);

const tlaVerdict = async (file) => {
  const source = readFileSync(join(dir, file), "utf8");
  const result = await check({ source, config });
  if (result.status === "success") return { verdict: "holds", result };
  if (result.status === "liveness-violation") return { verdict: "broken", result };
  throw new Error(`${file}: tla-ts answered ${result.status}: ${result.error ?? ""}`);
};

let differs = 0;
for (const file of readdirSync(dir)
  .filter((name) => name.endsWith(".tla"))
  .sort()) {
  const name = file.replace(/\.tla$/, "");
  const leftAlone = name.endsWith("_left_alone");
  const model = leftAlone ? name.replace(/_left_alone$/, "") : name;
  const field = leftAlone ? "left_alone" : "with_people";
  const ours = verdicts.get(model);
  if (!ours) {
    console.log(`DIFF ${name}: the checker gave no verdict for ${model}`);
    differs += 1;
    continue;
  }
  const { verdict, result } = await tlaVerdict(file);
  const same = verdict === ours[field];
  differs += same ? 0 : 1;
  const states = result.statistics?.distinctStates ?? result.distinctStates ?? "?";
  console.log(
    `${same ? "same" : "DIFF"} ${name}: tla-ts ${verdict} (${states} states), temper ${field} ${ours[field]} (${ours.states} states, can-reach ${ours.can_reach})`,
  );
}
if (differs > 0) {
  console.log(`${differs} differ`);
  process.exit(1);
}
console.log("identical");
