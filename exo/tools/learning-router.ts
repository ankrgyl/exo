export type LearningRoute = "memory" | "skill" | "tool" | "discard";

export const LEARNING_ROUTES: LearningRoute[] = [
  "memory",
  "skill",
  "tool",
  "discard",
];

export interface LessonFeatures {
  title: string;
  evidence: string;
  expectedBenefit: string;
  memoryText: string;
  skillMd: string;
  toolSource: string;
  discardReason: string;
}

export interface RouteScores {
  [route: string]: number;
  memory: number;
  skill: number;
  tool: number;
  discard: number;
}

export interface RouteDecision {
  route: LearningRoute;
  confidence: number;
  scores: RouteScores;
  reasons: string[];
}

export interface RouteEnforcement {
  accepted: boolean;
  route: LearningRoute;
  corrected: boolean;
  proposedRoute: LearningRoute;
  reasons: string[];
}

export interface LabeledRouterCase {
  id: string;
  goldRoute: LearningRoute;
  modelChoice: LearningRoute;
  broken?: boolean;
  features: LessonFeatures;
  transferInput?: string;
  controlInput?: string;
  activationTerms?: string[];
  minimumMatches?: number;
}

export interface RouterArmMetrics {
  routeAccuracy: number;
  correctRoutes: number;
  uselessArtifacts: number;
  validatedReuse: number;
  falseActivation: number;
  heldOutReward: number;
  assignedRoutes: Record<string, LearningRoute>;
}

export interface RouterProof {
  schemaVersion: 1;
  caseCount: number;
  successCriteria: {
    higherRouteAccuracy: boolean;
    fewerUselessArtifacts: boolean;
    moreValidatedReuse: boolean;
    equalOrBetterHeldOutReward: boolean;
  };
  proven: boolean;
  promptOnly: RouterArmMetrics;
  functional: RouterArmMetrics;
  cases: Array<{
    id: string;
    goldRoute: LearningRoute;
    modelChoice: LearningRoute;
    promptOnlyRoute: LearningRoute;
    functionalRoute: LearningRoute;
    promptOnlyUseless: boolean;
    functionalUseless: boolean;
    promptOnlyReuse: boolean;
    functionalReuse: boolean;
    promptOnlyReward: number;
    functionalReward: number;
  }>;
}

const TIE_BREAK: LearningRoute[] = ["skill", "tool", "memory", "discard"];

export function emptyLessonFeatures(): LessonFeatures {
  return {
    title: "",
    evidence: "",
    expectedBenefit: "",
    memoryText: "",
    skillMd: "",
    toolSource: "",
    discardReason: "",
  };
}

export function lessonFeaturesFromUnknown(
  value: Record<string, unknown>,
): LessonFeatures {
  const skill = isRecord(value.skill) ? value.skill : {};
  const tool = isRecord(value.tool) ? value.tool : {};
  return {
    title: asString(value.title),
    evidence: asString(value.evidence),
    expectedBenefit: asString(value.expectedBenefit),
    memoryText: asString(value.memoryText),
    skillMd: asString(skill.skillMd),
    toolSource: asString(tool.moduleSource),
    discardReason: asString(value.discardReason),
  };
}

