"""Check release history preservation and build an actual Helm repository."""

import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import build_chart_repository as charts


def release(version, **extra):
    return {
        'draft': False, 'prerelease': False,
        'assets': [{'name': f'temba-catalog-{version}.tgz', 'url': f'https://api.github.com/assets/{version}'}],
        **extra,
    }


class ChartRepositoryTests(unittest.TestCase):
    def test_pagination_and_only_stable_chart_assets(self):
        pages = [[release('2.3.0'), release('2.4.0', draft=True)],
                 [release('2.2.0'), release('2.5.0', prerelease=True), release('../invalid')]]
        self.assertEqual(set(charts.chart_assets(pages)),
                         {'temba-catalog-2.2.0.tgz', 'temba-catalog-2.3.0.tgz'})

    def test_duplicate_and_empty_repositories_fail(self):
        for pages in [[], [[release('2.3.0'), release('2.3.0')]]]:
            with self.assertRaises(ValueError):
                charts.chart_assets(pages)

    def test_build_preserves_history_and_excludes_local_overrides(self):
        root = Path(__file__).resolve().parents[1]
        real_run = subprocess.run
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            chart = directory / 'temba-catalog'
            shutil.copytree(root / 'chart/temba-catalog', chart)
            shutil.copy2(root / 'LICENSE', chart / 'LICENSE')
            archives = {}
            for version in ['2.2.0', '2.3.0']:
                real_run(['helm', 'package', str(chart), '--version', version,
                          '--destination', str(directory)], check=True, capture_output=True)
                archive = directory / f'temba-catalog-{version}.tgz'
                archives[f'https://api.github.com/assets/{version}'] = archive.read_bytes()
                with tarfile.open(archive) as contents:
                    self.assertIn('temba-catalog/LICENSE', contents.getnames())
                    self.assertNotIn('temba-catalog/overide.yaml', contents.getnames())

            def run(command, **kwargs):
                if command[0] == 'gh':
                    kwargs['stdout'].write(archives[command[-1]])
                    return subprocess.CompletedProcess(command, 0)
                return real_run(command, **kwargs)

            pages = json.dumps([[release('2.3.0')], [release('2.2.0')]])
            with patch.object(charts.subprocess, 'check_output', return_value=pages), \
                 patch.object(charts.subprocess, 'run', side_effect=run):
                charts.build('owner/repo', 'https://example.com/charts/', directory / 'site')
            index = (directory / 'site/index.yaml').read_text()
            for version in ['2.2.0', '2.3.0']:
                self.assertIn(f'https://example.com/charts/temba-catalog-{version}.tgz', index)
                self.assertEqual((directory / f'site/temba-catalog-{version}.tgz').read_bytes(),
                                 archives[f'https://api.github.com/assets/{version}'])


if __name__ == '__main__':
    unittest.main()
