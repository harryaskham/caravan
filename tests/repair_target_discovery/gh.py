#!/usr/bin/env python3
"""Offline gh protocol fixture; no delegation or credential output."""
import json
import os
from pathlib import Path
import sys
import time

root = Path(os.environ['FIXTURE_ROOT'])
args = sys.argv[1:]
scenario = os.environ['FIXTURE_SCENARIO']
data = json.loads((root / 'provider.json').read_text())
with (root / 'calls').open('a') as log:
    log.write(json.dumps(args) + '\n')

def emit(value):
    print(json.dumps(value))
    sys.exit(0)

def refuse(message):
    with (root / 'refusals').open('a') as log:
        log.write(json.dumps({'message': message, 'args': args}) + '\n')
    print(message, file=sys.stderr)
    sys.exit(90)

if args[:1] == ['auth']:
    sys.exit(1)  # No host account/credentials enter this fixture.
if args[:2] == ['repo', 'view']:
    if 'sshUrl' in args:
        print('git@github.com:acme/widgets.git')
        sys.exit(0)
    emit({'nameWithOwner': 'acme/widgets', 'defaultBranchRef': {'name': 'main'}})
if args[:2] == ['api', 'repos/acme/widgets/git/ref/heads/main']:
    emit({'object': {'sha': data['base']}})
if args[:2] == ['label', 'list']:
    if scenario == 'budget':
        time.sleep(2)
    emit([] if scenario == 'missing-labels' else data['labels'])
if args[:2] == ['pr', 'list']:
    if '--state' in args and args[args.index('--state') + 1] == 'merged':
        emit([])
    if '--label' in args and args[args.index('--label') + 1] == 'caravan':
        active = [data['active']]
        if scenario in ('active-target', 'target-drift'):
            target = data['target'].copy()
            target['labels'] = [{'name': 'caravan'}]
            active.append(target)
        emit(active)
    projection = args[args.index('--json') + 1] if '--json' in args else ''
    if projection == 'number,body,headRefName,headRefOid,createdAt':
        emit([data['active'], data['candidate'], data['target'], data['unrelated']])
    refuse('FORBIDDEN_FULL_ROLLUPS')
if args[:2] == ['api', 'graphql']:
    number = next((arg.split('=', 1)[1] for arg in args if arg.startswith('number=')), None)
    if number == '8':
        emit(data['candidate'])
    if number == '7':
        if scenario == 'missing-target':
            refuse('EXPLICIT_TARGET_UNAVAILABLE')
        target = data['target'].copy()
        if scenario in ('active-target', 'target-drift'):
            target['labels'] = [{'name': 'caravan'}]
        if scenario == 'target-drift':
            target['headRefOid'] = data['base']
        emit(target)
    query = next((arg[6:] for arg in args if arg.startswith('query=')), '')
    if 'pullRequests(states:OPEN' in query:
        refuse('FORBIDDEN_FULL_ROLLUPS')
    if 'defaultBranchRef' in query and 'history(first:20)' in query:
        emit({'data': {'repository': {
            'c0': None, 'c1': None,
            'defaultBranchRef': {'target': {'history': {'nodes': []}}}
        }}})
refuse('UNEXPECTED_GH_REQUEST')