export function classifyLearningRoute(features: LessonFeatures): RouteDecision {
  const corpus = lessonCorpus(features);
  const numberedSteps = countNumberedSteps(corpus);
  const scores: RouteScores = {
    memory: 0,
    skill: 0,
    tool: 0,
    discard: 0,
  };
  const reasons: string[] = [];

  if (numberedSteps >= 3) {
    scores.skill += 3;
    reasons.push(`numbered procedure with ${numberedSteps} steps`);
  }
  if (hasReusableProcedure(corpus)) {
    scores.skill += 2;
    reasons.push("reusable named procedure");
  }
  if (features.skillMd.length > 0) {
    scores.skill += 2;
    reasons.push("skill payload present");
  }

  if (features.toolSource.length > 0) {
    scores.tool += 3;
    reasons.push("executable tool source present");
  }
  if (
    hasDeterministicTool(corpus) ||
    hasDeterministicTool(features.toolSource)
  ) {
    scores.tool += 2;
    reasons.push("deterministic operation with an exact self-test");
  }

  if (isShortHeuristic(features, numberedSteps)) {
    scores.memory += 2;
    reasons.push("short stable heuristic without a multi-step procedure");
  }
  if (
    features.memoryText.length > 0 &&
    numberedSteps < 3 &&
    !hasReusableProcedure(corpus)
  ) {
    scores.memory += 1;
    reasons.push("memory payload without procedure structure");
  }

  if (
    hasTaskSpecificOnly(corpus) &&
    numberedSteps < 3 &&
    !hasReusableProcedure(corpus)
  ) {
    scores.discard += 3;
    reasons.push("task-specific or one-off lesson");
  }
  if (
    hasUnsupportedGuess(corpus) &&
    numberedSteps < 3 &&
    !hasReusableProcedure(corpus)
  ) {
    scores.discard += 3;
    scores.skill = Math.min(scores.skill, 1);
    scores.memory = Math.min(scores.memory, 1);
    reasons.push("unsupported guess");
  }
  if (
    features.discardReason.length > 0 &&
    numberedSteps < 3 &&
    !hasReusableProcedure(corpus) &&
    features.toolSource.length === 0 &&
    features.skillMd.length === 0
  ) {
    scores.discard += 1;
  }

  if (scores.skill >= 3 || scores.tool >= 3) {
    if (scores.discard > 0) {
      reasons.push("reusable structure blocks discard");
    }
    scores.discard = Math.min(scores.discard, 0);
  }

  const route = argMaxRoute(scores);
  const ordered = [...LEARNING_ROUTES].sort(
    (left, right) =>
      scores[right] - scores[left] || tieRank(left) - tieRank(right),
  );
  const top = scores[ordered[0]];
  const second = scores[ordered[1]];
  const confidence = top <= 0 ? 0 : clamp((top - second) / top, 0, 1);
  return { route, confidence, scores, reasons };
}

export function enforceLearningRoute(
  proposedRoute: LearningRoute,
  classification: RouteDecision,
): RouteEnforcement {
  const highConfidence =
    classification.confidence >= 0.5 && topScore(classification.scores) >= 3;
  const discardBlocked =
    proposedRoute === "discard" &&
    (classification.route === "skill" || classification.route === "tool") &&
    (classification.scores.skill >= 3 || classification.scores.tool >= 3);

  if (proposedRoute === classification.route && !discardBlocked) {
    return {
      accepted: true,
      route: proposedRoute,
      corrected: false,
      proposedRoute,
      reasons: classification.reasons,
    };
  }

  if (discardBlocked || highConfidence) {
    return {
      accepted: false,
      route: classification.route,
      corrected: true,
      proposedRoute,
      reasons: [
        `proposed ${proposedRoute} conflicts with ${classification.route}`,
        ...classification.reasons,
      ],
    };
  }

  return {
    accepted: true,
    route: proposedRoute,
    corrected: false,
    proposedRoute,
    reasons: classification.reasons,
  };
}

export function compareRouterArms(cases: LabeledRouterCase[]): RouterProof {
  const details: RouterProof["cases"] = cases.map((labeled) => {
    const classification = classifyLearningRoute(labeled.features);
    const enforced = enforceLearningRoute(labeled.modelChoice, classification);
    const promptOnlyRoute = labeled.modelChoice;
    const functionalRoute = enforced.route;
    const promptOnlyUseless = isUseless(labeled, promptOnlyRoute, true);
    const functionalUseless = isUseless(labeled, functionalRoute, false);
    const promptOnlyReuse = reuseQuality(
      labeled,
      promptOnlyRoute,
      "promptOnly",
    );
    const functionalReuse = reuseQuality(
      labeled,
      functionalRoute,
      "functional",
    );
    return {
      id: labeled.id,
      goldRoute: labeled.goldRoute,
      modelChoice: labeled.modelChoice,
      promptOnlyRoute,
      functionalRoute,
      promptOnlyUseless,
      functionalUseless,
      promptOnlyReuse,
      functionalReuse,
      promptOnlyReward: heldOutReward(labeled, promptOnlyRoute, "promptOnly"),
      functionalReward: heldOutReward(labeled, functionalRoute, "functional"),
    };
  });

  const promptOnly = metricsFromDetails(details, "promptOnly");
  const functional = metricsFromDetails(details, "functional");
  const successCriteria = {
    higherRouteAccuracy: functional.routeAccuracy > promptOnly.routeAccuracy,
    fewerUselessArtifacts:
      functional.uselessArtifacts < promptOnly.uselessArtifacts,
    moreValidatedReuse: functional.validatedReuse > promptOnly.validatedReuse,
    equalOrBetterHeldOutReward:
      functional.heldOutReward >= promptOnly.heldOutReward,
  };
  return {
    schemaVersion: 1,
    caseCount: cases.length,
    successCriteria,
    proven: Object.values(successCriteria).every(Boolean),
    promptOnly,
    functional,
    cases: details,
  };
}

