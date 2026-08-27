import { describe, expect, it } from "vitest";

import {
  ROUTER_PROOF_CASES,
  classifyLearningRoute,
  compareRouterArms,
  enforceLearningRoute,
} from "./learning-router";

describe("functional learning router", () => {
  it("classifies each labeled case as the gold route", () => {
    for (const labeled of ROUTER_PROOF_CASES) {
      const decision = classifyLearningRoute(labeled.features);
      expect(decision.route, labeled.id).toBe(labeled.goldRoute);
    }
  });

  it("corrects the recorded prompt-only FLINT discard to skill", () => {
    const flint = ROUTER_PROOF_CASES.find(
      (item) => item.id === "flint-named-contract-discarded",
    );
    expect(flint).toBeDefined();
    const decision = classifyLearningRoute(flint!.features);
    const enforced = enforceLearningRoute("discard", decision);
    expect(enforced.accepted).toBe(false);
    expect(enforced.corrected).toBe(true);
    expect(enforced.route).toBe("skill");
  });

  it("beats prompt-only routing on the published success criteria", () => {
    const proof = compareRouterArms(ROUTER_PROOF_CASES);
    expect(proof.promptOnly.routeAccuracy).toBeLessThan(1);
    expect(proof.functional.routeAccuracy).toBe(1);
    expect(proof.functional.uselessArtifacts).toBe(0);
    expect(proof.promptOnly.uselessArtifacts).toBeGreaterThan(0);
    expect(proof.functional.validatedReuse).toBeGreaterThan(
      proof.promptOnly.validatedReuse,
    );
    expect(proof.functional.heldOutReward).toBeGreaterThan(
      proof.promptOnly.heldOutReward,
    );
    expect(proof.successCriteria).toEqual({
      higherRouteAccuracy: true,
      fewerUselessArtifacts: true,
      moreValidatedReuse: true,
      equalOrBetterHeldOutReward: true,
    });
    expect(proof.proven).toBe(true);
  });
});
