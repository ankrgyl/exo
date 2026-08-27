from __future__ import annotations

import unittest
from pathlib import Path

from exo_harbor.router_quality import (
    better_than_prompt_only,
    load_gold_labels,
    persisted_keep_routes,
    score_router_quality,
)


GOLD = Path(__file__).parents[1] / "gold-labels/learning-router-transfer-test.json"


def trial(
    task: str,
    *,
    persisted: dict[str, int] | None = None,
    promotions: list[dict] | None = None,
    activated: list[dict] | None = None,
    reused: list[dict] | None = None,
) -> dict:
    return {
        "task_name": task,
        "route_counts": {
            route: {
                "attempted": count,
                "succeeded": count,
                "failed": 0,
                "unresolved": 0,
            }
            for route, count in (persisted or {}).items()
        },
        "lifecycle": {"promotions": promotions or []},
        "task_usage": {
            "learning_artifacts_activated": activated or [],
            "skills_reused_from_prior_tasks": reused or [],
            "agent_tools_reused_from_prior_tasks": [],
        },
    }


class RouterQualityTest(unittest.TestCase):
    def test_gold_labels_cover_the_transfer_protocol(self) -> None:
        gold = load_gold_labels(GOLD)
        self.assertEqual(gold["dataset"], "learning-router-transfer-test")
        self.assertEqual(
            list(gold["tasks"]),
            [
                "exo/learning-router-normalization-first",
                "exo/learning-router-normalization-transfer",
                "exo/learning-router-unrelated-control",
            ],
        )

    def test_prompt_only_flint_discard_loses_to_lifecycle_skill(self) -> None:
        gold = load_gold_labels(GOLD)
        baseline = {
            "trials": [
                trial(
                    "exo/learning-router-normalization-first",
                    persisted={},
                    promotions=[
                        {
                            "route": "discard",
                            "status": "discarded",
                        }
                    ],
                ),
                trial("exo/learning-router-normalization-transfer"),
                trial("exo/learning-router-unrelated-control"),
            ]
        }
        candidate = {
            "trials": [
                trial(
                    "exo/learning-router-normalization-first",
                    promotions=[
                        {
                            "route": "skill",
                            "status": "promoted",
                        }
                    ],
                ),
                trial(
                    "exo/learning-router-normalization-transfer",
                    activated=[{"id": "learn-1", "route": "skill"}],
                    reused=[{"name": "flint-normalization"}],
                ),
                trial("exo/learning-router-unrelated-control"),
            ]
        }

        baseline_quality = score_router_quality(baseline, gold)
        candidate_quality = score_router_quality(candidate, gold)
        proof = better_than_prompt_only(
            baseline=baseline_quality,
            candidate=candidate_quality,
        )

        self.assertEqual(baseline_quality["route_accuracy"], 0.0)
        self.assertEqual(candidate_quality["route_accuracy"], 1.0)
        self.assertEqual(candidate_quality["validated_reuse"], 1)
        self.assertEqual(candidate_quality["false_activation"], 0)
        self.assertTrue(proof["proven"])

    def test_memory_only_learn_is_an_incorrect_flint_route(self) -> None:
        gold = load_gold_labels(GOLD)
        summary = {
            "trials": [
                trial(
                    "exo/learning-router-normalization-first",
                    persisted={"memory": 1},
                )
            ]
        }
        quality = score_router_quality(summary, gold)
        learn = quality["tasks"][0]
        self.assertEqual(learn["persisted_routes"], ["memory"])
        self.assertFalse(learn["route_correct"])
        self.assertTrue(learn["useless_artifact"])

    def test_lifecycle_promotions_are_preferred_over_route_counts(self) -> None:
        trial_row = trial(
            "learn",
            persisted={"memory": 1},
            promotions=[{"route": "skill", "status": "promoted"}],
        )
        self.assertEqual(persisted_keep_routes(trial_row), {"skill"})


if __name__ == "__main__":
    unittest.main()