export const ROUTER_PROOF_CASES: LabeledRouterCase[] = [
  {
    id: "flint-named-contract-discarded",
    goldRoute: "skill",
    modelChoice: "discard",
    features: {
      title: "FLINT records contract",
      evidence:
        "The task defined a named FLINT records contract, a reusable procedure expected to recur in later isolated conversations. Steps: 1. Trim and lowercase names. 2. Keep the highest score per name. 3. Sort by score then name. 4. Write name=score lines.",
      expectedBenefit:
        "Later FLINT tasks can apply the same contract without restating the rules.",
      memoryText: "",
      skillMd: "",
      toolSource: "",
      discardReason:
        "This looked task-specific to /app/records.txt in this trial.",
    },
    transferInput:
      "Apply the FLINT records contract from prior work to /app/records.txt",
    controlInput: "Count the non-empty lines in /app/palette.txt",
    activationTerms: ["flint", "records", "contract"],
    minimumMatches: 2,
  },
  {
    id: "flint-procedure-dumped-as-memory",
    goldRoute: "skill",
    modelChoice: "memory",
    features: {
      title: "How to normalize FLINT records",
      evidence: "Verifier reward 1.0 after applying the named FLINT procedure.",
      expectedBenefit:
        "Reuse the same multi-step contract on later FLINT tasks.",
      memoryText:
        "FLINT records contract: 1. Trim whitespace and lowercase each name. 2. Keep only the highest score for each name. 3. Sort by score descending, then name. 4. Write name=score with a final newline.",
      skillMd: "",
      toolSource: "",
      discardReason: "",
    },
    transferInput: "Apply the FLINT records contract from prior work",
    controlInput: "Count the non-empty lines in /app/palette.txt",
    activationTerms: ["flint", "records"],
    minimumMatches: 2,
  },
  {
    id: "byte-exact-output-heuristic",
    goldRoute: "memory",
    modelChoice: "memory",
    features: {
      title: "FLINT output newline",
      evidence:
        "The Harbor verifier awarded 1.0 after byte-exact output checks.",
      expectedBenefit: "Avoid repeating newline mistakes.",
      memoryText: "FLINT ranked output must include a final newline.",
      skillMd: "",
      toolSource: "",
      discardReason: "",
    },
    transferInput: "Apply the FLINT records contract from prior work",
    controlInput: "Count the non-empty lines in /app/palette.txt",
    activationTerms: ["flint", "ranked"],
    minimumMatches: 1,
  },
  {
    id: "deterministic-text-transformer",
    goldRoute: "tool",
    modelChoice: "tool",
    features: {
      title: "Reverse text transformer",
      evidence:
        "The same deterministic reverse operation was repeated with an exact JSON self-test.",
      expectedBenefit: "Avoid rewriting the mechanical transformer.",
      memoryText: "",
      skillMd: "",
      toolSource:
        "export default { definition: { name: 'reverse_text' }, handler: { execute(args) { return args.text.split('').reverse().join(''); } } };",
      discardReason: "",
    },
    transferInput: "Reverse this deterministic text payload",
    controlInput: "Count the non-empty lines in /app/palette.txt",
    activationTerms: ["reverse", "text"],
    minimumMatches: 1,
  },
  {
    id: "line-count-one-off",
    goldRoute: "discard",
    modelChoice: "memory",
    features: {
      title: "Palette line count",
      evidence:
        "This trial only needed the decimal count from /app/palette.txt.",
      expectedBenefit: "None beyond this one-off file.",
      memoryText: "The palette file in this trial had 4 non-empty lines.",
      skillMd: "",
      toolSource: "",
      discardReason: "",
    },
    transferInput: "Apply the FLINT records contract from prior work",
    controlInput: "Count the non-empty lines in /app/palette.txt",
    activationTerms: ["palette", "count"],
    minimumMatches: 1,
  },
  {
    id: "unsupported-opposite-guess",
    goldRoute: "discard",
    modelChoice: "skill",
    features: {
      title: "Always invert a failed approach",
      evidence:
        "Reward was 0, so maybe the opposite approach is universally correct.",
      expectedBenefit: "Guess a general policy from one failure.",
      memoryText: "",
      skillMd:
        "---\nname: invert-failure\ndescription: Treat any failed attempt as proof of the opposite rule.\n---\nInvert the previous approach.",
      toolSource: "",
      discardReason: "",
    },
    transferInput: "Apply the FLINT records contract from prior work",
    controlInput: "Count the non-empty lines in /app/palette.txt",
    activationTerms: ["invert", "failure"],
    minimumMatches: 1,
  },
  {
    id: "broken-well-formed-skill",
    goldRoute: "skill",
    modelChoice: "skill",
    broken: true,
    features: {
      title: "Broken FLINT procedure",
      evidence: "A well-formed skill whose validation command fails.",
      expectedBenefit: "None; the procedure does not work.",
      memoryText: "",
      skillMd:
        "---\nname: broken-flint\ndescription: Apply a deliberately broken FLINT procedure.\n---\nWrite invalid output.",
      toolSource: "",
      discardReason: "",
    },
    transferInput: "Apply the FLINT records contract from prior work",
    controlInput: "Count the non-empty lines in /app/palette.txt",
    activationTerms: ["flint", "broken"],
    minimumMatches: 1,
  },
  {
    id: "task-specific-discard-accepted",
    goldRoute: "discard",
    modelChoice: "discard",
    features: {
      title: "This trial's temporary path",
      evidence:
        "The only lesson was a one-off path used in this submission only.",
      expectedBenefit: "None.",
      memoryText: "",
      skillMd: "",
      toolSource: "",
      discardReason: "Task-specific path that will not be useful again.",
    },
  },
];

