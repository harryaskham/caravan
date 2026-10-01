#!/usr/bin/env python3
"""Isolated protocol peer: never delegates to a real gh or a network endpoint."""
import json
import os
import pathlib
import sys

root = pathlib.Path(os.environ['CLOSE_FIXTURE_ROOT'])
args = sys.argv[1:]
with (root / 'calls.jsonl').open('a') as stream:
    stream.write(json.dumps(args) + '\n')


def reply(value):
    print(json.dumps(value))
    raise SystemExit(0)


if args[:2] == ['repo', 'view']:
    if 'viewerPermission' in args:
        reply({'viewerPermission': 'WRITE'})
    reply({'nameWithOwner': 'acme/widgets', 'defaultBranchRef': {'name': 'main'}})
if args[:2] == ['api', 'graphql']:
    lost = root / 'lose-readback'
    if lost.exists():
        lost.unlink()
        raise SystemExit(1)
    reply({
        'number': 8, 'title': 'fixture', 'state': 'CLOSED' if (root / 'closed').exists() else 'OPEN',
        'isDraft': False, 'headRefName': 'feature', 'headRefOid': 'a' * 40,
        'headRepository': {'name': 'widgets', 'nameWithOwner': 'acme/widgets'},
        'headRepositoryOwner': {'login': 'acme'}, 'isCrossRepository': False,
        'baseRefName': 'main', 'baseRefOid': 'b' * 40, 'labels': [], 'labelsTruncated': False,
        'autoMergeRequest': None, 'createdAt': '2026-01-01T00:00:00Z', 'mergedAt': None,
        'url': 'https://github.com/acme/widgets/pull/8', 'updatedAt': '2026-01-01T00:00:00Z',
    })
if args and args[0] == 'api':
    endpoint = args[1]
    if endpoint.endswith('/pulls/8') and '--method' in args:
        assert args[args.index('--method') + 1] == 'PATCH'
        assert args[args.index('--raw-field') + 1] == 'state=closed'
        (root / 'closed').write_text('closed')
        if os.environ.get('CLOSE_FIXTURE_LOST') == '1':
            (root / 'lose-readback').write_text('once')
            raise SystemExit(1)
        reply({})
    if '/git/ref/heads/' in endpoint:
        reply({'object': {'sha': ('a' if endpoint.endswith('/feature') else 'b') * 40}})
    if '/commits/' in endpoint:
        revision = endpoint.rsplit('/', 1)[1]
        reply({'sha': revision, 'commit': {'tree': {'sha': 'e' * 40}, 'committer': {'date': '2026-01-01T00:00:00Z'}}, 'parents': []})
    if '/compare/' in endpoint:
        reply({'status': 'ahead'})
    if '/stacks?' in endpoint:
        mode_file = root / 'native-mode'
        if mode_file.exists():
            mode = mode_file.read_text()
            reply([{'id': 42, 'number': 7, 'node_id': 'fixture-stack',
                    'base': {'ref': 'main'}, 'open': mode == 'open',
                    'created_at': '2026-01-01T00:00:00Z',
                    'pull_requests': [{'number': 8, 'state': 'open', 'draft': False,
                                       'head': {'ref': 'feature', 'sha': 'a' * 40}}]}])
        reply([])
raise SystemExit(99)
