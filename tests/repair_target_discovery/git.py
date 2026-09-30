#!/usr/bin/env python3
"""Real isolated Git; route only fixture network reads to its private bare repo."""
import os
from pathlib import Path
import sys
import time

root = Path(os.environ['FIXTURE_ROOT'])
args = sys.argv[1:]
if args == ['remote', 'get-url', 'origin']:
    print('git@github.com:acme/widgets.git')
    sys.exit(0)
network = next((verb for verb in ('fetch', 'ls-remote', 'push', 'clone') if verb in args), None)
if network == 'push':
    sys.exit('PUBLIC_START_MUST_NOT_PUSH')
if network:
    if network == 'ls-remote' and os.environ['FIXTURE_SCENARIO'] == 'budget':
        marker = root / 'compatibility-delay-used'
        if not marker.exists():
            marker.touch()
            time.sleep(3)
    args = [str(root / 'remote.git') if arg in ('origin', 'git@github.com:acme/widgets.git') else arg for arg in args]
    # Never delegate an unexpected remote to real Git.
    if any(arg.startswith(('http:', 'https:', 'git@', 'ssh:')) for arg in args):
        sys.exit('NETWORK_TARGET_REFUSED')
os.execv(os.environ['FIXTURE_REAL_GIT'], [os.environ['FIXTURE_REAL_GIT'], *args])