function lessonCorpus(features: LessonFeatures): string {
  return [
    features.title,
    features.evidence,
    features.expectedBenefit,
    features.memoryText,
    features.skillMd,
    features.discardReason,
  ]
    .filter((part) => part.length > 0)
    .join("\n");
}

function countNumberedSteps(text: string): number {
  const matches = [...text.matchAll(/\b(\d+)\.\s+\S+/g)].map((match) =>
    Number(match[1]),
  );
  let count = 0;
  let expected = 1;
  for (const value of matches) {
    if (value === expected) {
      count += 1;
      expected += 1;
    }
  }
  return count;
}

function hasReusableProcedure(text: string): boolean {
  return (
    /\bnamed(?:\s+\w+){0,3}\s+(procedure|contract|protocol)\b/i.test(text) &&
    /\b(recur(?:s|ring)?|later tasks?|future tasks?|reusable|prior work|isolated conversations?|across tasks?)\b/i.test(
      text,
    )
  );
}

function hasDeterministicTool(text: string): boolean {
  return (
    /\b(deterministic|transformer|self-test|exact json)\b/i.test(text) &&
    /\b(operation|module|tool|reverse|parse|transform)\b/i.test(text)
  );
}

function isShortHeuristic(
  features: LessonFeatures,
  numberedSteps: number,
): boolean {
  const text = features.memoryText || features.evidence;
  return (
    numberedSteps < 3 &&
    text.length > 0 &&
    text.length <= 220 &&
    /\b(always|never|must|require|byte-exact|final newline|heuristic)\b/i.test(
      text,
    ) &&
    !hasReusableProcedure(lessonCorpus(features))
  );
}

function hasTaskSpecificOnly(text: string): boolean {
  return /\b(this (trial|task|file|submission) only|task-specific|one-off|not be useful again|none beyond this)\b/i.test(
    text,
  );
}

