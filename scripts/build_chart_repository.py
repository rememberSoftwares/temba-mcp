#!/usr/bin/env python3
"""Rebuild the Pages Helm repository from immutable GitHub Release chart assets."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess


def chart_assets(pages):
    assets = {}
    for releases in pages:
        for release in releases:
            if release['draft'] or release['prerelease']:
                continue
            for asset in release['assets']:
                name = asset['name']
                if not re.fullmatch(r'temba-catalog-[0-9]+\.[0-9]+\.[0-9]+\.tgz', name):
                    continue
                if name in assets:
                    raise ValueError(f'Duplicate chart version in releases: {name}')
                assets[name] = asset['url']
    if not assets:
        raise ValueError('No released Helm charts found; refusing to publish an empty repository')
    return assets


def build(repository, url, output):
    if output.exists() and any(output.iterdir()):
        raise ValueError('Output directory must be empty')
    output.mkdir(parents=True, exist_ok=True)
    pages = json.loads(subprocess.check_output(
        ['gh', 'api', '--paginate', '--slurp', f'repos/{repository}/releases?per_page=100'],
        text=True,
    ))
    for name, asset_url in sorted(chart_assets(pages).items()):
        with (output / name).open('wb') as archive:
            subprocess.run(
                ['gh', 'api', '-H', 'Accept: application/octet-stream', asset_url],
                stdout=archive, check=True,
            )
    subprocess.run(['helm', 'repo', 'index', str(output), '--url', url.rstrip('/')], check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--url', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    build(os.environ['GH_REPO'], args.url, args.output)


if __name__ == '__main__':
    main()
