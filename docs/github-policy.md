# Fork policy and Runnerless

The default branch is maintained exclusively through pull requests. The native
GitHub protect-default-branch ruleset blocks direct pushes, branch deletion and
force pushes, including for administrators. It requires resolved review threads,
linear history, and the current-head checks codex/review-gate and
ci-toolkit/pull-request-title from GitHub App integration 4602759. Human approval
count is zero because the Codex outcome is enforced by the App-owned review gate.

Squash is the only enabled merge method. The PR title becomes the commit title,
and the PR description becomes its body (PR_TITLE / PR_BODY). Automatic merging
remains disabled. Merged feature branches are cleaned up automatically; mirrored
upstream contributions use separate branches to preserve their upstream heads.

.github/repository-policy.json records the desired provider state and immutable
repository ID. Apply or verify it using:

```sh
python3 tools/github-policy.py --apply
python3 tools/github-policy.py
```

The helper changes only the named native ruleset and listed merge settings; it
preserves independent protections. Its successful readback does not prove that
Runnerless is connected or that review checks are being delivered.

## Runnerless enrollment

Connect pawelchcki/adb_client (GitHub ID 1182595109) to the existing Runnerless
workspace with an App installation covering this repository. Enable management of
the Codex review gate, with administrator bypass disabled. Request reviews using
@codex review; a request alone does not satisfy the gate. The checked-in
.runnerless-ci.ts declares conventional PR-title validation and no runtime jobs.
A proposed branch grants no application authority.

Repository enrollment must use the supported Runnerless administration flow. If
it reports admission_disabled or operator_only, an authorized Runnerless operator
must connect the repository; do not edit its database, impersonate the operator,
forge checks or relax protection. Until the App emits the required current-head
checks, GitHub deliberately blocks merging. Verify actual check delivery and a
completed Codex verdict on an open PR after enrollment. Existing upstream Actions
workflows remain intact and continue to supply their independent validation.
