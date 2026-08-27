import { writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { ROUTER_PROOF_CASES, compareRouterArms } from "./learning-router";

const proof = compareRouterArms(ROUTER_PROOF_CASES);
const destination = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../../eval/harbor/evidence/router-proof.json",
);
writeFileSync(destination, `${JSON.stringify(proof, null, 2)}\n`);
process.stdout.write(`${destination}\n`);
process.stdout.write(`${JSON.stringify(proof, null, 2)}\n`);
if (!proof.proven) {
  process.exit(2);
}
