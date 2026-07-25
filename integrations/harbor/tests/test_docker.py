import unittest

from exo_harbor.docker import compose_project_name


class ComposeProjectNameTest(unittest.TestCase):
    def test_matches_harbor_sanitization(self) -> None:
        self.assertEqual(
            compose_project_name("_Task.Name__AbC__env"),
            "0_task-name__abc__env",
        )
