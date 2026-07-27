import { defineHarness } from "@exo/harness";

import { runResponsesHarnessTurn } from "@exo/model-runtime/turn-loop";

const harness = defineHarness({
  tools: [],

  async runTurn(context) {
    await runResponsesHarnessTurn(context);
  },
});

export default harness;
