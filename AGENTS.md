# Pull request workflow

Work on feature branches and open pull requests against main. Never push changes
directly to main. Use Conventional Commit PR titles and describe the final change,
its rationale and validation in the PR description.

The default branch permits squash merges only, with the PR title and description
used as the squash commit title and body. All review conversations must be
resolved. Administrators have no bypass; force pushes and branch deletion are
blocked on main.

Require codex/review-gate and ci-toolkit/pull-request-title from GitHub App
integration 4602759 on the current PR head. Request a real Codex review using
@codex review and address proven findings. Never fabricate passing checks, weaken
protection or bypass review to merge. Runnerless enrollment is an administrative
operation; committing configuration does not enroll the repository.

When mirroring an upstream PR, use a separate mirror branch so fork branch cleanup
cannot delete the source branch of an open upstream contribution. Preserve the
upstream link and document any fork-only changes.