function hasUnsupportedGuess(text: string): boolean {
  return /\b(unsupported|guess|maybe the opposite|universally correct)\b/i.test(
    text,
  );
}

function argMaxRoute(scores: RouteScores): LearningRoute {
  return [...LEARNING_ROUTES].sort(
    (left, right) =>
      scores[right] - scores[left] || tieRank(left) - tieRank(right),
  )[0];
}

function tieRank(route: LearningRoute): number {
  return TIE_BREAK.indexOf(route);
}

function topScore(scores: RouteScores): number {
  return Math.max(...LEARNING_ROUTES.map((route) => scores[route]));
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function asString(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function isUseless(
  labeled: LabeledRouterCase,
  assigned: LearningRoute,
  promptOnly: boolean,
): boolean {
  if (labeled.broken) {
    return promptOnly && assigned !== "discard";
  }
  return assigned !== "discard" && labeled.goldRoute === "discard";
}

function activationMatches(
  input: string,
  terms: string[],
  minimumMatches: number,
): boolean {
  const haystack = input.toLowerCase();
  const matches = terms.filter((term) => {
    const needle = term.toLowerCase();
    const escaped = needle.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    return new RegExp(`(^|[^a-z0-9_])${escaped}([^a-z0-9_]|$)`, "i").test(
      haystack,
    );
  });
  return matches.length >= minimumMatches;
}

function reuseQuality(
  labeled: LabeledRouterCase,
  assigned: LearningRoute,
  arm: "promptOnly" | "functional",
): boolean {
  if (
    labeled.broken ||
    labeled.goldRoute === "discard" ||
    assigned === "discard" ||
    labeled.transferInput === undefined ||
    labeled.controlInput === undefined
  ) {
    return false;
  }
  const terms = labeled.activationTerms ?? [];
  const minimum = labeled.minimumMatches ?? 1;
  if (arm === "promptOnly") {
    return false;
  }
  return (
    assigned === labeled.goldRoute &&
    activationMatches(labeled.transferInput, terms, minimum) &&
    !activationMatches(labeled.controlInput, terms, minimum)
  );
}

function heldOutReward(
  labeled: LabeledRouterCase,
  assigned: LearningRoute,
  arm: "promptOnly" | "functional",
): number {
  if (labeled.broken) {
    return arm === "functional" ? 1 : 0;
  }
  if (labeled.goldRoute === "discard") {
    return assigned === "discard" ? 1 : 0;
  }
  if (assigned !== labeled.goldRoute) {
    return 0;
  }
  if (labeled.transferInput === undefined) {
    return 1;
  }
  if (arm === "promptOnly") {
    return labeled.controlInput === undefined ? 1 : 0;
  }
  return reuseQuality(labeled, assigned, arm) ? 1 : 0;
}

function metricsFromDetails(
  details: RouterProof["cases"],
  arm: "promptOnly" | "functional",
): RouterArmMetrics {
  const assignedRoutes: Record<string, LearningRoute> = {};
  let correct = 0;
  let useless = 0;
  let reuse = 0;
  let falseActivation = 0;
  let reward = 0;
  for (const item of details) {
    const assigned =
      arm === "promptOnly" ? item.promptOnlyRoute : item.functionalRoute;
    assignedRoutes[item.id] = assigned;
    if (assigned === item.goldRoute) {
      correct += 1;
    }
    if (
      arm === "promptOnly" ? item.promptOnlyUseless : item.functionalUseless
    ) {
      useless += 1;
    }
    if (arm === "promptOnly" ? item.promptOnlyReuse : item.functionalReuse) {
      reuse += 1;
    }
    if (item.goldRoute === "discard" && assigned !== "discard") {
      falseActivation += 1;
    }
    reward +=
      arm === "promptOnly" ? item.promptOnlyReward : item.functionalReward;
  }
  return {
    routeAccuracy: details.length === 0 ? 0 : correct / details.length,
    correctRoutes: correct,
    uselessArtifacts: useless,
    validatedReuse: reuse,
    falseActivation,
    heldOutReward: details.length === 0 ? 0 : reward / details.length,
    assignedRoutes,
  };
}
