#!/usr/bin/env python3
"""Apply/check this repository's GitHub policy using the authenticated gh CLI.

Runnerless adoption and automation are authorized separately in its UI. No App
credential, API token, fabricated check result or bypass is created here.
"""
import argparse
import json
from pathlib import Path
import subprocess


def api(path, method='GET', data=None, paginate=False):
    command = ['gh', 'api', '--method', method, path]
    if data is not None:
        command += ['--input', '-']
    if paginate:
        command += ['--paginate', '--slurp']
    reply = subprocess.run(command, input=None if data is None else json.dumps(data),
                           text=True, capture_output=True, check=True)
    value = json.loads(reply.stdout)
    return [item for page in value for item in page] if paginate else value


def contains(actual, expected):
    if isinstance(expected, dict):
        return isinstance(actual, dict) and all(k in actual and contains(actual[k], v)
                                                for k, v in expected.items())
    if isinstance(expected, list):
        return actual == expected
    return actual == expected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--apply', action='store_true', help='Apply policy; default only checks it')
    args = parser.parse_args()
    config = json.loads((Path(__file__).resolve().parents[1] / '.github/repository-policy.json').read_text())
    path = 'repos/' + config['repository']
    repository = api(path)
    if repository['id'] != config['repository_id']:
        raise SystemExit('Repository identity mismatch; refusing to change a reused name')
    ruleset = config['ruleset']
    summaries = [r for r in api(path + '/rulesets', paginate=True) if r['name'] == ruleset['name']]
    if len(summaries) > 1:
        raise SystemExit('Ambiguous ruleset name; resolve duplicate policies first')
    if args.apply:
        api(path, 'PATCH', config['settings'])
        if summaries:
            api(path + '/rulesets/' + str(summaries[0]['id']), 'PUT', ruleset)
        else:
            summaries = [api(path + '/rulesets', 'POST', ruleset)]
        repository = api(path)
    if not summaries:
        raise SystemExit('Required ruleset is missing')
    actual = api(path + '/rulesets/' + str(summaries[0]['id']))
    for name in ['target', 'enforcement', 'conditions', 'bypass_actors']:
        if actual.get(name) != ruleset[name]:
            raise SystemExit('Policy mismatch: ' + name)
    actual_rules = {r['type']: r for r in actual['rules']}
    for rule in ruleset['rules']:
        if not contains(actual_rules.get(rule['type']), rule):
            raise SystemExit('Policy mismatch: ' + rule['type'])
    if not contains(repository, config['settings']):
        raise SystemExit('Repository merge settings do not match policy')
    print('Verified: PR-only default branch, no bypass/force-push/deletion, squash merges,')
    print('and App-pinned Codex review + PR-title checks (integration 4602759).')
    print('Runnerless enrollment and current-head review check delivery require separate live verification.')


if __name__ == '__main__':
    main()
