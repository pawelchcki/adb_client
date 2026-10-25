"""Regression coverage for policy discovery across GitHub API pages."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("github_policy", Path(__file__).with_name("github-policy.py"))
policy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(policy)


class PolicyPaginationTests(unittest.TestCase):
    def test_api_consumes_all_pages_including_later_matching_ruleset(self):
        pages = [[{"id": 1, "name": "other"}],
                 [{"id": 2, "name": "protect-default-branch"}]]
        reply = SimpleNamespace(stdout=json.dumps(pages))
        with patch.object(policy.subprocess, "run", return_value=reply) as run:
            rows = policy.api("repos/owner/repository/rulesets", paginate=True)
        self.assertEqual([row["id"] for row in rows], [1, 2])
        command = run.call_args.args[0]
        self.assertIn("--paginate", command)
        self.assertIn("--slurp", command)

    def test_verification_finds_target_on_later_page(self):
        cfg = json.loads((Path(__file__).resolve().parents[1] / ".github/repository-policy.json").read_text())
        calls = []

        def api(path, method="GET", data=None, paginate=False):
            calls.append((path, method, paginate))
            self.assertEqual(method, "GET")
            if path.endswith("/rulesets"):
                self.assertTrue(paginate)
                return [{"id": 1, "name": "other"}, {"id": 2, "name": cfg["ruleset"]["name"]}]
            if path.endswith("/rulesets/2"):
                return cfg["ruleset"]
            return dict(cfg["settings"], id=cfg["repository_id"])

        with patch.object(policy, "api", side_effect=api), patch("sys.argv", ["github-policy.py"]), contextlib.redirect_stdout(io.StringIO()):
            policy.main()
        self.assertTrue(any(path.endswith("/rulesets/2") for path, _, _ in calls))

    def test_duplicate_names_across_pages_abort_before_mutations(self):
        cfg = json.loads((Path(__file__).resolve().parents[1] / ".github/repository-policy.json").read_text())

        def api(path, method="GET", data=None, paginate=False):
            self.assertEqual(method, "GET", "ambiguous discovery must not mutate GitHub")
            if path.endswith("/rulesets"):
                self.assertTrue(paginate)
                return [{"id": 1, "name": cfg["ruleset"]["name"]},
                        {"id": 2, "name": cfg["ruleset"]["name"]}]
            return dict(cfg["settings"], id=cfg["repository_id"])

        with patch.object(policy, "api", side_effect=api), patch("sys.argv", ["github-policy.py", "--apply"]):
            with self.assertRaisesRegex(SystemExit, "Ambiguous"):
                policy.main()


if __name__ == "__main__":
    unittest.main()
