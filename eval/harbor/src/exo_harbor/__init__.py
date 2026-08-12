"""Run Exo as a Harbor external agent.

    harbor run \\
      --env docker \\
      --n-concurrent 1 \\
      --agent exo_harbor.agent:ExoAgent \\
      --plugin exo_harbor.plugin:ExoSessionPlugin \\
      --ak exo_root=/path/to/run/exo \\
      --ak exo_bin=/path/to/target/debug/exo

Both flags are required. The agent alone would run tasks but never receive a
grade, producing a job that looks scored but measures nothing.
"""

from exo_harbor.agent import ExoAgent
from exo_harbor.plugin import ExoSessionPlugin

__all__ = ["ExoAgent", "ExoSessionPlugin"]
